//! Minimal Kythe `Entry` proto decoder. We don't pull `protobuf` in:
//! the Entry/VName messages are exactly two depth levels of repeated
//! length-delimited fields, and a 150-line hand decoder beats the
//! codegen story on bench transparency and crate-graph size.
//!
//! ## Kythe wire format we consume
//!
//! ```proto
//! message VName {
//!   string signature = 1;
//!   string corpus    = 2;
//!   string root      = 3;
//!   string path      = 4;
//!   string language  = 5;
//! }
//! message Entry {
//!   VName  source     = 1;
//!   string edge_kind  = 2;
//!   VName  target     = 3;
//!   string fact_name  = 4;
//!   bytes  fact_value = 5;
//! }
//! ```
//!
//! Stream framing: each Entry is preceded by its proto-varint length —
//! the canonical Kythe format used by every v0.0.75 indexer's stdout.
//!
//! ## What we extract
//!
//! Anchor nodes carry three node-facts (`/kythe/node/kind = "anchor"`,
//! `/kythe/loc/start`, `/kythe/loc/end`) and one or more out-edges to
//! the symbol they reference. We stream-accumulate anchors and flush
//! a `(sym, role, file, offset)` row the moment we have start+edge.
//! `end` is captured but currently unused — scry2's xref row stores
//! offset only, matching scry's precision-packed shape.
//!
//! Edge kinds we honour:
//!   /kythe/edge/defines/binding  → role::DECL
//!   /kythe/edge/defines          → role::DEF
//!   /kythe/edge/ref              → role::REF
//!   /kythe/edge/ref/call         → role::CALL
//!   /kythe/edge/ref/writes       → role::REF
//!   /kythe/edge/ref/imports      → role::REF
//!   /kythe/edge/extends          → emitted to inherits[] table
//!   /kythe/edge/extends/private  → inherits[]
//!   /kythe/edge/extends/protected→ inherits[]
//!   /kythe/edge/overrides        → inherits[]
//!   /kythe/edge/completes        → DEFN→DECL bridge (scoped per stream)

use crate::format::{kind, lang, role, sym_of};
use crate::writer::IndexBuilder;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::io::Read;

#[derive(Default, Clone, Debug)]
pub struct VName {
    pub signature: String,
    pub corpus:    String,
    pub root:      String,
    pub path:      String,
    pub language:  String,
}

impl VName {
    /// Canonical scry symbol string: same shape as Kythe's own VName
    /// hashing. Sufficient to distinguish every node in any indexer's
    /// stream.
    pub fn to_symbol_string(&self) -> String {
        format!(
            "kythe:{}:{}#{}#{}#{}",
            self.language, self.corpus, self.root, self.path, self.signature,
        )
    }
    pub fn is_empty(&self) -> bool {
        self.signature.is_empty() && self.corpus.is_empty()
            && self.root.is_empty() && self.path.is_empty()
            && self.language.is_empty()
    }
    /// Map Kythe `language` field onto our compact lang byte.
    pub fn lang_byte(&self) -> u8 {
        match self.language.as_str() {
            "c++"    => lang::CXX,
            "java"   => lang::JAVA,
            "jvm"    => lang::JVM,
            "go"     => lang::GO,
            "proto"  => lang::PROTO,
            "rust"   => lang::RUST,
            "kotlin" => lang::KOTLIN,
            _        => lang::UNK,
        }
    }
}

#[derive(Default, Debug)]
pub struct Entry {
    pub source:     VName,
    pub edge_kind:  String,
    pub target:     VName,
    pub fact_name:  String,
    pub fact_value: Vec<u8>,
}

/// Drive an entries stream into `builder`. Streams the reader — peak
/// memory is one Entry buffer plus the anchor accumulator (size ≈
/// in-flight anchors per CU, normally < 10k).
///
/// `file_ids` is the caller's stable allocator for paths → file_id;
/// we share it across CUs so the same file in two CUs uses the same
/// id, which keeps the xref table compact and queries fast.
pub fn ingest<R: Read>(
    reader: R,
    builder: &mut IndexBuilder,
    file_ids: &FileIdAllocator,
) -> Result<IngestStats> {
    ingest_tolerant(reader, builder, file_ids, /*tolerate_trunc=*/ false)
}

/// Like [`ingest`], but if the stream is truncated mid-entry we log a
/// warning and return the partial stats instead of erroring. Used by
/// `from-kzip` where one indexer crashing shouldn't sink the whole
/// multi-language ingest.
pub fn ingest_tolerant<R: Read>(
    reader: R,
    builder: &mut IndexBuilder,
    file_ids: &FileIdAllocator,
    tolerate_trunc: bool,
) -> Result<IngestStats> {
    let mut r = std::io::BufReader::with_capacity(64 * 1024, reader);
    // Callgraph emission: we cannot rely on `/kythe/edge/childof` —
    // in cxx_indexer's output that edge connects sym scopes
    // (namespace nesting, class membership), not anchors to their
    // enclosing function. The Kythe-correct way is to use body
    // anchors: an anchor that carries a `/kythe/edge/defines` edge
    // (not `defines/binding`) spans the whole function definition,
    // start..end. Every call/ref anchor whose offset falls inside a
    // body anchor's range belongs to that function.
    //
    // We collect both at anchor-flush time, then post-stream do an
    // O(log n) lookup per call site against the sorted body table.
    //
    // Layout:
    //   body_anchors: (file_id, start, end, def_sym), sorted by (file_id, start)
    //   call_sites:   (file_id, start, target_sym, role)
    let mut state = IngestState::default();
    let mut stats = IngestStats::default();
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    loop {
        let len = match read_varint(&mut r) {
            Ok(Some(v)) => v as usize,
            Ok(None)    => break,
            Err(e) if tolerate_trunc => {
                eprintln!("[ingest] tolerated truncated varint after {} entries: {e}",
                    stats.entries);
                break;
            }
            Err(e) => return Err(e),
        };
        // Guard the allocation: a corrupt length prefix could demand a
        // multi-GB resize and OOM-kill the worker (aborting the whole
        // run). Real Kythe entries are at most a few MB even for huge
        // MarkedSource, so anything past this cap is a corrupted stream —
        // treat it like a truncation.
        const MAX_ENTRY_LEN: usize = 1 << 28; // 256 MiB
        if len > MAX_ENTRY_LEN {
            if tolerate_trunc {
                eprintln!("[ingest] oversize entry len {len} after {} entries; treating as truncation",
                    stats.entries);
                break;
            }
            bail!("entry length {len} exceeds sane maximum {MAX_ENTRY_LEN} (corrupt stream)");
        }
        buf.resize(len, 0);
        match r.read_exact(&mut buf) {
            Ok(()) => {}
            Err(e) if tolerate_trunc => {
                eprintln!("[ingest] tolerated truncated entry body after {} entries: {e}",
                    stats.entries);
                break;
            }
            Err(e) => return Err(e).context("truncated entry stream"),
        }
        let entry = match parse_entry(&buf) {
            Ok(e) => e,
            Err(e) if tolerate_trunc => {
                eprintln!("[ingest] tolerated malformed Entry #{}: {e}", stats.entries);
                break;
            }
            Err(e) => return Err(e.context(format!("decode Entry #{}", stats.entries))),
        };
        stats.entries += 1;
        process_entry(&entry, &mut state, builder, file_ids, &mut stats);
    }
    // -- Callgraph emission: resolve call_sites against body_anchors.
    //
    // Sort body anchors by (file_id, start). For each call_site
    // (file, off, target, role), binary-search the slice of body
    // anchors with that file_id for the smallest range that contains
    // `off`. The Kythe convention is non-overlapping body anchors
    // PER FUNCTION but functions can nest (lambdas, inner classes).
    // We pick the innermost (= shortest span) containing range so
    // an `inner_lambda → calls → foo` edge is attributed to the
    // lambda, not the outer function.
    state.body_anchors.sort_unstable_by_key(|(f, s, _, _)| (*f, *s));
    stats.diag_defines_seen = state.body_anchors.len() as u64;
    stats.diag_pending      = state.call_sites.len() as u64;
    for (file, off, target_sym, role_byte) in &state.call_sites {
        if let Some(enc) = innermost_containing(&state.body_anchors, *file, *off) {
            builder.add_call(enc, *target_sym, *role_byte);
            stats.calls_emitted += 1;
        } else {
            stats.diag_unresolved += 1;
        }
    }
    // Apply per-stream completes bridges: rewrite any sym that came in
    // as a definition-VName to its declaration-VName when an explicit
    // bridge exists. The IndexBuilder doesn't currently expose that
    // affordance; we defer it to a follow-up. The bridge count is
    // surfaced via stats so callers can log it.
    stats.completes_bridges = state.completes_bridges.len();

    // -- Resolved-type emission: render each typed edge's type node.
    //
    // The whole CU's type-node graph is now buffered, so a `tapp` and its
    // param/leaf nodes are all present regardless of stream order. Render
    // once per typed edge; store only what renders ("honest emptiness").
    {
        let g = CuTypeGraph { side: &state.types };
        for (src_key, type_key) in &state.types.edges {
            if let Some(rendered) = render_type(&g, type_key.as_str()) {
                builder.add_type(sym_of(src_key), &rendered);
                stats.types_emitted += 1;
            }
        }
    }
    Ok(stats)
}

#[derive(Default, Debug, Clone, Copy)]
pub struct IngestStats {
    pub entries:           u64,
    pub anchors_flushed:   u64,
    pub xrefs_emitted:     u64,
    pub inherits_emitted:  u64,
    pub aliases_emitted:   u64,
    pub calls_emitted:     u64,
    pub types_emitted:     u64,
    pub completes_bridges: usize,
    // Diagnostic: side-table sizes at resolution time. Useful for
    // spotting "0 calls emitted" failures — typically a sign that an
    // indexer used a different edge kind for childof than expected.
    pub diag_defines_seen: u64,
    pub diag_pending:      u64,
    pub diag_unresolved:   u64,
}

/// Maps file path → u32 id. The mapping is stable for the lifetime of
/// the allocator: two CUs that reference the same file get the same
/// id.
///
/// Interior mutability is intentional: `from-kzip` workers all share
/// a single `&FileIdAllocator` and call `intern` concurrently. The
/// mutex is held only for the O(hash) lookup, not for the whole CU
/// ingest — that's the difference between "real parallelism" and
/// the prior single-allocator-mutex shape where 36 workers all
/// parked in futex_wait_queue.
#[derive(Default)]
pub struct FileIdAllocator {
    // (path -> id, next id to assign). Held together under one lock so
    // id assignment is atomic and `seed_from` can advance the counter
    // past any pre-loaded ids. The counter is explicit (not `map.len()`)
    // so a seeded, possibly-non-dense namespace can't hand out an id
    // that collides with a seeded one.
    inner: std::sync::Mutex<(HashMap<String, u32>, u32)>,
}

impl FileIdAllocator {
    pub fn intern(&self, path: &str) -> u32 {
        let mut g = self.inner.lock().unwrap();
        if let Some(&id) = g.0.get(path) { return id; }
        let id = g.1;
        g.1 += 1;
        g.0.insert(path.to_string(), id);
        id
    }
    /// Resume support: pre-load (path, id) pairs from a prior base/shard
    /// so a resumed run continues the SAME file-id namespace. Without
    /// this a resumed run restarts ids at 0, which collide with the
    /// existing shards' ids when the final merge dedups the file tables
    /// by id — silently misattributing xrefs to the wrong file.
    pub fn seed_from(&self, ix: &crate::reader::Index) {
        let mut g = self.inner.lock().unwrap();
        for (id, path) in ix.iter_files() {
            g.0.entry(path.to_string()).or_insert(id);
            if id >= g.1 { g.1 = id + 1; }
        }
    }
    pub fn drain_into(self, builder: &mut IndexBuilder) {
        for (path, id) in self.inner.into_inner().unwrap().0 {
            builder.upsert_file(id, &path);
        }
    }
    /// Non-consuming variant for mid-run snapshots: copy the current
    /// (path, id) map into `builder` so each shard captures every
    /// file_id its xrefs might reference. `IndexBuilder::upsert_file`
    /// is first-write-wins, so this is safe to call repeatedly and to
    /// interleave with the final `drain_into`.
    pub fn push_to(&self, builder: &mut IndexBuilder) {
        let g = self.inner.lock().unwrap();
        for (path, &id) in g.0.iter() {
            builder.upsert_file(id, path);
        }
    }
}

/// In-flight ingestion state. Lives for one `ingest_tolerant` call.
///
/// Grouping these four fields into a struct keeps `process_entry`'s
/// signature short and makes "what mutable state does the decoder
/// own?" answerable in one place. Each field is documented at its
/// use site.
#[derive(Default)]
struct IngestState {
    /// Anchor VName-string → in-flight per-anchor accumulator.
    anchors: HashMap<String, AnchorAccum>,
    /// cxx DEFN-VName → DECL-VName (captured per stream — see
    /// completes-edge handling below).
    completes_bridges: HashMap<String, String>,
    /// `(file_id, start, end, def_sym)` — function-body anchors used
    /// to attribute call sites to their enclosing function.
    body_anchors: Vec<(u32, u32, u32, u64)>,
    /// `(file_id, start, target_sym, role)` — call/ref sites waiting
    /// to be resolved against body_anchors after the stream ends.
    call_sites: Vec<(u32, u32, u64, u8)>,
    /// Resolved-type side-table, drained at CU finalize.
    ///
    /// `/kythe/edge/typed` connects a sym node to a *type node*, which is
    /// often a `tapp` composite whose structure is in `param.N` edges and
    /// whose leaves carry `/kythe/code` MarkedSource. None of that is
    /// stream-ordered, so we buffer the whole type-node graph for the CU
    /// and render once at the end against [`render_type`].
    types: TypeSide,
}

/// Per-CU buffer of everything the type renderer needs. Keyed by the
/// node's VName symbol string (the same key `to_symbol_string` produces).
#[derive(Default)]
struct TypeSide {
    /// `/kythe/edge/typed`: sym-node key → type-node key.
    edges: Vec<(String, String)>,
    /// `/kythe/node/kind` of a type node ("tapp", "record", "tbuiltin", …).
    kind: HashMap<String, String>,
    /// `/kythe/code` MarkedSource bytes of a type node.
    code: HashMap<String, Vec<u8>>,
    /// `param.N` targets of a `tapp`, by ordinal. A BTree keeps them in
    /// ascending N so the head (param.0) and args (param.1..) come out
    /// ordered without a post-sort.
    params: HashMap<String, std::collections::BTreeMap<u32, String>>,
    /// The builtin operator (`ptr`, `const`, `fn`, `int`, …) of a
    /// `tbuiltin` node — the part of its signature before `#builtin`.
    builtin_op: HashMap<String, String>,
}

#[derive(Default)]
struct AnchorAccum {
    is_anchor: bool,
    path:      String,       // anchor's source file (from VName.path)
    start:     Option<u32>,
    end:       Option<u32>,
    pending:   Vec<(VName, u8)>,  // (target_vn, role)
    /// Sym this anchor binds via a `/kythe/edge/defines` edge. That
    /// edge marks the anchor as covering the WHOLE function body —
    /// distinct from `defines/binding` which is just the name. Body
    /// anchors are what we use to attribute call-site offsets to their
    /// enclosing function for callgraph emission.
    body_def_sym: Option<u64>,
}

fn process_entry(
    e: &Entry,
    state: &mut IngestState,
    builder: &mut IndexBuilder,
    file_ids: &FileIdAllocator,
    stats: &mut IngestStats,
) {
    // Local aliases so the body code stays terse; the compiler elides
    // these on release builds.
    let anchors           = &mut state.anchors;
    let completes_bridges = &mut state.completes_bridges;
    let body_anchors      = &mut state.body_anchors;
    let call_sites        = &mut state.call_sites;
    let types             = &mut state.types;
    if e.source.is_empty() { return; }
    let source_key = e.source.to_symbol_string();

    // Edge?
    if !e.edge_kind.is_empty() {
        // completes bridges (cxx DEFN↔DECL)
        if is_completes_edge(&e.edge_kind) && !e.target.is_empty() {
            completes_bridges.insert(source_key, e.target.to_symbol_string());
            return;
        }
        // `/kythe/edge/named` — source = real sym VName, target = name
        // VName whose signature is the human FQN. The Java indexer
        // appends a JVM method descriptor (e.g. `()J`, `(II)V`); we
        // store both the raw alias AND a descriptor-stripped form so
        // `def android.os.Binder.clearCallingIdentity` resolves
        // without the user knowing the descriptor.
        if is_named_edge(&e.edge_kind) && !e.target.signature.is_empty() {
            let sym = sym_of(&source_key);
            let raw = e.target.signature.as_ref();
            builder.add_alias(sym, raw);
            stats.aliases_emitted += 1;
            if let Some(stripped) = strip_jvm_method_descriptor(raw) {
                if stripped.len() != raw.len() {
                    builder.add_alias(sym, stripped);
                    stats.aliases_emitted += 1;
                }
            }
            return;
        }
        // inheritance edges → inh[] table
        if is_inherit_edge(&e.edge_kind) && !e.target.is_empty() {
            let child  = sym_of(&source_key);
            let parent = sym_of(&e.target.to_symbol_string());
            builder.add_inherit(child, parent);
            stats.inherits_emitted += 1;
            return;
        }
        // `/kythe/edge/typed` — sym node → type node. Buffer the edge;
        // the type node's facts/params may not have arrived yet, so we
        // render at CU finalize over the whole accumulated graph.
        if e.edge_kind == "/kythe/edge/typed" && !e.target.is_empty() {
            types.edges.push((source_key, e.target.to_symbol_string()));
            return;
        }
        // `/kythe/edge/param.N` — a `tapp`'s head (N=0) and args (N>=1).
        if let Some(ord) = parse_param_ordinal(&e.edge_kind) {
            if !e.target.is_empty() {
                types.params.entry(source_key)
                    .or_default()
                    .insert(ord, e.target.to_symbol_string());
            }
            return;
        }
        // /kythe/edge/childof in cxx_indexer connects sym scopes
        // (namespace / class nesting), NOT anchors to functions —
        // not useful for callgraph. We ignore it here. Function
        // containment is reconstructed from body anchors instead.

        // xref edges
        if let Some(role_byte) = edge_to_role(&e.edge_kind) {
            let target_sym = sym_of(&e.target.to_symbol_string());
            let a = anchors.entry(source_key.clone()).or_default();
            if a.path.is_empty() { a.path = e.source.path.clone(); }
            // `defines` (NOT defines/binding) anchors cover the whole
            // function body — that's the range we attribute call
            // sites against. Stash on the accumulator so flush_ready
            // can record it once start+end land.
            if role_byte == role::DEF {
                a.body_def_sym = Some(target_sym);
            }
            if let (true, Some(start)) = (a.is_anchor, a.start) {
                let file_id = file_ids.intern(&a.path);
                emit_xref_resolved(target_sym, &e.target, role_byte, file_id, start,
                                   builder, stats);
                if role_byte == role::REF || role_byte == role::CALL {
                    call_sites.push((file_id, start, target_sym, role_byte));
                }
                // If this is a body-defining anchor and end is known,
                // record the body range now.
                if role_byte == role::DEF {
                    if let Some(end) = a.end {
                        body_anchors.push((file_id, start, end, target_sym));
                    }
                }
            } else {
                a.pending.push((e.target.clone(), role_byte));
            }
        }
        return;
    }

    // Node fact.
    match e.fact_name.as_str() {
        // C++ has no `/kythe/edge/named` — the human-readable name is
        // encoded as a MarkedSource proto under `/kythe/code`. Parse
        // it and emit the rendered FQN as a sym alias so
        // `def android::Parcel::writeStrongBinder` resolves.
        "/kythe/code" => {
            if let Some(fqn) = parse_marked_source_fqn(&e.fact_value) {
                let sym = sym_of(&source_key);
                builder.add_alias(sym, &fqn);
                stats.aliases_emitted += 1;
            }
            // A type node's MarkedSource is the source of its rendered
            // name (`Widget`, `int`, `java.lang.String`). Buffer it for the
            // finalize-time type render. kind/code can arrive in either
            // order, so we keep the bytes for every node and let the
            // renderer read only the ones a typed edge actually reaches —
            // unreferenced entries are dropped when the CU's side-table is.
            types.code.entry(source_key.clone())
                .or_insert_with(|| e.fact_value.clone());
        }
        "/kythe/node/kind" => {
            let value = std::str::from_utf8(&e.fact_value).unwrap_or("");
            if value == "anchor" {
                let a = anchors.entry(source_key.clone()).or_default();
                a.is_anchor = true;
                if a.path.is_empty() { a.path = e.source.path.clone(); }
                flush_ready(a, body_anchors, call_sites, builder, file_ids, stats);
            } else {
                // Symbol node. Register name (= source_key for now —
                // FQN normalization via `named` edges is a follow-up)
                // + kind + lang.
                let k = node_kind_byte(value);
                let l = e.source.lang_byte();
                builder.upsert_sym(sym_of(&source_key), k, l, &source_key);
            }
            // Record the raw Kythe kind for the type renderer (it branches
            // on "tapp"/"tbuiltin"/"record"/… text, not our compact byte).
            // A builtin node also exposes its operator via its signature
            // (`ptr#builtin`, `const#builtin`, `int#builtin`).
            if is_type_node_kind(value) {
                types.kind.insert(source_key.clone(), value.to_string());
                if value == "tbuiltin" {
                    if let Some(op) = builtin_op_of(&e.source.signature) {
                        types.builtin_op.insert(source_key.clone(), op.to_string());
                    }
                }
            }
        }
        "/kythe/loc/start" => {
            if let Some(v) = parse_ascii_u32(&e.fact_value) {
                let a = anchors.entry(source_key.clone()).or_default();
                if a.path.is_empty() { a.path = e.source.path.clone(); }
                a.start = Some(v);
                flush_ready(a, body_anchors, call_sites, builder, file_ids, stats);
            }
        }
        "/kythe/loc/end" => {
            if let Some(v) = parse_ascii_u32(&e.fact_value) {
                let a = anchors.entry(source_key.clone()).or_default();
                a.end = Some(v);
                // If end is the field that completes a body anchor,
                // record the body range now.
                if let (true, Some(start), Some(sym)) = (a.is_anchor, a.start, a.body_def_sym) {
                    let file_id = file_ids.intern(&a.path);
                    body_anchors.push((file_id, start, v, sym));
                }
            }
        }
        _ => {}
    }
}

fn flush_ready(
    a: &mut AnchorAccum,
    body_anchors: &mut Vec<(u32, u32, u32, u64)>,
    call_sites:   &mut Vec<(u32, u32, u64, u8)>,
    builder: &mut IndexBuilder,
    file_ids: &FileIdAllocator,
    stats: &mut IngestStats,
) {
    if !a.is_anchor || a.start.is_none() { return; }
    let start = a.start.unwrap();
    let path  = a.path.clone();
    let file_id = file_ids.intern(&path);
    let pend  = std::mem::take(&mut a.pending);
    for (target, role_byte) in pend {
        let target_sym = sym_of(&target.to_symbol_string());
        emit_xref_resolved(target_sym, &target, role_byte, file_id, start, builder, stats);
        if role_byte == role::REF || role_byte == role::CALL {
            call_sites.push((file_id, start, target_sym, role_byte));
        }
        if role_byte == role::DEF {
            // Record body-anchor range if end is known.
            if let Some(end) = a.end {
                body_anchors.push((file_id, start, end, target_sym));
                a.body_def_sym = None; // consumed
            } else {
                a.body_def_sym = Some(target_sym);
            }
        }
    }
    stats.anchors_flushed += 1;
}

fn emit_xref_resolved(
    sym: u64,
    target: &VName,
    role_byte: u8,
    file_id: u32,
    offset: u32,
    builder: &mut IndexBuilder,
    stats: &mut IngestStats,
) {
    if target.is_empty() { return; }
    let sym_str = target.to_symbol_string();
    builder.upsert_sym(sym, kind::UNK, target.lang_byte(), &sym_str);
    builder.add_xref(sym, role_byte, file_id, offset);
    stats.xrefs_emitted += 1;
}

/// Find the SHORTEST body anchor that contains `(file, off)` — the
/// innermost enclosing function. O(log n) into the file's range, then
/// O(matching-bodies) linear within (which is tiny in practice — a
/// few nested lambdas at most).
fn innermost_containing(
    body_anchors: &[(u32, u32, u32, u64)],
    file: u32,
    off: u32,
) -> Option<u64> {
    // Binary-search the first row with file_id == `file` AND start >= 0.
    // We want all rows with file_id == file and start <= off.
    // body_anchors is sorted by (file_id, start) ascending.
    let n = body_anchors.len();
    let file_start = match body_anchors.binary_search_by_key(&(file, 0u32), |(f, s, _, _)| (*f, *s)) {
        Ok(i) | Err(i) => i,
    };
    let mut best: Option<(u32, u64)> = None;  // (span_len, sym)
    let mut i = file_start;
    while i < n && body_anchors[i].0 == file {
        let (_, start, end, sym) = body_anchors[i];
        if start > off { break; }                  // sorted; rest start later
        if end > off {
            let span = end - start;
            if best.is_none_or(|(b_span, _)| span < b_span) {
                best = Some((span, sym));
            }
        }
        i += 1;
    }
    best.map(|(_, sym)| sym)
}

fn edge_to_role(kind: &str) -> Option<u8> {
    let base = kind.split('.').next().unwrap_or(kind);
    match base {
        "/kythe/edge/defines/binding" => Some(role::DECL),
        "/kythe/edge/defines"         => Some(role::DEF),
        "/kythe/edge/ref"             => Some(role::REF),
        "/kythe/edge/ref/call"        => Some(role::CALL),
        "/kythe/edge/ref/writes"      => Some(role::REF),
        "/kythe/edge/ref/imports"     => Some(role::REF),
        _ => None,
    }
}

fn is_inherit_edge(kind: &str) -> bool {
    let base = kind.split('.').next().unwrap_or(kind);
    matches!(base,
        "/kythe/edge/extends"
        | "/kythe/edge/extends/private"
        | "/kythe/edge/extends/protected"
        | "/kythe/edge/extends/public"
        | "/kythe/edge/overrides"
        | "/kythe/edge/satisfies"
    )
}

fn is_completes_edge(kind: &str) -> bool {
    let base = kind.split('.').next().unwrap_or(kind);
    matches!(base, "/kythe/edge/completes" | "/kythe/edge/completes/uniquely")
}

fn is_named_edge(kind: &str) -> bool {
    let base = kind.split('.').next().unwrap_or(kind);
    base == "/kythe/edge/named"
}

/// Parse a Kythe MarkedSource proto (from `/kythe/code` facts emitted
/// by cxx_indexer) and render it to a flat C++ FQN like
/// `android::Parcel::writeStrongBinder`. Returns None if the proto is
/// malformed or yields no IDENTIFIER content. Parameter lists are
/// truncated at the first `(` so the caller can still query by FQN
/// without knowing the parameter types.
///
/// MarkedSource schema (kythe/proto/common.proto):
///   field 1: kind (varint enum) — BOX=0, IDENTIFIER=3, CONTEXT=4, ...
///   field 2: pre_text (string)
///   field 3: child (repeated submessage, recurse)
///   field 4: post_child_text (string — joiner between children)
///   field 5: post_text (string)
///   (other fields ignored)
/// MarkedSource kind values per kythe/proto/common.proto.
const MS_BOX:        u32 = 0;
const MS_TYPE:       u32 = 1;
const MS_IDENTIFIER: u32 = 3;
const MS_CONTEXT:    u32 = 4;

pub fn parse_marked_source_fqn(buf: &[u8]) -> Option<String> {
    // Field numbers per kythe.proto.common.MarkedSource (v0.0.75):
    //   1 kind, 2 pre_text, 3 child, 4 post_child_text, 5 post_text,
    //   10 add_final_list_token (bool — when true, `post_child_text`
    //      is also appended AFTER the last child). The latter is the
    //      load-bearing detail: cxx_indexer encodes a CONTEXT
    //      `android::Parcel` with joiner `::` and add_final_list=1,
    //      so the rendered text is `android::Parcel::`, with the
    //      trailing `::` becoming the separator before the sibling
    //      IDENTIFIER `writeAligned`. Ignoring field 10 collapsed the
    //      FQN to `android::Parcelwriteaisn` or, depending on
    //      surrounding context, `android::Parcel::writeAlignedval`
    //      (where `val` is a parameter name).
    fn render(buf: &[u8]) -> Option<(String, u32)> {
        let mut kind: u32 = MS_BOX;
        let mut pre = String::new();
        let mut joiner = String::new();
        let mut post = String::new();
        let mut add_final_list_token = false;
        let mut child_renders: Vec<String> = Vec::new();
        let mut pos = 0;
        while pos < buf.len() {
            let (field, wire, val_end, val_start) = read_proto_field(buf, pos)?;
            pos = val_end;
            match (field, wire) {
                (1, 0) => {
                    if let Some((v, _)) = read_varint_at(&buf[val_start..val_end], 0) {
                        kind = v as u32;
                    }
                }
                (2, 2) => pre = String::from_utf8_lossy(&buf[val_start..val_end]).into_owned(),
                (3, 2) => {
                    if let Some((s, _)) = render(&buf[val_start..val_end]) {
                        child_renders.push(s);
                    }
                }
                (4, 2) => joiner = String::from_utf8_lossy(&buf[val_start..val_end]).into_owned(),
                (5, 2) => post = String::from_utf8_lossy(&buf[val_start..val_end]).into_owned(),
                (10, 0) => {
                    if let Some((v, _)) = read_varint_at(&buf[val_start..val_end], 0) {
                        add_final_list_token = v != 0;
                    }
                }
                _      => {}
            }
        }
        let mut out = String::with_capacity(pre.len() + post.len());
        out.push_str(&pre);
        if !child_renders.is_empty() {
            out.push_str(&child_renders.join(&joiner));
            if add_final_list_token { out.push_str(&joiner); }
        }
        out.push_str(&post);
        if out.is_empty() { None } else { Some((out, kind)) }
    }
    let (full, _) = render(buf)?;
    // Truncate at the first `(` — the parameter list lives inside a
    // BOX child whose pre_text starts with `(`.
    let cut = full.find('(').unwrap_or(full.len());
    let trimmed = full[..cut].trim_end();
    // Method MarkedSources are prefixed with return-type + visibility
    // modifiers ("LIBBINDER_EXPORTED status_t android::Parcel::foo");
    // the FQN never contains whitespace, so the last whitespace-
    // separated token is the FQN.
    let last_token = trimmed.rsplit_once(char::is_whitespace)
        .map(|(_, fqn)| fqn).unwrap_or(trimmed);
    // Trim a trailing "::" left over from add_final_list_token when
    // the FQN was the very last text in the rendering.
    let fqn = last_token.trim_end_matches(':');
    if fqn.is_empty() { None } else { Some(fqn.to_string()) }
}

// Const docs — MS_TYPE / MS_IDENTIFIER / MS_CONTEXT are documentary
// references to the kind enum even though render() no longer reads
// them directly (the trailing-list-token fix removed the need for
// kind-based child filtering).
#[allow(dead_code)]
const _MS_KINDS_DOC: (u32, u32, u32) = (MS_TYPE, MS_IDENTIFIER, MS_CONTEXT);

// MS_TYPE is referenced by the inner kind-decode branch; the others
// are used directly by the rendering heuristic.
#[allow(dead_code)] const _MS_TYPE_USED_FOR_DOC: u32 = MS_TYPE;

// ============================================================
// Resolved-type renderer (backs the `typed` section)
// ============================================================
//
// A symbol's resolved type comes in over `/kythe/edge/typed`, pointing
// at a *type node*. Two shapes occur:
//
//   1. A named leaf — `record` / `interface` / `tbuiltin` / `tvar`
//      carrying a `/kythe/code` MarkedSource. We render that proto to a
//      clean type name (`Widget`, `int`, `java.lang.String`).
//
//   2. A `tapp` (type application) — a composite with no useful
//      MarkedSource for our purposes (C++ `Box<int>`, `const Box<int>&`,
//      `int*`, function types). Its structure is in `/kythe/edge/param.N`:
//      `param.0` is the *head*, `param.1..` the arguments. We render it
//      recursively. A builtin head (signature `ptr#builtin`, `const#builtin`,
//      `fn#builtin`, …) maps to C++ syntax; a record/interface head renders
//      as `Head<arg, …>`. Java `tapp` nodes (`List<String>`) ALSO carry a
//      MarkedSource but it uses parameter-lookup tokens that only make
//      sense with the param edges in hand, so we render those recursively
//      too — the param structure is the single source of truth.
//
// The renderer is generic over a [`TypeGraph`] so the ingest path drives
// it over the per-CU side-table while tests drive it over a hand-built
// graph mirroring the exact node shapes the indexers emit.

/// Read-only view of the type-node graph for one CU. A `Tk` is whatever
/// opaque ticket the caller keys nodes by (in ingest, the node's VName
/// symbol string; in tests, a small integer or `&str`).
pub trait TypeGraph {
    type Tk: Copy + Eq;
    /// Kythe `/kythe/node/kind` of `tk` ("record", "tapp", "tbuiltin", …),
    /// or "" if unknown.
    fn node_kind(&self, tk: Self::Tk) -> &str;
    /// Raw `/kythe/code` MarkedSource bytes for `tk`, if it has one.
    fn node_code(&self, tk: Self::Tk) -> Option<&[u8]>;
    /// The node's `param.N` targets in ascending N order (head first),
    /// or empty if it has none. Returned owned so the renderer can
    /// recurse without aliasing the graph's internal storage.
    fn params(&self, tk: Self::Tk) -> Vec<Self::Tk>;
    /// The builtin "operator" name for a `tbuiltin` head: the part of the
    /// node's signature before `#builtin` (`ptr`, `const`, `fn`, `int`,
    /// …). Returns "" for non-builtin nodes.
    fn builtin_op(&self, tk: Self::Tk) -> &str;
}

/// Render the type node `tk` to a display string, or None if it carries
/// neither a MarkedSource name nor a renderable `tapp` structure (honest
/// emptiness — a wrong type is worse than absent).
pub fn render_type<G: TypeGraph>(g: &G, tk: G::Tk) -> Option<String> {
    render_type_rec(g, tk, 0)
}

/// Recursion depth cap. Real C++ composites nest a handful deep
/// (`const Box<int>&` is 3); a cycle in a corrupt graph would otherwise
/// spin. 64 is far past any real type and bounds the stack.
const TYPE_RENDER_MAX_DEPTH: u32 = 64;

fn render_type_rec<G: TypeGraph>(g: &G, tk: G::Tk, depth: u32) -> Option<String> {
    if depth > TYPE_RENDER_MAX_DEPTH { return None; }
    let kind = g.node_kind(tk);
    if kind == "tapp" {
        // Composite: head = param.0, args = param.1.. . Recurse.
        let params = g.params(tk);
        if params.is_empty() {
            // No structure to recurse into — fall back to MarkedSource if
            // present (some Java tapps), else give up.
            return g.node_code(tk).and_then(typename_from_marked_source);
        }
        let head = params[0];
        let args: Vec<String> = params[1..].iter()
            .map(|&a| render_type_rec(g, a, depth + 1)
                .unwrap_or_else(|| "?".to_string()))
            .collect();
        if g.node_kind(head) == "tbuiltin" {
            return Some(render_builtin_app(g.builtin_op(head), &args));
        }
        // Non-builtin head (record/interface like `Box`, `List`) →
        // `Head<arg, …>`. Render the head as a leaf name.
        let head_name = render_type_rec(g, head, depth + 1)?;
        if args.is_empty() {
            return Some(head_name);
        }
        return Some(format!("{head_name}<{}>", args.join(", ")));
    }
    // Named leaf (record / interface / tbuiltin / tvar): its MarkedSource
    // carries the type name.
    if let Some(code) = g.node_code(tk) {
        if let Some(name) = typename_from_marked_source(code) {
            return Some(name);
        }
    }
    // A bare builtin leaf with no code but a known operator (e.g.
    // `int#builtin` reached as an arg) renders to the operator name.
    let op = g.builtin_op(tk);
    if !op.is_empty() { return Some(op.to_string()); }
    None
}

/// Render a builtin type-application head over its already-rendered args.
/// `op` is the builtin operator name (`ptr`, `lvr`, `const`, `fn`, …).
/// Unknown operators fall back to `op<args>` so a new builtin still
/// produces a legible (if generic) rendering rather than nothing.
fn render_builtin_app(op: &str, args: &[String]) -> String {
    let a0 = || args.first().map(String::as_str).unwrap_or("?");
    match op {
        "ptr"      => format!("{} *", a0()),
        "lvr"      => format!("{} &", a0()),
        "rvr"      => format!("{} &&", a0()),
        "const"    => format!("const {}", a0()),
        "volatile" => format!("volatile {}", a0()),
        // Function type: arg0 is the return type, the rest are parameters.
        "fn" => {
            let ret = a0();
            let rest = if args.len() > 1 { &args[1..] } else { &[][..] };
            format!("{ret}({})", rest.join(", "))
        }
        // A builtin with no args is a scalar (`int`, `void`, `char`).
        _ if args.is_empty() => op.to_string(),
        // Unknown composite builtin — keep it legible.
        _ => format!("{op}<{}>", args.join(", ")),
    }
}

/// Render a `/kythe/code` MarkedSource to a clean *type name* (as opposed
/// to [`parse_marked_source_fqn`], which is tuned for callable FQNs —
/// truncating parameter lists and dropping return types). For a type
/// node the FQN renderer already produces the right name in the common
/// cases (`Widget`, `int`, `java.lang.String`); the one artifact is a
/// trailing empty `<>` that a generic class/interface definition's
/// MarkedSource appends (`java.util.List<>`). Strip that — the concrete
/// arguments come from the enclosing `tapp`'s param edges, never from the
/// head's own decoration.
fn typename_from_marked_source(code: &[u8]) -> Option<String> {
    let name = parse_marked_source_fqn(code)?;
    let trimmed = name.trim_end_matches("<>").trim_end();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

/// Read one proto field header (varint tag + wire-size payload) at
/// `pos` from `buf`. Returns `(field, wire, val_end, val_start)` where
/// `val_start..val_end` is the payload slice. Returns None on EOF.
fn read_proto_field(buf: &[u8], mut pos: usize) -> Option<(u32, u8, usize, usize)> {
    let (tag, p1) = read_varint_at(buf, pos)?;
    pos = p1;
    let field = (tag >> 3) as u32;
    let wire  = (tag & 0x7) as u8;
    let (val_start, val_end) = match wire {
        0 => { // varint
            let (_, p2) = read_varint_at(buf, pos)?;
            (pos, p2)
        }
        1 => {                               // fixed64
            let end = pos.checked_add(8).filter(|&e| e <= buf.len())?;
            (pos, end)
        }
        2 => {                               // length-delim
            let (len, p2) = read_varint_at(buf, pos)?;
            let end = p2.checked_add(len as usize)?;
            if end > buf.len() { return None; }
            (p2, end)
        }
        5 => {                               // fixed32
            let end = pos.checked_add(4).filter(|&e| e <= buf.len())?;
            (pos, end)
        }
        _ => return None,
    };
    Some((field, wire, val_end, val_start))
}

fn read_varint_at(buf: &[u8], mut pos: usize) -> Option<(u64, usize)> {
    let mut val: u64 = 0;
    let mut shift = 0u32;
    for _ in 0..10 {
        if pos >= buf.len() { return None; }
        let b = buf[pos];
        pos += 1;
        val |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 { return Some((val, pos)); }
        shift += 7;
    }
    None
}

/// Strip the trailing JVM method descriptor from a Java named-edge
/// signature. The java_indexer emits aliases like
/// `android.os.Binder.clearCallingIdentity()J` — the `()J` is a JVM
/// method descriptor (no args → returns long). Users expect to query
/// by `android.os.Binder.clearCallingIdentity` (no descriptor) so we
/// store both forms.
///
/// Returns `Some(prefix)` when a descriptor is found, else `None`.
/// Non-method symbols (classes, fields) don't have a descriptor and
/// the function returns None — caller already stored the raw form.
fn strip_jvm_method_descriptor(sig: &str) -> Option<&str> {
    // Find the LAST `(` — descriptors are at the end of the
    // signature, never embedded.
    let open = sig.rfind('(')?;
    // Walk from `open` forward to find the matching `)`. JVM type
    // descriptors don't nest parens, so a simple search suffices.
    let bytes = sig.as_bytes();
    let close_rel = bytes[open..].iter().position(|&b| b == b')')?;
    let close = open + close_rel;
    // Everything between `(` and `)` must be valid JVM type
    // descriptor chars (or empty for no-arg methods).
    if !bytes[open + 1..close].iter().all(is_jvm_type_byte) { return None; }
    // Everything AFTER `)` must be a single return-type descriptor.
    let ret = &bytes[close + 1..];
    if ret.is_empty() || !ret.iter().all(is_jvm_type_byte) { return None; }
    Some(&sig[..open])
}

/// JVM type descriptor characters: B C D F I J S V Z (primitives),
/// L<class>; (reference), [ (array). Also '/' and '$' and identifier
/// chars for class names.
fn is_jvm_type_byte(b: &u8) -> bool {
    matches!(*b,
        b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'V' | b'Z'
        | b'L' | b';' | b'[' | b'/' | b'$' | b'_'
        | b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9')
}


fn node_kind_byte(s: &str) -> u8 {
    match s {
        "function"            => kind::FUNCTION,
        "record" | "interface"=> kind::TYPE,
        "variable"            => kind::VARIABLE,
        "constant" | "field"  => kind::FIELD,
        "package"             => kind::PACKAGE,
        _                     => kind::UNK,
    }
}

fn parse_ascii_u32(b: &[u8]) -> Option<u32> {
    std::str::from_utf8(b).ok()?.parse().ok()
}

/// Parse the ordinal N out of a `/kythe/edge/param.N` edge kind. Returns
/// None for any other edge (including a bare `/kythe/edge/param` with no
/// ordinal, which the indexers don't emit for type applications).
fn parse_param_ordinal(kind: &str) -> Option<u32> {
    kind.strip_prefix("/kythe/edge/param.")?.parse().ok()
}

/// True for the `/kythe/node/kind` values that name a *type* node — the
/// ones a `/kythe/edge/typed` target can be. Limits what the type
/// side-table retains.
fn is_type_node_kind(kind: &str) -> bool {
    matches!(kind,
        "tapp" | "tbuiltin" | "tvar" | "record" | "interface" | "sum" | "talias")
}

/// The builtin "operator" of a `tbuiltin` node, i.e. the part of its
/// signature before `#builtin`. `ptr#builtin` → `ptr`, `int#builtin` →
/// `int`. Returns None when the signature isn't a `#builtin` form.
fn builtin_op_of(signature: &str) -> Option<&str> {
    let op = signature.strip_suffix("#builtin")
        .or_else(|| signature.split("#builtin").next().filter(|p| *p != signature))?;
    if op.is_empty() { None } else { Some(op) }
}

/// [`TypeGraph`] over the per-CU side-table. Nodes are keyed by their
/// VName symbol string. The renderer walks only the nodes a typed edge
/// actually reaches, so an entry the side-table buffered but nothing
/// referenced is simply never visited.
struct CuTypeGraph<'a> {
    side: &'a TypeSide,
}

impl<'a> TypeGraph for CuTypeGraph<'a> {
    type Tk = &'a str;
    fn node_kind(&self, tk: &'a str) -> &str {
        self.side.kind.get(tk).map(String::as_str).unwrap_or("")
    }
    fn node_code(&self, tk: &'a str) -> Option<&[u8]> {
        self.side.code.get(tk).map(Vec::as_slice)
    }
    fn params(&self, tk: &'a str) -> Vec<&'a str> {
        match self.side.params.get(tk) {
            Some(pm) => pm.values().map(String::as_str).collect(),
            None => Vec::new(),
        }
    }
    fn builtin_op(&self, tk: &'a str) -> &str {
        self.side.builtin_op.get(tk).map(String::as_str).unwrap_or("")
    }
}

// -- proto wire decoder --------------------------------------------------

fn read_varint<R: Read>(r: &mut R) -> Result<Option<u64>> {
    let mut val: u64 = 0;
    let mut shift = 0u32;
    let mut byte = [0u8; 1];
    for i in 0..10 {
        match r.read(&mut byte)? {
            0 if i == 0 => return Ok(None),
            0 => bail!("truncated varint after {i} bytes"),
            _ => {}
        }
        val |= ((byte[0] & 0x7F) as u64) << shift;
        if byte[0] & 0x80 == 0 { return Ok(Some(val)); }
        shift += 7;
    }
    bail!("varint > 10 bytes")
}

fn read_varint_bytes(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut val: u64 = 0;
    let mut shift = 0u32;
    for i in 0..10 {
        if *pos >= buf.len() { bail!("truncated varint at byte {}", *pos); }
        let b = buf[*pos];
        *pos += 1;
        val |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 { return Ok(val); }
        shift += 7;
        let _ = i;
    }
    bail!("varint > 10 bytes")
}

/// Parse one Entry from a complete byte slice (length already consumed
/// by the streaming caller). Tag layout: tag = (field_num << 3) | wire_type.
fn parse_entry(buf: &[u8]) -> Result<Entry> {
    let mut e = Entry::default();
    let mut pos = 0;
    while pos < buf.len() {
        let tag = read_varint_bytes(buf, &mut pos)?;
        let field = tag >> 3;
        let wire  = tag & 0x7;
        if wire != 2 {
            bail!("Entry: unexpected wire type {} for field {}", wire, field);
        }
        let len = read_varint_bytes(buf, &mut pos)? as usize;
        // checked_add: `len` is an untrusted u64 from the stream, so
        // `pos + len` could wrap usize and pass a naive `> buf.len()`
        // check, then panic (or mis-slice) on the index below.
        let end = pos.checked_add(len)
            .filter(|&e| e <= buf.len())
            .with_context(|| format!("Entry: field {field} len {len} extends past buffer (pos {pos} buf {})", buf.len()))?;
        let slice = &buf[pos..end];
        pos += len;
        match field {
            1 => e.source     = parse_vname(slice)?,
            2 => e.edge_kind  = String::from_utf8_lossy(slice).into_owned(),
            3 => e.target     = parse_vname(slice)?,
            4 => e.fact_name  = String::from_utf8_lossy(slice).into_owned(),
            5 => e.fact_value = slice.to_vec(),
            _ => {} // forward-compat: unknown field, skip
        }
    }
    Ok(e)
}

fn parse_vname(buf: &[u8]) -> Result<VName> {
    let mut v = VName::default();
    let mut pos = 0;
    while pos < buf.len() {
        let tag = read_varint_bytes(buf, &mut pos)?;
        let field = tag >> 3;
        let wire  = tag & 0x7;
        if wire != 2 {
            bail!("VName: unexpected wire type {} for field {}", wire, field);
        }
        let len = read_varint_bytes(buf, &mut pos)? as usize;
        // checked_add: `len` is untrusted; avoid a usize wrap that would
        // defeat the bound check and panic on the slice below.
        let end = pos.checked_add(len)
            .filter(|&e| e <= buf.len())
            .with_context(|| format!("VName: field {field} len {len} extends past buffer"))?;
        let slice = &buf[pos..end];
        pos += len;
        match field {
            1 => v.signature = String::from_utf8_lossy(slice).into_owned(),
            2 => v.corpus    = String::from_utf8_lossy(slice).into_owned(),
            3 => v.root      = String::from_utf8_lossy(slice).into_owned(),
            4 => v.path      = String::from_utf8_lossy(slice).into_owned(),
            5 => v.language  = String::from_utf8_lossy(slice).into_owned(),
            _ => {}
        }
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};

    // ---- Real `/kythe/code` MarkedSource bytes ----
    //
    // Captured verbatim from the stock v0.0.75 indexers run on tiny probe
    // files (see the renderer tests below for the exact source). These are
    // the leaf type nodes' code facts; the recursive renderer turns them
    // into the rendered type names the indexer's own structure implies.

    // cxx_indexer: `struct Widget {}` record node.
    const WIDGET_CODE: &[u8] = &[0x08, 0x03, 0x12, 0x06, 0x57, 0x69, 0x64, 0x67, 0x65, 0x74];
    // cxx_indexer: `int#builtin` tbuiltin node.
    const INT_CODE: &[u8] = &[0x08, 0x03, 0x12, 0x03, 0x69, 0x6e, 0x74];
    // cxx_indexer: `template <typename T> struct Box` record node.
    const BOX_CODE: &[u8] = &[0x08, 0x03, 0x12, 0x03, 0x42, 0x6f, 0x78];
    // java_indexer: `java.lang.String` record node (typed target of `var s`).
    const STRING_CODE: &[u8] = &[
        0x1a, 0x27, 0x1a, 0x0a, 0x08, 0x0c, 0x12, 0x06, 0x70, 0x75, 0x62, 0x6c, 0x69, 0x63,
        0x1a, 0x09, 0x08, 0x0c, 0x12, 0x05, 0x66, 0x69, 0x6e, 0x61, 0x6c, 0x1a, 0x09, 0x08,
        0x0c, 0x12, 0x05, 0x63, 0x6c, 0x61, 0x73, 0x73, 0x22, 0x01, 0x20, 0x50, 0x01, 0x1a,
        0x1b, 0x08, 0x04, 0x1a, 0x08, 0x08, 0x03, 0x12, 0x04, 0x6a, 0x61, 0x76, 0x61, 0x1a,
        0x08, 0x08, 0x03, 0x12, 0x04, 0x6c, 0x61, 0x6e, 0x67, 0x22, 0x01, 0x2e, 0x50, 0x01,
        0x1a, 0x0a, 0x08, 0x03, 0x12, 0x06, 0x53, 0x74, 0x72, 0x69, 0x6e, 0x67];
    // java_indexer: `java.util.List` interface head of the `List<String>`
    // tapp (param.0). Its own MarkedSource carries a trailing empty `<>`
    // generic decoration that `typename_from_marked_source` strips.
    const LIST_HEAD_CODE: &[u8] = &[
        0x1a, 0x20, 0x1a, 0x0a, 0x08, 0x0c, 0x12, 0x06, 0x70, 0x75, 0x62, 0x6c, 0x69, 0x63,
        0x1a, 0x0d, 0x08, 0x0c, 0x12, 0x09, 0x69, 0x6e, 0x74, 0x65, 0x72, 0x66, 0x61, 0x63,
        0x65, 0x22, 0x01, 0x20, 0x50, 0x01, 0x1a, 0x1b, 0x08, 0x04, 0x1a, 0x08, 0x08, 0x03,
        0x12, 0x04, 0x6a, 0x61, 0x76, 0x61, 0x1a, 0x08, 0x08, 0x03, 0x12, 0x04, 0x75, 0x74,
        0x69, 0x6c, 0x22, 0x01, 0x2e, 0x50, 0x01, 0x1a, 0x08, 0x08, 0x03, 0x12, 0x04, 0x4c,
        0x69, 0x73, 0x74, 0x1a, 0x0c, 0x08, 0x0a, 0x12, 0x01, 0x3c, 0x22, 0x02, 0x2c, 0x20,
        0x2a, 0x01, 0x3e];

    /// In-memory [`TypeGraph`] for the renderer tests. Each node is keyed
    /// by a `&'static str` ticket and carries a kind, optional code-fact
    /// bytes, optional param edges, and an optional builtin operator —
    /// mirroring exactly the four facts the ingest side-table buffers.
    #[derive(Default)]
    struct MapGraph {
        kind:    HashMap<&'static str, &'static str>,
        code:    HashMap<&'static str, &'static [u8]>,
        params:  HashMap<&'static str, BTreeMap<u32, &'static str>>,
        builtin: HashMap<&'static str, &'static str>,
    }
    impl MapGraph {
        fn leaf(&mut self, tk: &'static str, kind: &'static str, code: &'static [u8]) {
            self.kind.insert(tk, kind);
            self.code.insert(tk, code);
        }
        fn builtin_leaf(&mut self, tk: &'static str, op: &'static str) {
            self.kind.insert(tk, "tbuiltin");
            self.builtin.insert(tk, op);
        }
        /// A `tapp` whose params are `[head, args...]` in that order.
        fn tapp(&mut self, tk: &'static str, params: &[&'static str]) {
            self.kind.insert(tk, "tapp");
            let mut pm = BTreeMap::new();
            for (i, p) in params.iter().enumerate() { pm.insert(i as u32, *p); }
            self.params.insert(tk, pm);
        }
    }
    impl TypeGraph for MapGraph {
        type Tk = &'static str;
        fn node_kind(&self, tk: &'static str) -> &str {
            self.kind.get(tk).copied().unwrap_or("")
        }
        fn node_code(&self, tk: &'static str) -> Option<&[u8]> {
            self.code.get(tk).copied()
        }
        fn params(&self, tk: &'static str) -> Vec<&'static str> {
            self.params.get(tk).map(|m| m.values().copied().collect()).unwrap_or_default()
        }
        fn builtin_op(&self, tk: &'static str) -> &str {
            self.builtin.get(tk).copied().unwrap_or("")
        }
    }

    // The renderer tests below assert the exact strings the indexers' node
    // graphs imply. The node SHAPES (kinds, param ordering, builtin head
    // signatures) and the leaf code-fact BYTES are taken from real
    // cxx_indexer / java_indexer output:
    //
    //   f.cc:
    //     auto w   = make_widget();   // Widget       (record leaf)
    //     auto k   = identity(42);    // int          (tbuiltin leaf)
    //     Box<int> bi;                // Box<int>     (tapp record-head)
    //     const Box<int>& cbi = bi;   // const Box<int> &   (nested tapp)
    //     int*  p;                    // int *        (tapp ptr#builtin)
    //     Widget* wp;                 // Widget *
    //   F.java:
    //     var s  = make();            // java.lang.String   (record leaf)
    //     var xs = list();            // java.util.List<java.lang.String>

    #[test]
    fn render_cxx_record_leaf_widget() {
        let mut g = MapGraph::default();
        g.leaf("Widget", "record", WIDGET_CODE);
        assert_eq!(render_type(&g, "Widget").as_deref(), Some("Widget"));
    }

    #[test]
    fn render_cxx_builtin_leaf_int() {
        // `identity(42)` deduces `int` — a `tbuiltin` leaf carrying the
        // `int` MarkedSource.
        let mut g = MapGraph::default();
        g.leaf("int", "tbuiltin", INT_CODE);
        assert_eq!(render_type(&g, "int").as_deref(), Some("int"));
    }

    #[test]
    fn render_cxx_tapp_box_of_int() {
        // tapp[Box(record, code), int(tbuiltin, code)] → "Box<int>".
        let mut g = MapGraph::default();
        g.leaf("Box", "record", BOX_CODE);
        g.leaf("int", "tbuiltin", INT_CODE);
        g.tapp("Box<int>", &["Box", "int"]);
        assert_eq!(render_type(&g, "Box<int>").as_deref(), Some("Box<int>"));
    }

    #[test]
    fn render_cxx_const_ref_box_of_int() {
        // The real chain for `const Box<int>&`:
        //   tapp[lvr#builtin, tapp[const#builtin, tapp[Box, int]]]
        let mut g = MapGraph::default();
        g.leaf("Box", "record", BOX_CODE);
        g.leaf("int", "tbuiltin", INT_CODE);
        g.builtin_leaf("lvr", "lvr");
        g.builtin_leaf("const", "const");
        g.tapp("Box<int>", &["Box", "int"]);
        g.tapp("const Box<int>", &["const", "Box<int>"]);
        g.tapp("const Box<int> &", &["lvr", "const Box<int>"]);
        let r = render_type(&g, "const Box<int> &").unwrap();
        assert!(r.contains("const Box<int>"), "got {r:?}");
        assert!(r.contains('&'), "got {r:?}");
        assert_eq!(r, "const Box<int> &");
    }

    #[test]
    fn render_cxx_pointer_to_int() {
        // tapp[ptr#builtin, int] → "int *".
        let mut g = MapGraph::default();
        g.leaf("int", "tbuiltin", INT_CODE);
        g.builtin_leaf("ptr", "ptr");
        g.tapp("int*", &["ptr", "int"]);
        assert_eq!(render_type(&g, "int*").as_deref(), Some("int *"));
    }

    #[test]
    fn render_cxx_pointer_to_record() {
        // tapp[ptr#builtin, Widget] → "Widget *".
        let mut g = MapGraph::default();
        g.leaf("Widget", "record", WIDGET_CODE);
        g.builtin_leaf("ptr", "ptr");
        g.tapp("Widget*", &["ptr", "Widget"]);
        assert_eq!(render_type(&g, "Widget*").as_deref(), Some("Widget *"));
    }

    #[test]
    fn render_cxx_function_type() {
        // tapp[fn#builtin, int(ret), int, int] → "int(int, int)". The head
        // node and the tapp node are distinct nodes (distinct tickets).
        let mut g = MapGraph::default();
        g.leaf("int", "tbuiltin", INT_CODE);
        g.builtin_leaf("fn#builtin", "fn");
        g.tapp("fn_tapp", &["fn#builtin", "int", "int", "int"]);
        assert_eq!(render_type(&g, "fn_tapp").as_deref(), Some("int(int, int)"));
    }

    #[test]
    fn render_java_record_leaf_string() {
        // `var s = make()` → record leaf rendering to "java.lang.String"
        // (the assertion accepts either "String" or the FQN form).
        let mut g = MapGraph::default();
        g.leaf("String", "record", STRING_CODE);
        let r = render_type(&g, "String").unwrap();
        assert!(r == "String" || r == "java.lang.String", "got {r:?}");
        assert_eq!(r, "java.lang.String");
    }

    #[test]
    fn render_java_tapp_list_of_string() {
        // `var xs = list()` → tapp[List(interface,code), String(record,code)].
        // The List head's own MarkedSource carries a trailing `<>` that
        // `typename_from_marked_source` strips; the concrete arg comes from
        // param.1.
        let mut g = MapGraph::default();
        g.leaf("List", "interface", LIST_HEAD_CODE);
        g.leaf("String", "record", STRING_CODE);
        g.tapp("List<String>", &["List", "String"]);
        let r = render_type(&g, "List<String>").unwrap();
        assert!(r.contains("List"), "got {r:?}");
        assert!(r.contains("String"), "got {r:?}");
        assert_eq!(r, "java.util.List<java.lang.String>");
    }

    #[test]
    fn render_honest_emptiness_on_unknown_node() {
        // A node with no kind, no code, no params, no builtin op — the
        // renderer returns None rather than inventing a type.
        let g = MapGraph::default();
        assert_eq!(render_type(&g, "ghost"), None);
        // A tapp whose head can't render and which has no args also yields
        // None (head_name is required).
        let mut g2 = MapGraph::default();
        g2.tapp("bad", &["ghost"]);
        assert_eq!(render_type(&g2, "bad"), None);
    }

    #[test]
    fn render_type_cycle_is_bounded() {
        // A corrupt graph with a self-referential tapp must not spin: the
        // depth cap returns (a possibly-degraded) result, never hangs.
        let mut g = MapGraph::default();
        g.builtin_leaf("ptr", "ptr");
        // tapp "loop" = ptr<loop> — references itself as its only arg.
        let mut pm = BTreeMap::new();
        pm.insert(0u32, "ptr");
        pm.insert(1u32, "loop");
        g.kind.insert("loop", "tapp");
        g.params.insert("loop", pm);
        // Should terminate (depth cap) — we only assert it returns.
        let _ = render_type(&g, "loop");
    }

    #[test]
    fn builtin_op_of_extracts_operator() {
        assert_eq!(builtin_op_of("ptr#builtin"), Some("ptr"));
        assert_eq!(builtin_op_of("const#builtin"), Some("const"));
        assert_eq!(builtin_op_of("int#builtin"), Some("int"));
        // Composite builtin signature `fn#builtin(void#builtin)` — the
        // operator is still `fn`.
        assert_eq!(builtin_op_of("fn#builtin(void#builtin)"), Some("fn"));
        // Non-builtin signatures yield None.
        assert_eq!(builtin_op_of("Widget#c#bgf6LUGjSuL"), None);
        assert_eq!(builtin_op_of("#builtin"), None);
    }

    #[test]
    fn parse_param_ordinal_reads_index() {
        assert_eq!(parse_param_ordinal("/kythe/edge/param.0"), Some(0));
        assert_eq!(parse_param_ordinal("/kythe/edge/param.7"), Some(7));
        assert_eq!(parse_param_ordinal("/kythe/edge/param"), None);
        assert_eq!(parse_param_ordinal("/kythe/edge/typed"), None);
    }

    #[test]
    fn strip_descriptor_method_with_primitive_return() {
        assert_eq!(
            strip_jvm_method_descriptor("android.os.Binder.clearCallingIdentity()J"),
            Some("android.os.Binder.clearCallingIdentity"),
        );
    }

    #[test]
    fn strip_descriptor_method_with_reference_return() {
        assert_eq!(
            strip_jvm_method_descriptor("java.lang.String.toString()Ljava/lang/String;"),
            Some("java.lang.String.toString"),
        );
    }

    #[test]
    fn strip_descriptor_method_with_args() {
        assert_eq!(
            strip_jvm_method_descriptor("foo.Bar.baz(II)V"),
            Some("foo.Bar.baz"),
        );
        assert_eq!(
            strip_jvm_method_descriptor("foo.Bar.baz(Ljava/lang/String;I)Z"),
            Some("foo.Bar.baz"),
        );
    }

    #[test]
    fn strip_descriptor_array_return() {
        assert_eq!(
            strip_jvm_method_descriptor("foo.Bar.bytes()[B"),
            Some("foo.Bar.bytes"),
        );
    }

    #[test]
    fn parse_marked_source_renders_cxx_fqn() {
        // Build a MarkedSource proto for `android::Parcel::writeStrongBinder(args)`.
        // Layout: outer BOX with children [CONTEXT, IDENTIFIER, BOX(params)].
        // CONTEXT has children [ID "android", ID "Parcel"] joined by "::"
        // and a post_text of "::".
        fn varint(mut v: u64, out: &mut Vec<u8>) {
            while v >= 0x80 { out.push(((v & 0x7F) | 0x80) as u8); v >>= 7; }
            out.push(v as u8);
        }
        fn submsg(field: u64, body: &[u8], out: &mut Vec<u8>) {
            out.push(((field << 3) | 2) as u8);
            varint(body.len() as u64, out);
            out.extend_from_slice(body);
        }
        fn strfield(field: u64, s: &str, out: &mut Vec<u8>) {
            out.push(((field << 3) | 2) as u8);
            varint(s.len() as u64, out);
            out.extend_from_slice(s.as_bytes());
        }
        fn id(name: &str) -> Vec<u8> {
            let mut buf = Vec::new();
            // field 1 (kind) = IDENTIFIER = 3, wire 0 (varint)
            buf.push(0x08); buf.push(3);
            strfield(2, name, &mut buf);  // pre_text
            buf
        }
        let android = id("android");
        let parcel  = id("Parcel");
        let writer  = id("writeStrongBinder");
        let param_box = {
            let mut b = Vec::new();
            // BOX kind=0 (default, omittable); pre_text="(", post_text=")"
            strfield(2, "(", &mut b);
            strfield(5, ")", &mut b);
            b
        };

        let mut context = Vec::new();
        // field 1 kind = CONTEXT = 4
        context.push(0x08); context.push(4);
        submsg(3, &android, &mut context);
        submsg(3, &parcel,  &mut context);
        strfield(4, "::",  &mut context);  // post_child_text joins children
        strfield(5, "::",  &mut context);  // post_text after the context

        let mut root = Vec::new();
        // field 1 kind = BOX = 0 (optional)
        submsg(3, &context,   &mut root);
        submsg(3, &writer,    &mut root);
        submsg(3, &param_box, &mut root);

        let fqn = parse_marked_source_fqn(&root).expect("renders");
        assert_eq!(fqn, "android::Parcel::writeStrongBinder",
            "params truncated at first '(', context joined by '::'");
    }

    #[test]
    fn parse_marked_source_returns_none_on_empty() {
        assert_eq!(parse_marked_source_fqn(&[]), None);
    }

    #[test]
    fn parse_marked_source_aosp_method_real_bytes() {
        // Exact /kythe/code fact_value bytes captured from cxx_indexer
        // running on the AOSP Parcel.cpp CU. This is `LIBBINDER_EXPORTED
        // status_t android::Parcel::writeAligned(...)`. Structure:
        //   outer (BOX, pre="LIBBINDER_EXPORTED " was absent here, padding
        //   spaces instead): [TYPE("status_t"), BOX("            "),
        //   BOX(CONTEXT("android::Parcel", add_final_list_token=1) +
        //       IDENT("writeAligned")),
        //   BOX(kind=6, pre="(", post_child=", ", post=")")]
        let bytes: &[u8] = &[
            0x1a, 0x0c, 0x08, 0x01, 0x12, 0x08, b's', b't', b'a', b't', b'u',
            b's', b'_', b't',                                             // TYPE "status_t"
            0x1a, 0x0e, 0x12, 0x0c, b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b' ', b' ', b' ',                           // BOX(spaces)
            0x1a, 0x35,                                                    // BOX(53) — the FQN
              0x1a, 0x21, 0x08, 0x04,                                       // CONTEXT
                0x1a, 0x0b, 0x08, 0x03, 0x12, 0x07, b'a', b'n', b'd', b'r',
                b'o', b'i', b'd',
                0x1a, 0x0a, 0x08, 0x03, 0x12, 0x06, b'P', b'a', b'r', b'c',
                b'e', b'l',
                0x22, 0x02, b':', b':',                                     // post_child_text "::"
                0x50, 0x01,                                                 // add_final_list_token = 1
              0x1a, 0x10, 0x08, 0x03, 0x12, 0x0c, b'w', b'r', b'i', b't',
              b'e', b'A', b'l', b'i', b'g', b'n', b'e', b'd',
            0x1a, 0x0c, 0x08, 0x06, 0x12, 0x01, b'(', 0x22, 0x02, b',',
            b' ', 0x2a, 0x01, b')',                                        // PARAM BOX
        ];
        let fqn = parse_marked_source_fqn(bytes).expect("renders");
        assert_eq!(fqn, "android::Parcel::writeAligned",
            "method FQN: return-type + padding stripped, params truncated");
    }

    #[test]
    fn parse_marked_source_aosp_parameter_real_bytes() {
        // Exact bytes for the parameter `val` MarkedSource emitted by
        // cxx_indexer alongside the writeAligned method. Structure:
        //   outer: [TYPE("T"), BOX(" "),
        //          BOX(CONTEXT("android::Parcel::writeAligned",
        //               add_final_list_token=1) + IDENT("val"))]
        // Note that the CONTEXT here includes the FUNCTION simple name
        // as a third IDENTIFIER, and the trailing `::` is what
        // separates it from "val". The FQN for this parameter sym is
        // "android::Parcel::writeAligned::val".
        let bytes: &[u8] = &[
            0x1a, 0x05, 0x08, 0x01, 0x12, 0x01, b'T',                      // TYPE "T"
            0x1a, 0x03, 0x12, 0x01, b' ',                                  // BOX(" ")
            0x1a, 0x3e,                                                    // BOX(62)
              0x1a, 0x33, 0x08, 0x04,                                       // CONTEXT
                0x1a, 0x0b, 0x08, 0x03, 0x12, 0x07, b'a', b'n', b'd', b'r',
                b'o', b'i', b'd',
                0x1a, 0x0a, 0x08, 0x03, 0x12, 0x06, b'P', b'a', b'r', b'c',
                b'e', b'l',
                0x1a, 0x10, 0x08, 0x03, 0x12, 0x0c, b'w', b'r', b'i', b't',
                b'e', b'A', b'l', b'i', b'g', b'n', b'e', b'd',
                0x22, 0x02, b':', b':',                                     // post_child_text "::"
                0x50, 0x01,                                                 // add_final_list_token = 1
              0x1a, 0x07, 0x08, 0x03, 0x12, 0x03, b'v', b'a', b'l',         // IDENT "val"
        ];
        let fqn = parse_marked_source_fqn(bytes).expect("renders");
        assert_eq!(fqn, "android::Parcel::writeAligned::val",
            "parameter FQN: <enclosing-function>::<param-name>");
    }

    #[test]
    fn parse_marked_source_strips_return_type_and_modifiers() {
        // Real-world cxx_indexer shape for `LIBBINDER_EXPORTED status_t
        // android::Parcel::writeStrongBinder(...)`. Top-level pre_text
        // carries the modifier "LIBBINDER_EXPORTED "; a TYPE child
        // carries the return type "status_t"; the next non-type child
        // is the actual name BOX. After truncating at `(` and taking
        // the last whitespace-separated token we want the FQN clean.
        fn varint(mut v: u64, out: &mut Vec<u8>) {
            while v >= 0x80 { out.push(((v & 0x7F) | 0x80) as u8); v >>= 7; }
            out.push(v as u8);
        }
        fn lendelim(field: u64, body: &[u8], out: &mut Vec<u8>) {
            out.push(((field << 3) | 2) as u8);
            varint(body.len() as u64, out);
            out.extend_from_slice(body);
        }
        fn str_field(field: u64, s: &str, out: &mut Vec<u8>) {
            out.push(((field << 3) | 2) as u8);
            varint(s.len() as u64, out);
            out.extend_from_slice(s.as_bytes());
        }
        fn id(name: &str) -> Vec<u8> {
            let mut buf = Vec::new();
            buf.push(0x08); buf.push(3);            // kind=IDENTIFIER
            str_field(2, name, &mut buf);            // pre_text
            buf
        }

        // Inner FQN BOX: CONTEXT(android, Parcel) post_text="::" + IDENTIFIER("foo")
        let mut context = Vec::new();
        context.push(0x08); context.push(4);         // kind=CONTEXT
        lendelim(3, &id("android"), &mut context);
        lendelim(3, &id("Parcel"),  &mut context);
        str_field(4, "::", &mut context);            // joiner
        str_field(5, "::", &mut context);            // post_text
        let mut name_box = Vec::new();
        lendelim(3, &context, &mut name_box);
        lendelim(3, &id("foo"), &mut name_box);

        // TYPE child: "status_t"
        let mut type_child = Vec::new();
        type_child.push(0x08); type_child.push(1);   // kind=TYPE
        str_field(2, "status_t", &mut type_child);

        // Param BOX: pre_text="(", post_text=")"
        let mut param_box = Vec::new();
        str_field(2, "(", &mut param_box);
        str_field(5, ")", &mut param_box);

        // Outer MarkedSource: pre_text="LIBBINDER_EXPORTED " + TYPE +
        // " " + name_box + param_box.
        let mut root = Vec::new();
        str_field(2, "LIBBINDER_EXPORTED ", &mut root);
        lendelim(3, &type_child, &mut root);
        // A " " separator child (a BOX with pre_text=" ").
        let mut space = Vec::new();
        str_field(2, " ", &mut space);
        lendelim(3, &space, &mut root);
        lendelim(3, &name_box,  &mut root);
        lendelim(3, &param_box, &mut root);

        let fqn = parse_marked_source_fqn(&root).expect("renders");
        assert_eq!(fqn, "android::Parcel::foo",
            "modifier + return-type prefix stripped, params truncated");
    }

    #[test]
    fn strip_descriptor_no_descriptor_is_none() {
        // A class or field has no method descriptor — return None
        // so the caller doesn't add a duplicate alias.
        assert_eq!(strip_jvm_method_descriptor("android.os.Binder"), None);
        assert_eq!(strip_jvm_method_descriptor("foo.Bar.someField"), None);
        // C++-style names with `::` don't have descriptors.
        assert_eq!(strip_jvm_method_descriptor("android::Parcel::writeStrongBinder"), None);
        // Truncated / malformed — don't strip.
        assert_eq!(strip_jvm_method_descriptor("foo.bar()"), None);  // no return type
        assert_eq!(strip_jvm_method_descriptor("foo.bar(II"), None);  // no `)`
        // Looks like a method-call paren in some non-Kythe text — only
        // strip if the suffix LOOKS like a real descriptor.
        assert_eq!(strip_jvm_method_descriptor("foo (something) bar"), None);
    }

    /// Build a wire-format Entry by hand and confirm we decode it.
    /// One anchor at path=Binder.java offset 12345 binds clearCallingIdentity.
    #[test]
    fn decode_handcrafted_anchor() {
        // Each varint here is < 128 so it's a single byte.
        // tag = (field<<3) | 2 for length-delim
        fn lendelim(field: u64, bytes: &[u8], out: &mut Vec<u8>) {
            out.push(((field << 3) | 2) as u8);
            write_varint(bytes.len() as u64, out);
            out.extend_from_slice(bytes);
        }
        fn write_varint(mut v: u64, out: &mut Vec<u8>) {
            while v >= 0x80 { out.push(((v & 0x7F) | 0x80) as u8); v >>= 7; }
            out.push(v as u8);
        }

        // build VName for source (anchor) and target (function)
        let mut source_v = Vec::new();
        lendelim(1, b"#a", &mut source_v);                   // signature
        lendelim(2, b"android",                  &mut source_v);
        lendelim(4, b"core/java/android/os/Binder.java", &mut source_v);
        lendelim(5, b"java",                     &mut source_v);
        let mut target_v = Vec::new();
        lendelim(1, b"clearCallingIdentity()",   &mut target_v);
        lendelim(2, b"android",                  &mut target_v);
        lendelim(4, b"core/java/android/os/Binder.java", &mut target_v);
        lendelim(5, b"java",                     &mut target_v);

        let mk_entry = |source: &[u8], edge: &[u8], target: &[u8], fact_name: &[u8], fact_value: &[u8]| -> Vec<u8> {
            let mut e = Vec::new();
            lendelim(1, source, &mut e);
            if !edge.is_empty() { lendelim(2, edge, &mut e); }
            if !target.is_empty() { lendelim(3, target, &mut e); }
            if !fact_name.is_empty() { lendelim(4, fact_name, &mut e); }
            if !fact_value.is_empty() { lendelim(5, fact_value, &mut e); }
            e
        };

        let kind_entry  = mk_entry(&source_v, b"", b"", b"/kythe/node/kind",  b"anchor");
        let start_entry = mk_entry(&source_v, b"", b"", b"/kythe/loc/start",  b"12345");
        let end_entry   = mk_entry(&source_v, b"", b"", b"/kythe/loc/end",    b"12365");
        let edge_entry  = mk_entry(&source_v, b"/kythe/edge/defines/binding", &target_v, b"/", b"");

        let mut stream = Vec::new();
        for entry in [kind_entry, start_entry, end_entry, edge_entry] {
            write_varint(entry.len() as u64, &mut stream);
            stream.extend_from_slice(&entry);
        }

        let mut builder = IndexBuilder::new();
        let fids = FileIdAllocator::default();
        let stats = ingest(&stream[..], &mut builder, &fids).unwrap();
        assert_eq!(stats.entries, 4);
        assert_eq!(stats.anchors_flushed, 1);
        assert_eq!(stats.xrefs_emitted, 1);
        assert_eq!(builder.n_xrefs(), 1);
    }
}
