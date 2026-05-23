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
    // -- Completes bridge: def-VName sym → decl-VName sym, for THIS CU.
    //
    // A `/kythe/edge/completes` edge links a C++ definition node (the
    // `.cpp`) to the declaration it completes (the `.h`). Without applying
    // it the definition's rows live under a separate, unmerged sym, so
    // `def <method FQN>` (which resolves via the declaration's FQN alias)
    // finds only the `.h` declaration location, not the `.cpp` body. We
    // rewrite every buffered sym-keyed row from the def sym to the decl
    // sym so the two unify on the queryable decl sym.
    //
    // Per-CU is correct: a definition VName is unique to the CU that
    // contains it, so this remap touches only this CU's rows and is O(rows
    // in this CU). The bridge is built here (not while streaming) because
    // a `completes` edge can arrive after the rows it should rewrite; by
    // finalize the whole CU — and thus every bridge — is known.
    let bridge: HashMap<u64, u64> = state.completes_bridges.iter()
        .map(|(def_vn, decl_vn)| (sym_of(def_vn), sym_of(decl_vn)))
        .collect();
    // Count APPLIED remaps (a sym that actually had a bridge entry), not
    // the number of bridge edges — the latter says nothing about whether
    // any row used them.
    let mut applied: u64 = 0;
    let mut remap = |s: u64| -> u64 {
        match bridge.get(&s) {
            Some(&d) => { applied += 1; d }
            None => s,
        }
    };

    // -- Buffered sym-keyed rows → builder, with the bridge applied.
    for (sym, role, file, offset) in &state.xrefs {
        builder.add_xref(remap(*sym), *role, *file, *offset);
    }
    // -- Aliases → name index, AND promote each sym's DISPLAY name.
    //
    // `add_alias` feeds the discovery index (FQN → sym lookup); that is
    // unchanged. Separately, a sym's display name was seeded to its raw
    // Kythe ticket at `upsert_sym` time. Now that the whole CU's aliases
    // are known, pick the cleanest human FQN among a sym's aliases and set
    // it as the display name so `members`/`super`/`def` render a readable
    // FQN instead of `kythe:...#<hash>`. The chosen alias is keyed on the
    // SAME remapped sym the alias/name index uses, so `def FQN` resolves
    // to a sym whose own metadata also shows that FQN. `set_sym_name`'s
    // `prefers_name` makes the pick order-independent (an FQN beats the
    // ticket, shortest/lex-smallest among FQNs), so it does not matter
    // that aliases arrive in stream order. A sym with no alias keeps its
    // ticket.
    let mut best_name: HashMap<u64, &str> = HashMap::new();
    for (sym, alias) in &state.aliases {
        let rs = remap(*sym);
        builder.add_alias(rs, alias);
        best_name.entry(rs)
            .and_modify(|cur| {
                if crate::writer::prefers_name(cur, alias) { *cur = alias.as_str(); }
            })
            .or_insert(alias.as_str());
    }
    for (sym, name) in &best_name {
        builder.set_sym_name(*sym, name);
    }
    for (child, parent) in &state.inherits {
        // Both endpoints can be a bridged def VName.
        builder.add_inherit(remap(*child), remap(*parent));
    }
    // -- Supertype name rendering. A generic supertype (`Comparable<T>`,
    //    `AbstractMap<K,V>`) is a `tapp` type node whose own `/kythe/code`
    //    MarkedSource renders to a degenerate `<>` — its head is a LOOKUP
    //    token absent from the bare proto. Render these through
    //    `render_type`, which walks the buffered `param.N` graph and so
    //    recovers the head + args, then promote the supertype sym's display
    //    name to it. `set_sym_name` applies the order-independent
    //    `prefers_name` rule, so a real `named`-edge FQN already on the sym
    //    still wins and a degenerate render can never overwrite a good name.
    {
        let g = CuTypeGraph { side: &state.types };
        for key in &state.inherit_parent_keys {
            // Only type nodes render; a non-type parent (rare) is left to
            // its alias/ticket name.
            if let Some(rendered) = render_type(&g, key.as_str()) {
                let rs = remap(sym_of(key));
                builder.set_sym_name(rs, &rendered);
            }
        }
    }
    for (child, parent) in &state.childof {
        builder.add_childof(remap(*child), remap(*parent));
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
            // Bridge BOTH endpoints: the enclosing caller may be the def
            // sym, and the callee may be a def sym too.
            builder.add_call(remap(enc), remap(*target_sym), *role_byte);
            stats.calls_emitted += 1;
        } else {
            stats.diag_unresolved += 1;
        }
    }

    // -- Resolved-type emission: render each typed edge's type node.
    //
    // The whole CU's type-node graph is now buffered, so a `tapp` and its
    // param/leaf nodes are all present regardless of stream order. Render
    // once per typed edge; store only what renders ("honest emptiness").
    {
        let g = CuTypeGraph { side: &state.types };
        for (src_key, type_key) in &state.types.edges {
            if let Some(rendered) = render_type(&g, type_key.as_str()) {
                builder.add_type(remap(sym_of(src_key)), &rendered);
                stats.types_emitted += 1;
            }
        }
    }

    // -- Signature emission: render each FUNCTION's full signature with
    //    parameter names. `typed` already gives the type-only function
    //    type; `sig` adds the param NAMES (e.g. `setEnabled(bool enabled)`),
    //    which carry semantic meaning. Built from the function's `param.N`
    //    edges (param syms, ordinal order), each param's own NAME (its
    //    MarkedSource short name) and TYPE (its typed edge, rendered via
    //    the same renderer the typed work uses), plus the function's
    //    return type (its own typed edge → `fn` render, return part).
    //    Honest emptiness: no param info / no renderable types → no row.
    {
        let g = CuTypeGraph { side: &state.types };
        // sym-key → type-node-key index over the buffered typed edges.
        let typed_of: HashMap<&str, &str> = state.types.edges.iter()
            .map(|(s, t)| (s.as_str(), t.as_str())).collect();
        for fn_key in &state.types.functions {
            if let Some(sig) = render_signature(&g, fn_key, &state.types, &typed_of) {
                builder.add_sig(remap(sym_of(fn_key)), &sig);
                stats.sigs_emitted += 1;
            }
        }
    }
    // Report APPLIED remaps so callers can log how many def→decl rewrites
    // actually landed (0 when a CU has completes edges but none of its
    // buffered rows referenced a bridged def sym).
    stats.completes_bridges = applied as usize;
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
    pub childof_emitted:   u64,
    pub sigs_emitted:      u64,
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
/// Allocator state behind the mutex: the (path → (id, accessed)) table plus
/// the next id to hand out.
///
/// `accessed` is true once a path is `intern`ed (actually referenced by an
/// anchor during ingest), false for a path only PRE-LOADED via
/// `seed_paths`/`seed_from`. `push_to`/`drain_into` emit only accessed
/// paths, so a deterministic pre-seed (which loads every candidate path in
/// the plan, most of which a given build never references) doesn't bloat the
/// output's `files` table with phantom unreferenced paths — the table stays
/// exactly the set of paths the ingest touched, while ids are the
/// deterministic seed ranks. The next-id counter is explicit (not
/// `map.len()`) so a seeded, possibly-non-dense namespace can't hand out an
/// id that collides with a seeded one.
type AllocState = (HashMap<String, (u32, bool)>, u32);

#[derive(Default)]
pub struct FileIdAllocator {
    // Held together under one lock so id assignment is atomic and
    // `seed_from` can advance the counter past any pre-loaded ids.
    inner: std::sync::Mutex<AllocState>,
}

impl FileIdAllocator {
    pub fn intern(&self, path: &str) -> u32 {
        let mut g = self.inner.lock().unwrap();
        if let Some(e) = g.0.get_mut(path) {
            // Mark a pre-seeded path as actually referenced so it lands in
            // the output's `files` table (a seed-only path stays out).
            e.1 = true;
            return e.0;
        }
        let id = g.1;
        g.1 += 1;
        g.0.insert(path.to_string(), (id, true));
        id
    }
    /// Deterministic pre-seed: assign every path in `sorted_paths` a dense
    /// id in iteration order (0, 1, 2, …), so a path's file-id is its rank
    /// in that order rather than the parallel-intern order it happens to be
    /// first touched in. The caller passes a deterministically-ordered,
    /// deduplicated path list (the sorted union of every planned CU's
    /// candidate file paths), so the resulting (path → id) map is a pure
    /// function of the plan — identical across a full build and a
    /// cache-assisted rebuild of the same kzip, and identical regardless of
    /// worker scheduling.
    ///
    /// This is what makes the merged `.s2db` byte-identical: every `intern`
    /// during ingest then resolves to a pre-assigned id, so xref `file`
    /// columns and the `files` table carry order-independent values. The
    /// seed must be a SUPERSET of the paths that ingest will actually
    /// intern; any interned path absent from the seed falls back to a fresh
    /// counter id whose value depends on intern order (non-deterministic),
    /// so callers build the seed from the same VName/info path fields the
    /// indexer reads from. Call once, before any ingest, on a fresh
    /// allocator. Idempotent for an already-seeded path (first id wins).
    pub fn seed_paths<'a, I: IntoIterator<Item = &'a str>>(&self, sorted_paths: I) {
        let mut g = self.inner.lock().unwrap();
        for path in sorted_paths {
            if !g.0.contains_key(path) {
                let id = g.1;
                g.1 += 1;
                // Pre-seeded, not yet referenced: emitted into the output's
                // `files` table only once `intern` marks it accessed.
                g.0.insert(path.to_string(), (id, false));
            }
        }
    }
    /// Resume support: pre-load (path, id) pairs from a prior base/shard
    /// so a resumed run continues the SAME file-id namespace. Without
    /// this a resumed run restarts ids at 0, which collide with the
    /// existing shards' ids when the final merge dedups the file tables
    /// by id — silently misattributing xrefs to the wrong file. Pre-loaded
    /// paths are marked not-accessed: the prior shard already carries them
    /// in its own file table, so they only re-enter this run's delta if it
    /// re-interns them — avoiding a redundant phantom row.
    pub fn seed_from(&self, ix: &crate::reader::Index) {
        let mut g = self.inner.lock().unwrap();
        for (id, path) in ix.iter_files() {
            g.0.entry(path.to_string()).or_insert((id, false));
            if id >= g.1 { g.1 = id + 1; }
        }
    }
    pub fn drain_into(self, builder: &mut IndexBuilder) {
        for (path, (id, accessed)) in self.inner.into_inner().unwrap().0 {
            if accessed { builder.upsert_file(id, &path); }
        }
    }
    /// Non-consuming variant for mid-run snapshots: copy the current
    /// (path, id) map into `builder` so each shard captures every
    /// file_id its xrefs might reference. `IndexBuilder::upsert_file`
    /// is first-write-wins, so this is safe to call repeatedly and to
    /// interleave with the final `drain_into`. Emits only accessed
    /// (actually-interned) paths — a pre-seeded but unreferenced path is
    /// not a fact this shard needs to carry.
    pub fn push_to(&self, builder: &mut IndexBuilder) {
        let g = self.inner.lock().unwrap();
        for (path, &(id, accessed)) in g.0.iter() {
            if accessed { builder.upsert_file(id, path); }
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
    /// Sym-keyed rows buffered for the WHOLE CU so the per-CU completes
    /// bridge (def-VName → decl-VName) can remap them before they reach
    /// the `IndexBuilder`. A `/kythe/edge/completes` edge can arrive after
    /// the rows it should rewrite (the indexer emits the .cpp definition's
    /// xrefs, then the bridge to the .h declaration), so streaming these
    /// straight into the builder would miss the bridge. Buffering defers
    /// every sym-keyed emission to the finalize step, where the bridge is
    /// complete. `upsert_sym` (sym metadata) is NOT buffered: it is not
    /// remapped — the def node keeps its own metadata row; only its
    /// references unify onto the decl sym.
    ///
    /// `(sym, role, file, offset)` — resolved xref rows.
    xrefs: Vec<(u64, u8, u32, u32)>,
    /// `(sym, alias)` — FQN/name aliases for a sym.
    aliases: Vec<(u64, String)>,
    /// `(child, parent)` — inheritance edges.
    inherits: Vec<(u64, u64)>,
    /// VName string keys of inheritance PARENTS (supertypes). A generic
    /// supertype is a `tapp` node whose own `/kythe/code` MarkedSource
    /// renders to a degenerate `<>` (its head is a LOOKUP token, not in the
    /// bare proto), so it would carry `<>` as its display name. At CU
    /// finalize we re-render these keys through `render_type` — which uses
    /// the buffered `param.N` graph and so recovers the head (`AbstractMap`)
    /// and args (`<K, V>`) — and `set_sym_name` to the readable form.
    inherit_parent_keys: std::collections::HashSet<String>,
    /// `(child, parent)` — childof membership edges (stored child-first
    /// here; `builder.add_childof` reverses to (parent, child)).
    childof: Vec<(u64, u64)>,
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
    /// Keys of FUNCTION nodes seen this CU, in first-seen order. At CU
    /// finalize each one is rendered into a full signature (`sig`
    /// section) from its `param.N` edges + each param's name/type + the
    /// function's own return type. Recorded here because the renderer
    /// needs the whole CU's graph buffered (param/type nodes arrive in
    /// any stream order). The param/function NAME MarkedSource and the
    /// param/return TYPE both come from the already-buffered `code` /
    /// `edges` tables — no extra storage needed.
    functions: Vec<String>,
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
    let xrefs             = &mut state.xrefs;
    let aliases           = &mut state.aliases;
    let inherits          = &mut state.inherits;
    let inherit_parent_keys = &mut state.inherit_parent_keys;
    let childof           = &mut state.childof;
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
            let raw = e.target.signature.as_str();
            aliases.push((sym, raw.to_string()));
            stats.aliases_emitted += 1;
            if let Some(stripped) = strip_jvm_method_descriptor(raw) {
                if stripped.len() != raw.len() {
                    aliases.push((sym, stripped.to_string()));
                    stats.aliases_emitted += 1;
                }
            }
            return;
        }
        // inheritance edges → inh[] table
        if is_inherit_edge(&e.edge_kind) && !e.target.is_empty() {
            let parent_key = e.target.to_symbol_string();
            let child  = sym_of(&source_key);
            let parent = sym_of(&parent_key);
            inherits.push((child, parent));
            // Remember the parent's string key so finalize can render a
            // readable name for a generic (`tapp`) supertype.
            inherit_parent_keys.insert(parent_key);
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
        // `/kythe/edge/childof` — a child node points at its enclosing
        // parent (a field/method childof its class, a class childof its
        // file/package, a param childof its function). We record every
        // one as a membership edge; `members NAME` filters by the parent
        // sym's kind at query time so only a real container (type /
        // record / interface / package) lists members and function-local
        // children never leak. NOT used for callgraph — call containment
        // is reconstructed from body anchors above, since childof in
        // cxx_indexer is sym-scope nesting, not anchor→function.
        if is_childof_edge(&e.edge_kind) && !e.target.is_empty() {
            let child  = sym_of(&source_key);
            let parent = sym_of(&e.target.to_symbol_string());
            childof.push((child, parent));
            stats.childof_emitted += 1;
            return;
        }

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
                // Record this CU's (file_id → path) into the per-CU builder
                // so its shard carries exactly the files it touched. The
                // per-CU cache path writes that builder as a standalone
                // shard and needs a self-contained file table; the legacy
                // path merges it into the sink (idempotent — `upsert_file`
                // is first-wins). Done at intern time so even an anchor that
                // ultimately emits no xref still contributes its file.
                builder.upsert_file(file_id, &a.path);
                emit_xref_resolved(&e.target, role_byte, file_id, start,
                                   xrefs, builder, stats);
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
            // Skip a DEGENERATE render (`<>`, empty, all-punctuation). It
            // carries no human identity — a `tapp` supertype/operator node
            // whose MarkedSource head is a LOOKUP token absent from the bare
            // proto renders to a bare `<>`. Emitting it as an alias both
            // pollutes the name index (54k `<>` rows) and can win a sym's
            // display name. The readable name for these nodes comes from the
            // `param.N` graph via `render_type` at CU finalize instead.
            if let Some(fqn) = parse_marked_source_fqn(&e.fact_value)
                .filter(|s| !is_degenerate_render(s))
            {
                let sym = sym_of(&source_key);
                aliases.push((sym, fqn));
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
                flush_ready(a, body_anchors, call_sites, xrefs, builder, file_ids, stats);
            } else {
                // Symbol node. Seed name = source_key (the raw Kythe
                // ticket) + kind + lang. CU finalize promotes the name to
                // the cleanest human FQN among the sym's `named`-edge /
                // MarkedSource aliases via `builder.set_sym_name`; a sym
                // with no alias keeps the ticket.
                let k = node_kind_byte(value);
                let l = e.source.lang_byte();
                builder.upsert_sym(sym_of(&source_key), k, l, &source_key);
                // Remember function nodes so CU finalize can render their
                // full signature (with param names) into the `sig` section.
                if k == kind::FUNCTION {
                    types.functions.push(source_key.clone());
                }
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
        // `/kythe/subkind` refines a node's kind. java_indexer emits class
        // fields as node kind `variable` with subkind `field`; without this
        // they'd be left VARIABLE and the writer's variable-kind alias
        // suppression (meant for C++ parameters/locals) would strip their
        // FQN. Promote `field`/`constant` subkinds to FIELD so the field's
        // `/kythe/edge/named` alias survives and `members` renders it as a
        // field. Fact order is irrelevant: `mark_field` overwrites
        // VARIABLE/UNK, and the kind=variable upsert never downgrades FIELD.
        "/kythe/subkind" => {
            let value = std::str::from_utf8(&e.fact_value).unwrap_or("");
            if value == "field" || value == "constant" {
                builder.mark_field(sym_of(&source_key));
            }
        }
        "/kythe/loc/start" => {
            if let Some(v) = parse_ascii_u32(&e.fact_value) {
                let a = anchors.entry(source_key.clone()).or_default();
                if a.path.is_empty() { a.path = e.source.path.clone(); }
                a.start = Some(v);
                flush_ready(a, body_anchors, call_sites, xrefs, builder, file_ids, stats);
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
                    builder.upsert_file(file_id, &a.path);
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
    xrefs:        &mut Vec<(u64, u8, u32, u32)>,
    builder: &mut IndexBuilder,
    file_ids: &FileIdAllocator,
    stats: &mut IngestStats,
) {
    if !a.is_anchor || a.start.is_none() { return; }
    let start = a.start.unwrap();
    let path  = a.path.clone();
    let file_id = file_ids.intern(&path);
    // Record the file even if `pend` is empty (an anchor with no resolved
    // targets still names a file the CU touched), so the per-CU shard's
    // file table is exactly the set of paths this CU interned.
    builder.upsert_file(file_id, &path);
    let pend  = std::mem::take(&mut a.pending);
    for (target, role_byte) in pend {
        let target_sym = sym_of(&target.to_symbol_string());
        emit_xref_resolved(&target, role_byte, file_id, start, xrefs, builder, stats);
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
    target: &VName,
    role_byte: u8,
    file_id: u32,
    offset: u32,
    xrefs: &mut Vec<(u64, u8, u32, u32)>,
    builder: &mut IndexBuilder,
    stats: &mut IngestStats,
) {
    if target.is_empty() { return; }
    let sym_str = target.to_symbol_string();
    let sym = sym_of(&sym_str);
    // Sym metadata streams straight in (not bridged): the def node keeps
    // its own name/kind row. The xref row is buffered so the per-CU
    // completes bridge can remap its sym to the decl at finalize.
    builder.upsert_sym(sym, kind::UNK, target.lang_byte(), &sym_str);
    xrefs.push((sym, role_byte, file_id, offset));
    stats.xrefs_emitted += 1;
}

/// Find the innermost body anchor containing `(file, off)` — the
/// enclosing function/lambda with the smallest span. O(log n + nesting):
/// one binary search to the upper bound, then a backward scan that stops
/// at the first container.
///
/// `body_anchors` is sorted by (file_id, start) ascending and Kythe body
/// anchors are properly nested, so among the containers of a point the
/// one with the LARGEST start is the most deeply nested == smallest span.
/// Scanning backwards from the last anchor with (file_id == file &&
/// start <= off), the first row that also satisfies `end > off` is that
/// innermost container — return it immediately. Stop the scan once we
/// leave the file's range (file_id != file or past file_start). This
/// returns the same sym as a forward smallest-span scan for properly
/// nested anchors.
fn innermost_containing(
    body_anchors: &[(u32, u32, u32, u64)],
    file: u32,
    off: u32,
) -> Option<u64> {
    // Upper bound: index just past the last row with (file_id, start)
    // <= (file, off). Everything in [file_start, upper) has file_id ==
    // file && start <= off.
    let upper = body_anchors.partition_point(|(f, s, _, _)| (*f, *s) <= (file, off));
    // Lower bound for this file: first row with file_id == file.
    let file_start = body_anchors.partition_point(|(f, _, _, _)| *f < file);
    let mut i = upper;
    while i > file_start {
        i -= 1;
        let (f, start, end, sym) = body_anchors[i];
        debug_assert!(f == file && start <= off);
        if end > off {
            return Some(sym); // largest start among containers == innermost
        }
    }
    None
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

fn is_childof_edge(kind: &str) -> bool {
    let base = kind.split('.').next().unwrap_or(kind);
    base == "/kythe/edge/childof"
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
        if g.node_kind(head) == "tbuiltin" {
            let op = g.builtin_op(head);
            // Array builtins (`carr`/`array`) render their single element as
            // `E[]`. The bound is NOT in the node — `int data[8]` is `int[]`.
            // Nesting composes naturally via the recursive element render:
            // `carr(carr(int))` → `int[][]`, `carr(ptr(int))` → `int *[]`.
            if is_array_op(op) {
                let elem = params.get(1)
                    .and_then(|&e| render_type_rec(g, e, depth + 1))
                    .unwrap_or_else(|| "?".to_string());
                return Some(format!("{elem}[]"));
            }
            // Pointer/reference TO an array needs the parenthesized
            // declarator: `int (*)[]`, not `int[] *`. This keeps
            // pointer-to-array distinct from array-of-pointer (`int *[]`).
            // Detect it STRUCTURALLY: the sole arg is an array-typed `tapp`.
            // We render the array's ELEMENT (one level in), not the array
            // itself, then wrap the declarator symbol in parens before `[]`.
            if let Some(decl) = ptr_ref_decl_sym(op) {
                if let Some(&arg) = params.get(1) {
                    if let Some(elem) = array_element_render(g, arg, depth) {
                        return Some(format!("{elem} ({decl})[]"));
                    }
                }
            }
        }
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
            // A type variable displays as its short name: java_indexer
            // renders the qualified form (`T.compute.K`), cxx already emits
            // the bare `T`. Short-name both so `K` reads like the source.
            if kind == "tvar" {
                return Some(short_name(&name).to_string());
            }
            return Some(name);
        }
    }
    // A bare builtin leaf with no code but a known operator (e.g.
    // `int#builtin` reached as an arg) renders to the operator name.
    let op = g.builtin_op(tk);
    if !op.is_empty() { return Some(op.to_string()); }
    None
}

/// True for the array builtin operators: C++ `carr` (constant-bound array)
/// and Java `array`. Both render `E[]` over their single element.
fn is_array_op(op: &str) -> bool {
    matches!(op, "carr" | "array")
}

/// The C++ declarator symbol for a pointer/reference builtin operator, used
/// when it points/refers to an array (`int (*)[]`, `int (&)[]`,
/// `int (&&)[]`). Returns None for non-pointer/reference operators. Java's
/// `array` has no pointer cases, so this is C++-only by construction.
fn ptr_ref_decl_sym(op: &str) -> Option<&'static str> {
    match op {
        "ptr" => Some("*"),
        "lvr" => Some("&"),
        "rvr" => Some("&&"),
        _ => None,
    }
}

/// If `tk` is an array-typed `tapp` (its head builtin op is `carr`/`array`),
/// render its ELEMENT type (one level inside the array) and return it; else
/// None. Used by the pointer/reference-to-array declarator: from
/// `ptr(carr(int))` we want the element `int`, not the array render `int[]`,
/// so the caller can produce `int (*)[]`. The recursion budget passed in is
/// the array node's own depth; the element renders at `depth + 1`.
fn array_element_render<G: TypeGraph>(g: &G, tk: G::Tk, depth: u32) -> Option<String> {
    if depth > TYPE_RENDER_MAX_DEPTH { return None; }
    if g.node_kind(tk) != "tapp" { return None; }
    let params = g.params(tk);
    let head = *params.first()?;
    if g.node_kind(head) != "tbuiltin" || !is_array_op(g.builtin_op(head)) {
        return None;
    }
    let elem = *params.get(1)?;
    render_type_rec(g, elem, depth + 1)
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

/// True for a DEGENERATE MarkedSource render — one with no human identity,
/// so it must never become a sym's display name or a name-index alias. A
/// `tapp` (parameterized type) or operator node whose head is a LOOKUP
/// token (absent from the bare proto) renders to a bare `<>` / `[]` / `?` /
/// other pure decoration: no alphanumeric character, no identity. The
/// readable name for such a node is recovered from its `param.N` graph via
/// [`render_type`], not from this flat render. The same backstop lives in
/// the writer's `prefers_name`; guarding at emission keeps the degenerate
/// string out of the name index entirely.
fn is_degenerate_render(s: &str) -> bool {
    let t = s.trim();
    t.is_empty() || !t.chars().any(|c| c.is_alphanumeric())
}

/// The short (unqualified) name from a parsed FQN: the last `::`- or
/// `.`-separated component. `Gadget::frob::w` → `w`,
/// `android.os.Binder.clearCallingIdentity` → `clearCallingIdentity`.
fn short_name(fqn: &str) -> &str {
    let after_colon = fqn.rsplit("::").next().unwrap_or(fqn);
    after_colon.rsplit('.').next().unwrap_or(after_colon)
}

/// Strip a trailing array declarator from a parameter NAME. cxx_indexer's
/// MarkedSource for an array parameter encodes the declarator on the name
/// itself (`fill::xs[4]` → short name `xs[4]`), so the name would otherwise
/// double the array-ness the rendered TYPE already carries (`int[] xs[4]`).
/// The bare identifier is everything before the first `[`. A non-array name
/// has no `[` and passes through unchanged.
fn bare_param_name(name: &str) -> &str {
    match name.find('[') {
        Some(i) => name[..i].trim_end(),
        None => name,
    }
}

/// Render a FUNCTION's full signature WITH parameter names — the value
/// the `sig` section adds over `typed`'s type-only function type.
///
/// Shape: `<ret_type> <fn_name>(<p0type> <p0name>, <p1type> <p1name>, …)`.
///
/// Sources, all from the CU's buffered graph:
///   * params: the function's `/kythe/edge/param.N` targets in ordinal
///     order (the parameter syms).
///   * each param's NAME: its `/kythe/code` MarkedSource short name.
///   * each param's TYPE: its `/kythe/edge/typed` node, rendered via the
///     same renderer the typed work uses.
///   * the return type: the function's own `/kythe/edge/typed` node — a
///     `fn` application rendering `<ret>(args…)`; we take the `<ret>`
///     part (before the first `(`).
///   * the function name: the function's own `/kythe/code` MarkedSource
///     short name.
///
/// Honest emptiness: returns None when the function has no param edges
/// (so there's nothing the type-only `typed` doesn't already carry), or
/// when nothing renders. A param whose type doesn't render still keeps
/// its name (rendered as just the name), since the name is the point.
fn render_signature<'a>(
    g: &CuTypeGraph<'a>,
    fn_key: &'a str,
    side: &'a TypeSide,
    typed_of: &HashMap<&'a str, &'a str>,
) -> Option<String> {
    // Function short name from its own MarkedSource (may be absent).
    let fn_name = side.code.get(fn_key)
        .and_then(|c| parse_marked_source_fqn(c))
        .map(|fqn| short_name(&fqn).to_string());

    // Return type: render the function's own typed node, take the part
    // before the first `(` (the `fn` render is `<ret>(args…)`). For a
    // member function the member-fn `this` lands in the fn type's args,
    // never in the return part, so this stays correct.
    let ret_type = typed_of.get(fn_key)
        .and_then(|tk| render_type(g, tk))
        .map(|full| {
            let cut = full.find('(').unwrap_or(full.len());
            full[..cut].trim_end().to_string()
        })
        .filter(|s| !s.is_empty());

    // Parameter syms in ordinal order. A function with NO param edges
    // still gets a sig — `<ret> name()` — when it has BOTH a return type
    // and a name (`int64_t clearCallingIdentity()`, a constructor's
    // `S S()`). That's the value-add over `typed`, which carries only the
    // type-only `<ret>()`. We require the return type (not just the name)
    // so synthetic zero-param nodes with no type — e.g. the JVM `<clinit>`
    // static initializer — don't leak in as bogus signatures. With no
    // params and no return type there is nothing useful to add → honest
    // emptiness, no row.
    let params = match side.params.get(fn_key) {
        Some(p) if !p.is_empty() => p,
        _ => {
            return match (&ret_type, &fn_name) {
                (Some(r), Some(n)) if !n.is_empty() => Some(format!("{r} {n}()")),
                _ => None,
            };
        }
    };

    // Each parameter: `<type> <name>` when both render, else whichever
    // is present. A param that yields neither is rendered as `?`.
    let mut parts: Vec<String> = Vec::with_capacity(params.len());
    let mut any_named = false;
    for pkey in params.values() {
        let pty = typed_of.get(pkey.as_str())
            .and_then(|tk| render_type(g, tk))
            .filter(|s| !s.is_empty());
        let pname = side.code.get(pkey.as_str())
            .and_then(|c| parse_marked_source_fqn(c))
            .map(|fqn| bare_param_name(short_name(&fqn)).to_string())
            .filter(|s| !s.is_empty());
        if pname.is_some() { any_named = true; }
        let part = match (pty, pname) {
            (Some(t), Some(n)) => format!("{t} {n}"),
            (Some(t), None)    => t,
            (None,    Some(n)) => n,
            (None,    None)    => "?".to_string(),
        };
        parts.push(part);
    }
    // The whole point of `sig` over `typed` is the param NAMES. If not a
    // single param resolved a name, the type-only `typed` already carries
    // everything — don't store a redundant row.
    if !any_named { return None; }

    let name = fn_name.unwrap_or_default();
    let head = match (ret_type, name.is_empty()) {
        (Some(r), false) => format!("{r} {name}"),
        (Some(r), true)  => r,
        (None,    false) => name,
        (None,    true)  => String::new(),
    };
    Some(format!("{head}({})", parts.join(", ")))
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
    fn render_cxx_nested_generic_box_of_box() {
        // Generic args recurse: tapp[Box, tapp[Box, int]] → "Box<Box<int>>".
        let mut g = MapGraph::default();
        g.leaf("Box", "record", BOX_CODE);
        g.leaf("int", "tbuiltin", INT_CODE);
        g.tapp("Box<int>", &["Box", "int"]);
        g.tapp("Box<Box<int>>", &["Box", "Box<int>"]);
        assert_eq!(render_type(&g, "Box<Box<int>>").as_deref(), Some("Box<Box<int>>"));
    }

    #[test]
    fn render_cxx_volatile_int() {
        // tapp[volatile#builtin, int] → "volatile int".
        let mut g = MapGraph::default();
        g.leaf("int", "tbuiltin", INT_CODE);
        g.builtin_leaf("volatile", "volatile");
        g.tapp("vint", &["volatile", "int"]);
        assert_eq!(render_type(&g, "vint").as_deref(), Some("volatile int"));
    }

    #[test]
    fn render_cxx_pointer_to_pointer() {
        // tapp[ptr, tapp[ptr, int]] → "int * *".
        let mut g = MapGraph::default();
        g.leaf("int", "tbuiltin", INT_CODE);
        g.builtin_leaf("ptr", "ptr");
        g.tapp("int*", &["ptr", "int"]);
        g.tapp("int**", &["ptr", "int*"]);
        assert_eq!(render_type(&g, "int**").as_deref(), Some("int * *"));
    }

    #[test]
    fn render_cxx_array_of_pointer_to_array_pinned_limitation() {
        // KNOWN LIMITATION, pinned (not hidden): array-of-pointer-to-array
        // `int (*x[])[]` = tapp[carr, tapp[ptr, tapp[carr, int]]]. The outer
        // array appends `[]` after the ptr-to-array element render
        // (`int (*)[]`), so we emit "int (*)[][]" rather than the strictly
        // idiomatic "int (*[])[]". The load-bearing array-of-ptr vs
        // ptr-to-array distinction still holds; this asserts the current
        // behavior so any future declarator rewrite trips on the change.
        let mut g = MapGraph::default();
        g.leaf("int", "tbuiltin", INT_CODE);
        g.builtin_leaf("carr", "carr");
        g.builtin_leaf("ptr", "ptr");
        g.tapp("int[]", &["carr", "int"]);
        g.tapp("ptr_to_arr", &["ptr", "int[]"]);
        g.tapp("arr_of_ptr_to_arr", &["carr", "ptr_to_arr"]);
        assert_eq!(render_type(&g, "arr_of_ptr_to_arr").as_deref(), Some("int (*)[][]"));
    }

    // Real java_indexer MarkedSource for the `K` type variable of
    // `<K> K pick(List<K>, int)`. It encodes the qualified form, so the
    // tvar short-name rule must reduce it to `K` (matching how cxx emits `T`).
    const JAVA_TVAR_K_CODE: &[u8] = &[
        26,24,8,4,26,5,8,3,18,1,71,26,8,8,3,18,4,112,105,99,107,34,1,46,80,1,26,5,8,3,18,1,75,
    ];

    #[test]
    fn render_java_type_variable_uses_short_name() {
        // The fixture's MarkedSource parses to the qualified tvar name, and a
        // tvar must display as its short name — proves the rule is load-bearing.
        let qualified = typename_from_marked_source(JAVA_TVAR_K_CODE).unwrap();
        assert_ne!(qualified, "K", "fixture must be the qualified tvar form, got {qualified:?}");
        let mut g = MapGraph::default();
        g.leaf("K", "tvar", JAVA_TVAR_K_CODE);
        assert_eq!(render_type(&g, "K").as_deref(), Some("K"));
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

    // ---- arrays (FIX 1) ----------------------------------------------------
    //
    // Node shapes captured verbatim from cxx_indexer / java_indexer on the
    // probe files (see the report). C++ uses `carr` (constant-bound array),
    // Java uses `array`. Both are a `tapp` whose head is a `tbuiltin` with op
    // `carr`/`array` and whose param.1 is the element type. The bound is NOT
    // in the node, so `int data[8]` is `int[]`, never `int[8]`.

    #[test]
    fn render_cxx_array_of_int() {
        // `int data[8]` → tapp[carr#builtin, int] → "int[]" (no bound).
        let mut g = MapGraph::default();
        g.leaf("int", "tbuiltin", INT_CODE);
        g.builtin_leaf("carr", "carr");
        g.tapp("int[]", &["carr", "int"]);
        assert_eq!(render_type(&g, "int[]").as_deref(), Some("int[]"));
    }

    #[test]
    fn render_cxx_array_of_array() {
        // `int grid[4][3]` → tapp[carr, tapp[carr, int]] → "int[][]".
        let mut g = MapGraph::default();
        g.leaf("int", "tbuiltin", INT_CODE);
        g.builtin_leaf("carr", "carr");
        g.tapp("int[]", &["carr", "int"]);
        g.tapp("int[][]", &["carr", "int[]"]);
        assert_eq!(render_type(&g, "int[][]").as_deref(), Some("int[][]"));
    }

    #[test]
    fn render_cxx_array_of_pointer() {
        // `int* ptrs[8]` (array of pointer) → tapp[carr, tapp[ptr, int]]
        // → "int *[]". The element render (`int *`) simply gets `[]`.
        let mut g = MapGraph::default();
        g.leaf("int", "tbuiltin", INT_CODE);
        g.builtin_leaf("carr", "carr");
        g.builtin_leaf("ptr", "ptr");
        g.tapp("int*", &["ptr", "int"]);
        g.tapp("int*[]", &["carr", "int*"]);
        assert_eq!(render_type(&g, "int*[]").as_deref(), Some("int *[]"));
    }

    #[test]
    fn render_cxx_pointer_to_array_vs_array_of_pointer() {
        // The gate: pointer-to-array (`int (*)[]`) must stay distinct from
        // array-of-pointer (`int *[]`).
        //   `int (*pa)[3]`  → tapp[ptr, tapp[carr, int]] → "int (*)[]"
        //   `int* ptrs[8]`  → tapp[carr, tapp[ptr, int]] → "int *[]"
        let mut g = MapGraph::default();
        g.leaf("int", "tbuiltin", INT_CODE);
        g.builtin_leaf("carr", "carr");
        g.builtin_leaf("ptr", "ptr");
        // pointer to array
        g.tapp("int[]", &["carr", "int"]);
        g.tapp("ptr_to_arr", &["ptr", "int[]"]);
        // array of pointer
        g.tapp("int*", &["ptr", "int"]);
        g.tapp("arr_of_ptr", &["carr", "int*"]);

        let pta = render_type(&g, "ptr_to_arr").unwrap();
        let aop = render_type(&g, "arr_of_ptr").unwrap();
        assert_eq!(pta, "int (*)[]");
        assert_eq!(aop, "int *[]");
        assert_ne!(pta, aop, "pointer-to-array must differ from array-of-pointer");
    }

    #[test]
    fn render_cxx_lvalue_and_rvalue_ref_to_array() {
        // `int (&)[]` and `int (&&)[]` — references to array take the same
        // parenthesized declarator form as pointer-to-array.
        let mut g = MapGraph::default();
        g.leaf("int", "tbuiltin", INT_CODE);
        g.builtin_leaf("carr", "carr");
        g.builtin_leaf("lvr", "lvr");
        g.builtin_leaf("rvr", "rvr");
        g.tapp("int[]", &["carr", "int"]);
        g.tapp("lvr_to_arr", &["lvr", "int[]"]);
        g.tapp("rvr_to_arr", &["rvr", "int[]"]);
        assert_eq!(render_type(&g, "lvr_to_arr").as_deref(), Some("int (&)[]"));
        assert_eq!(render_type(&g, "rvr_to_arr").as_deref(), Some("int (&&)[]"));
    }

    #[test]
    fn render_java_array_of_string() {
        // `String[] names` → tapp[array#builtin, String] → element render
        // is the FQN, so "java.lang.String[]".
        let mut g = MapGraph::default();
        g.leaf("String", "record", STRING_CODE);
        g.builtin_leaf("array", "array");
        g.tapp("String[]", &["array", "String"]);
        assert_eq!(render_type(&g, "String[]").as_deref(),
                   Some("java.lang.String[]"));
    }

    #[test]
    fn render_java_array_of_array_int() {
        // `int[][] grid` → tapp[array, tapp[array, int]] → "int[][]". Java
        // arrays have no pointer cases, so `array(E)` is always `E[]`.
        let mut g = MapGraph::default();
        g.leaf("int", "tbuiltin", INT_CODE);
        g.builtin_leaf("array", "array");
        g.tapp("int[]", &["array", "int"]);
        g.tapp("int[][]", &["array", "int[]"]);
        assert_eq!(render_type(&g, "int[][]").as_deref(), Some("int[][]"));
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

    // #5: a generic supertype tapp (`Comparable<T>`) renders HEAD + args
    // from the param graph and NEVER a bare `<>`. The bug was that the
    // tapp's own `/kythe/code` MarkedSource (head = a LOOKUP token) renders
    // to `<>`; the param-edge render recovers the head + arg.
    #[test]
    fn render_tapp_with_head_and_arg_never_bare_decoration() {
        // `Comparable` head as an IDENTIFIER MarkedSource (kind 3, pre_text).
        const COMPARABLE_HEAD_CODE: &[u8] = &[
            0x08, 0x03, 0x12, 0x0a, b'C', b'o', b'm', b'p', b'a', b'r', b'a', b'b', b'l', b'e',
        ];
        let mut g = MapGraph::default();
        // Head leaf: a record whose MarkedSource is the IDENTIFIER `Comparable`.
        g.leaf("Comparable", "record", COMPARABLE_HEAD_CODE);
        // Arg: a type variable (renders to its short name `K`).
        g.leaf("K", "tvar", JAVA_TVAR_K_CODE);
        g.tapp("Comparable<K>", &["Comparable", "K"]);
        let r = render_type(&g, "Comparable<K>").expect("renders");
        assert_eq!(r, "Comparable<K>", "head + arg, never bare <>: {r:?}");
        assert_ne!(r, "<>");
        assert!(r.starts_with("Comparable"), "head present: {r:?}");
    }

    #[test]
    fn is_degenerate_render_rejects_decoration_keeps_real_names() {
        // The emission-side backstop: pure decoration is degenerate; a real
        // name (even with generic decoration) is not.
        assert!(is_degenerate_render("<>"));
        assert!(is_degenerate_render(""));
        assert!(is_degenerate_render("   "));
        assert!(is_degenerate_render("[]"));
        assert!(is_degenerate_render("?"));
        assert!(is_degenerate_render("::"));
        assert!(is_degenerate_render("{}"));
        assert!(!is_degenerate_render("Comparable"));
        assert!(!is_degenerate_render("Comparable<T>"));
        assert!(!is_degenerate_render("kythe:java:c#r#X.java#SIG"));
        assert!(!is_degenerate_render("operator<"));
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

    // ---- sig renderer (param NAMES + types) --------------------------------
    //
    // Real cxx_indexer output for `int Gadget::frob(Widget* w, int n)`:
    //   * the function node's own /kythe/code MarkedSource (FROB_FN_CODE)
    //     renders to the FQN `Gadget::frob`; short name `frob`.
    //   * its /kythe/edge/typed points at a `fn` tapp whose render is
    //     `int(Widget *, int)` (return then arg types).
    //   * /kythe/edge/param.0 → param var `w`, param.1 → `n`. Each param
    //     var's /kythe/code MarkedSource (PARAM{N}_NAME_CODE) renders to
    //     `Gadget::frob::w` / `::n` (short name `w` / `n`), and each
    //     param's /kythe/edge/typed gives its own type node.

    // frob's own name MarkedSource (FQN `Gadget::frob`).
    const FROB_FN_CODE: &[u8] = &[
        0x1a, 0x07, 0x08, 0x01, 0x12, 0x03, 0x69, 0x6e, 0x74, 0x1a, 0x03, 0x12, 0x01, 0x20,
        0x1a, 0x20, 0x1a, 0x14, 0x08, 0x04, 0x1a, 0x0a, 0x08, 0x03, 0x12, 0x06, 0x47, 0x61,
        0x64, 0x67, 0x65, 0x74, 0x22, 0x02, 0x3a, 0x3a, 0x50, 0x01, 0x1a, 0x08, 0x08, 0x03,
        0x12, 0x04, 0x66, 0x72, 0x6f, 0x62, 0x1a, 0x0c, 0x08, 0x06, 0x12, 0x01, 0x28, 0x22,
        0x02, 0x2c, 0x20, 0x2a, 0x01, 0x29];
    // param.0 `w` name MarkedSource (FQN `Gadget::frob::w`).
    const FROB_W_NAME: &[u8] = &[
        0x1a, 0x0b, 0x08, 0x01, 0x12, 0x07, 0x57, 0x69, 0x64, 0x67, 0x65, 0x74, 0x2a, 0x1a,
        0x03, 0x12, 0x01, 0x20, 0x1a, 0x27, 0x1a, 0x1e, 0x08, 0x04, 0x1a, 0x0a, 0x08, 0x03,
        0x12, 0x06, 0x47, 0x61, 0x64, 0x67, 0x65, 0x74, 0x1a, 0x08, 0x08, 0x03, 0x12, 0x04,
        0x66, 0x72, 0x6f, 0x62, 0x22, 0x02, 0x3a, 0x3a, 0x50, 0x01, 0x1a, 0x05, 0x08, 0x03,
        0x12, 0x01, 0x77];
    // param.1 `n` name MarkedSource (FQN `Gadget::frob::n`).
    const FROB_N_NAME: &[u8] = &[
        0x1a, 0x07, 0x08, 0x01, 0x12, 0x03, 0x69, 0x6e, 0x74, 0x1a, 0x03, 0x12, 0x01, 0x20,
        0x1a, 0x27, 0x1a, 0x1e, 0x08, 0x04, 0x1a, 0x0a, 0x08, 0x03, 0x12, 0x06, 0x47, 0x61,
        0x64, 0x67, 0x65, 0x74, 0x1a, 0x08, 0x08, 0x03, 0x12, 0x04, 0x66, 0x72, 0x6f, 0x62,
        0x22, 0x02, 0x3a, 0x3a, 0x50, 0x01, 0x1a, 0x05, 0x08, 0x03, 0x12, 0x01, 0x6e];

    /// Build a [`TypeSide`] mirroring the frob function's CU graph, then
    /// drive `render_signature` over it exactly as ingest does. Returns
    /// the rendered sig (or None).
    fn frob_typeside() -> TypeSide {
        let mut side = TypeSide::default();
        // Function node + its name code + param edges.
        side.functions.push("frob".into());
        side.code.insert("frob".into(), FROB_FN_CODE.to_vec());
        let mut fp = std::collections::BTreeMap::new();
        fp.insert(0u32, "w".to_string());
        fp.insert(1u32, "n".to_string());
        side.params.insert("frob".into(), fp);
        // Return type: the function's typed edge → `fn` tapp = int(Widget*, int).
        side.edges.push(("frob".into(), "fn_t".into()));
        side.kind.insert("fn_t".into(), "tapp".into());
        side.builtin_op.insert("fn_op".into(), "fn".into());
        side.kind.insert("fn_op".into(), "tbuiltin".into());
        // fn tapp params: head=fn_op, ret=int, arg0=ptr(Widget), arg1=int
        let mut fnp = std::collections::BTreeMap::new();
        fnp.insert(0u32, "fn_op".to_string());
        fnp.insert(1u32, "int".to_string());
        fnp.insert(2u32, "wptr".to_string());
        fnp.insert(3u32, "int".to_string());
        side.params.insert("fn_t".into(), fnp);
        // int leaf.
        side.kind.insert("int".into(), "tbuiltin".into());
        side.code.insert("int".into(), INT_CODE.to_vec());
        // Widget* tapp: head=ptr_op (builtin), arg0=Widget(record).
        side.kind.insert("wptr".into(), "tapp".into());
        side.builtin_op.insert("ptr_op".into(), "ptr".into());
        side.kind.insert("ptr_op".into(), "tbuiltin".into());
        let mut wp = std::collections::BTreeMap::new();
        wp.insert(0u32, "ptr_op".to_string());
        wp.insert(1u32, "Widget".to_string());
        side.params.insert("wptr".into(), wp);
        side.kind.insert("Widget".into(), "record".into());
        side.code.insert("Widget".into(), WIDGET_CODE.to_vec());
        // Param names + their typed edges.
        side.code.insert("w".into(), FROB_W_NAME.to_vec());
        side.code.insert("n".into(), FROB_N_NAME.to_vec());
        side.edges.push(("w".into(), "wptr".into()));   // w : Widget *
        side.edges.push(("n".into(), "int".into()));    // n : int
        side
    }

    #[test]
    fn render_signature_with_param_names_cxx() {
        // `int Gadget::frob(Widget* w, int n)` — the sig must carry the
        // param NAMES (w, n) and their rendered types, plus the return.
        let side = frob_typeside();
        let g = CuTypeGraph { side: &side };
        let typed_of: HashMap<&str, &str> = side.edges.iter()
            .map(|(s, t)| (s.as_str(), t.as_str())).collect();
        let sig = render_signature(&g, "frob", &side, &typed_of).unwrap();
        assert_eq!(sig, "int frob(Widget * w, int n)");
        // The whole point over `typed`: the param names appear.
        assert!(sig.contains(" w"), "param name w present: {sig}");
        assert!(sig.contains(" n"), "param name n present: {sig}");
    }

    // Real cxx_indexer bytes for `void fill(int xs[4])`:
    //   * the function node's /kythe/code → FQN `fill`.
    //   * param.0 `xs`'s /kythe/code → FQN `fill::xs[4]` (the `[4]`
    //     declarator rides on the NAME — this is the doubling FIX 2 removes).
    //   * `xs`'s /kythe/edge/typed → tapp[carr#builtin, int] → `int[]`.
    const FILL_FN_CODE: &[u8] = &[
        26, 8, 8, 1, 18, 4, 118, 111, 105, 100, 26, 3, 18, 1, 32, 26, 8, 8, 3,
        18, 4, 102, 105, 108, 108, 26, 12, 8, 6, 18, 1, 40, 34, 2, 44, 32, 42,
        1, 41, 42, 1, 32];
    const FILL_XS_NAME: &[u8] = &[
        26, 8, 8, 1, 18, 4, 105, 110, 116, 32, 26, 28, 26, 18, 8, 4, 26, 8, 8,
        3, 18, 4, 102, 105, 108, 108, 34, 2, 58, 58, 80, 1, 26, 6, 8, 3, 18, 2,
        120, 115, 26, 7, 8, 1, 18, 3, 91, 52, 93];

    #[test]
    fn render_signature_array_param_name_is_bare() {
        // `void fill(int xs[4])` — FIX 1 renders the type `int[]`, FIX 2
        // strips the `[4]` off the NAME so it reads `int[] xs`, not the
        // doubled `int[] xs[4]` (nor the pre-fix `carr<int> xs[4]`).
        let mut side = TypeSide::default();
        side.functions.push("fill".into());
        side.code.insert("fill".into(), FILL_FN_CODE.to_vec());
        let mut fp = std::collections::BTreeMap::new();
        fp.insert(0u32, "xs".to_string());
        side.params.insert("fill".into(), fp);
        // xs : carr(int) → int[]
        side.code.insert("xs".into(), FILL_XS_NAME.to_vec());
        side.edges.push(("xs".into(), "xs_arr".into()));
        side.kind.insert("xs_arr".into(), "tapp".into());
        side.builtin_op.insert("carr_op".into(), "carr".into());
        side.kind.insert("carr_op".into(), "tbuiltin".into());
        let mut ap = std::collections::BTreeMap::new();
        ap.insert(0u32, "carr_op".to_string());
        ap.insert(1u32, "int".to_string());
        side.params.insert("xs_arr".into(), ap);
        side.kind.insert("int".into(), "tbuiltin".into());
        side.code.insert("int".into(), INT_CODE.to_vec());

        let g = CuTypeGraph { side: &side };
        let typed_of: HashMap<&str, &str> = side.edges.iter()
            .map(|(s, t)| (s.as_str(), t.as_str())).collect();
        let sig = render_signature(&g, "fill", &side, &typed_of).unwrap();
        assert_eq!(sig, "fill(int[] xs)");
        // The name must be the bare identifier — no `[4]` doubling.
        assert!(!sig.contains("[4]"), "no array bound on the name: {sig}");
        assert!(sig.contains("int[] xs"), "type carries the array, name is bare: {sig}");
    }

    #[test]
    fn render_signature_no_params_name_only_is_empty() {
        // A zero-param function with a name but NO return type adds nothing
        // useful over `typed` and risks surfacing synthetic nodes (JVM
        // `<clinit>`), so it produces no row. `g`'s MarkedSource names it
        // `frob`, but with no typed edge there is no return type.
        let mut side = TypeSide::default();
        side.functions.push("g".into());
        side.code.insert("g".into(), FROB_FN_CODE.to_vec());
        let g = CuTypeGraph { side: &side };
        let typed_of: HashMap<&str, &str> = HashMap::new();
        assert_eq!(render_signature(&g, "g", &side, &typed_of), None);
    }

    #[test]
    fn render_signature_no_params_with_return_type() {
        // The clearCallingIdentity case: a zero-param function with a
        // return-type typed edge renders `<ret> name()`. The function's
        // own typed edge is a `fn` tapp `int64_t()`; we take the `<ret>`
        // part and prepend the name.
        let mut side = TypeSide::default();
        side.functions.push("cci".into());
        // Name `frob` via the shared FROB_FN_CODE MarkedSource.
        side.code.insert("cci".into(), FROB_FN_CODE.to_vec());
        // typed edge → `fn` tapp with just a return leaf (no args).
        side.edges.push(("cci".into(), "fn_t".into()));
        side.kind.insert("fn_t".into(), "tapp".into());
        side.builtin_op.insert("fn_op".into(), "fn".into());
        side.kind.insert("fn_op".into(), "tbuiltin".into());
        let mut fnp = std::collections::BTreeMap::new();
        fnp.insert(0u32, "fn_op".to_string());   // head = fn
        fnp.insert(1u32, "int".to_string());     // ret  = int
        side.params.insert("fn_t".into(), fnp);
        side.kind.insert("int".into(), "tbuiltin".into());
        side.code.insert("int".into(), INT_CODE.to_vec());

        let g = CuTypeGraph { side: &side };
        let typed_of: HashMap<&str, &str> = side.edges.iter()
            .map(|(s, t)| (s.as_str(), t.as_str())).collect();
        assert_eq!(render_signature(&g, "cci", &side, &typed_of).as_deref(),
                   Some("int frob()"));
    }

    #[test]
    fn render_signature_no_params_no_name_no_type_is_empty() {
        // With NEITHER a name NOR a return type, a zero-param function adds
        // nothing over `typed` → honest emptiness, no row.
        let mut side = TypeSide::default();
        side.functions.push("g".into());
        let g = CuTypeGraph { side: &side };
        let typed_of: HashMap<&str, &str> = HashMap::new();
        assert_eq!(render_signature(&g, "g", &side, &typed_of), None);
    }

    #[test]
    fn render_signature_honest_emptiness_no_param_names() {
        // Params exist but none resolves a name (a degenerate node with a
        // typed edge but no NAME MarkedSource on the param). `typed` already
        // carries the type, so sig stays empty rather than duplicating it.
        // (NOTE: real java_indexer DOES emit a param-name MarkedSource on each
        // param node — see `ingest_java_indexer_sig_named_params` — so the
        // common Java case renders names; this guards only the no-name case.)
        let mut side = TypeSide::default();
        side.functions.push("g".into());
        let mut fp = std::collections::BTreeMap::new();
        fp.insert(0u32, "p0".to_string());
        side.params.insert("g".into(), fp);
        // p0 has a type but no name code.
        side.edges.push(("p0".into(), "int".into()));
        side.kind.insert("int".into(), "tbuiltin".into());
        side.code.insert("int".into(), INT_CODE.to_vec());
        let g = CuTypeGraph { side: &side };
        let typed_of: HashMap<&str, &str> = side.edges.iter()
            .map(|(s, t)| (s.as_str(), t.as_str())).collect();
        assert_eq!(render_signature(&g, "g", &side, &typed_of), None);
    }

    /// End-to-end Java signature emission, driven by REAL java_indexer
    /// output (not a hand-built graph). The fixture is the verbatim
    /// `.entries` stream the stock v0.0.75 java_indexer produced for
    ///
    ///   class S {
    ///     void setEnabled(boolean enabled){}
    ///     java.util.List<String> pick(int idx, String key){return null;}
    ///   }
    ///
    /// Regenerate the fixture with the stock v0.0.75 toolchain:
    ///   javac_extractor.jar -d DIR/cls S.java   (KYTHE_CORPUS=test,
    ///     KYTHE_OUTPUT_DIRECTORY/ROOT_DIRECTORY=DIR)
    ///   java_indexer.jar --temp_directory DIR/jtmp DIR/*.kzip > S.entries
    ///
    /// It exercises the whole pipeline the sig section depends on:
    ///   * the function SOURCE node (java_indexer's opaque-hash function
    ///     node) carries the `/kythe/edge/typed` (return) and `param.N`
    ///     edges;
    ///   * each param's NAME comes from its own `/kythe/code` MarkedSource
    ///     (java_indexer DOES emit this — `enabled`, `idx`, `key`);
    ///   * each param's TYPE renders via the shared type renderer
    ///     (`boolean`, `int`, `java.lang.String`, and the `java.util.List<…>`
    ///     tapp return);
    ///   * the sig is keyed on the SAME sym a Java method is queryable by:
    ///     name resolution (`def`/`sig NAME`) lands on the source node via
    ///     its `/kythe/edge/named` alias `S.setEnabled` (the JVM descriptor
    ///     stripped), and `sig_of` on that resolved sym returns the render.
    #[test]
    fn ingest_java_indexer_sig_named_params() {
        use crate::reader::Index;
        const ENTRIES: &[u8] =
            include_bytes!("../tests/fixtures/java_sig_S.entries");

        let mut builder = IndexBuilder::new();
        let fids = FileIdAllocator::default();
        let stats = ingest(ENTRIES, &mut builder, &fids).unwrap();
        // Both methods render a full named signature; the zero-param
        // default constructor `S()` now renders `S S()` too (return type +
        // name). The synthetic `<clinit>` static initializer has no return
        // type, so it stays out (honest emptiness).
        assert_eq!(stats.sigs_emitted, 3,
            "two methods + the constructor render; <clinit> excluded");
        fids.drain_into(&mut builder);

        // Round-trip through the on-disk format so we exercise `sig_of`
        // and `sym_for_name` exactly as the `sig`/`def` verbs do.
        let tid = format!("{:?}", std::thread::current().id());
        let tid_num: String = tid.chars().filter(|c| c.is_ascii_digit()).collect();
        let dir = std::env::temp_dir().join(format!(
            "scry2-java-sig-{}-{}", std::process::id(), tid_num));
        std::fs::create_dir_all(&dir).unwrap();
        let s2db = dir.join("java_sig.s2db");
        builder.finish(&s2db).unwrap();
        let idx = Index::open(&s2db).unwrap();

        // The exact strings the renderer must produce, by NAME resolution
        // (the same path the `sig NAME` / `def` verbs use). This proves
        // the sig is keyed on the queryable sym, not stranded on a node
        // that name lookup can't reach.
        let set_sym = idx.sym_for_name("S.setEnabled")
            .expect("S.setEnabled resolves (descriptor-stripped alias)");
        assert_eq!(idx.sig_of(set_sym), Some("void setEnabled(boolean enabled)"),
            "named param + builtin type + return on a void method");

        let pick_sym = idx.sym_for_name("S.pick")
            .expect("S.pick resolves (descriptor-stripped alias)");
        assert_eq!(
            idx.sig_of(pick_sym),
            Some("java.util.List<java.lang.String> pick(int idx, java.lang.String key)"),
            "generic return tapp + builtin and record param types + names");

        // The sig section holds exactly these three rows: the two methods
        // plus the zero-param constructor `S S()`.
        let rows: std::collections::BTreeSet<&str> =
            idx.iter_sig().map(|(_, s)| s).collect();
        assert_eq!(rows.len(), 3, "the two method sigs plus the constructor");
        assert!(rows.contains("void setEnabled(boolean enabled)"));
        assert!(rows.contains(
            "java.util.List<java.lang.String> pick(int idx, java.lang.String key)"));
        assert!(rows.contains("S S()"),
            "zero-param constructor renders `<ret> name()`");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// CU-finalize FQN normalization, end-to-end on the SAME real
    /// java_indexer fixture. After ingest, a method sym's DISPLAY name
    /// (what `sym_meta` returns and `members`/`super`/`def` render) must be
    /// the readable FQN promoted from its `/kythe/edge/named` alias — NOT
    /// the raw Kythe ticket the streaming `upsert_sym` seeded. Discovery
    /// (FQN → sym via `sym_for_name`) must keep working unchanged, and the
    /// resolved sym's metadata name must match the FQN it was found by.
    #[test]
    fn ingest_promotes_sym_display_name_to_fqn() {
        use crate::reader::Index;
        const ENTRIES: &[u8] =
            include_bytes!("../tests/fixtures/java_sig_S.entries");

        let mut builder = IndexBuilder::new();
        let fids = FileIdAllocator::default();
        ingest(ENTRIES, &mut builder, &fids).unwrap();
        fids.drain_into(&mut builder);

        let tid = format!("{:?}", std::thread::current().id());
        let tid_num: String = tid.chars().filter(|c| c.is_ascii_digit()).collect();
        let dir = std::env::temp_dir().join(format!(
            "scry2-fqn-norm-{}-{}", std::process::id(), tid_num));
        std::fs::create_dir_all(&dir).unwrap();
        let s2db = dir.join("fqn_norm.s2db");
        builder.finish(&s2db).unwrap();
        let idx = Index::open(&s2db).unwrap();

        // Discovery still works: the descriptor-stripped FQN resolves.
        let set_sym = idx.sym_for_name("S.setEnabled")
            .expect("S.setEnabled resolves (discovery unbroken)");
        // The sym's DISPLAY name is now the readable FQN, not a ticket.
        let (name, _, _) = idx.sym_meta(set_sym).unwrap();
        assert!(!name.starts_with("kythe:"),
            "display name must not be a raw ticket, got {name:?}");
        assert_eq!(name, "S.setEnabled",
            "display name promoted to the cleanest (descriptor-stripped) FQN");

        let pick_sym = idx.sym_for_name("S.pick")
            .expect("S.pick resolves (discovery unbroken)");
        let (pname, _, _) = idx.sym_meta(pick_sym).unwrap();
        assert!(!pname.starts_with("kythe:"),
            "pick display name must not be a raw ticket, got {pname:?}");
        assert_eq!(pname, "S.pick");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn short_name_strips_qualifier() {
        assert_eq!(short_name("Gadget::frob::enabled"), "enabled");
        assert_eq!(short_name("android.os.Binder.clearCallingIdentity"),
                   "clearCallingIdentity");
        assert_eq!(short_name("plain"), "plain");
        assert_eq!(short_name("a.b::c"), "c");
    }

    #[test]
    fn bare_param_name_strips_array_declarator() {
        // cxx_indexer puts the array declarator on the param NAME's
        // MarkedSource (`fill::xs[4]` → short name `xs[4]`); the bare
        // identifier drops it so the name doesn't double the rendered type.
        assert_eq!(bare_param_name("xs[4]"), "xs");
        assert_eq!(bare_param_name("grid[4][3]"), "grid");
        assert_eq!(bare_param_name("xs []"), "xs");
        // A non-array name passes through unchanged.
        assert_eq!(bare_param_name("w"), "w");
        assert_eq!(bare_param_name("enabled"), "enabled");
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

    /// Wire-format helpers shared by the completes-bridge test: encode a
    /// length-delimited proto field, a bare varint, an Entry, and a VName.
    fn ld(field: u64, bytes: &[u8], out: &mut Vec<u8>) {
        out.push(((field << 3) | 2) as u8);
        let mut v = bytes.len() as u64;
        while v >= 0x80 { out.push(((v & 0x7F) | 0x80) as u8); v >>= 7; }
        out.push(v as u8);
        out.extend_from_slice(bytes);
    }
    fn varint(mut v: u64, out: &mut Vec<u8>) {
        while v >= 0x80 { out.push(((v & 0x7F) | 0x80) as u8); v >>= 7; }
        out.push(v as u8);
    }
    /// A C++ VName: signature + corpus + path + language (c++).
    fn cxx_vname(sig: &[u8], path: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        ld(1, sig, &mut v);
        ld(2, b"test", &mut v);
        ld(4, path, &mut v);
        ld(5, b"c++", &mut v);
        v
    }
    fn entry(source: &[u8], edge: &[u8], target: &[u8], fact_name: &[u8], fact_value: &[u8]) -> Vec<u8> {
        let mut e = Vec::new();
        ld(1, source, &mut e);
        if !edge.is_empty()       { ld(2, edge, &mut e); }
        if !target.is_empty()     { ld(3, target, &mut e); }
        if !fact_name.is_empty()  { ld(4, fact_name, &mut e); }
        if !fact_value.is_empty() { ld(5, fact_value, &mut e); }
        e
    }
    fn push_entry(stream: &mut Vec<u8>, e: Vec<u8>) {
        varint(e.len() as u64, stream);
        stream.extend_from_slice(&e);
    }

    /// C++ DEF↔DECL completes bridge: a `.cpp` definition node `completes`
    /// the `.h` declaration it implements. After ingest, the FQN (an alias
    /// on the DECL) must resolve to a sym whose xrefs include BOTH the `.h`
    /// declaration location AND the `.cpp` definition location — i.e. the
    /// definition's xref, which streamed in under the DEF VName's sym, was
    /// remapped onto the DECL sym so the two unify.
    ///
    /// The `completes` edge is emitted LAST, after the rows it rewrites,
    /// to exercise the deferred (finalize-time) application: a bridge that
    /// arrives after the definition's own xref still rewrites it.
    #[test]
    fn completes_bridge_unifies_def_onto_decl() {
        use crate::reader::Index;

        // Nodes.
        let decl_vn  = cxx_vname(b"#decl", b"foo.h");
        let def_vn   = cxx_vname(b"#def",  b"foo.cpp");
        // The human FQN that `def Foo::bar` queries by — a `named` edge
        // target whose signature carries the name.
        let mut name_vn = Vec::new();
        ld(1, b"Foo::bar", &mut name_vn);
        // Declaration anchor in foo.h binding the DECL node.
        let danchor_vn = cxx_vname(b"#da", b"foo.h");
        // Definition anchor in foo.cpp binding the DEF node.
        let fanchor_vn = cxx_vname(b"#fa", b"foo.cpp");

        let mut stream = Vec::new();
        // DECL is a function node + carries the FQN alias.
        push_entry(&mut stream, entry(&decl_vn, b"", b"", b"/kythe/node/kind", b"function"));
        push_entry(&mut stream, entry(&decl_vn, b"/kythe/edge/named", &name_vn, b"/", b""));
        // Declaration anchor → DECL (defines/binding = DECL role).
        push_entry(&mut stream, entry(&danchor_vn, b"", b"", b"/kythe/node/kind", b"anchor"));
        push_entry(&mut stream, entry(&danchor_vn, b"", b"", b"/kythe/loc/start", b"100"));
        push_entry(&mut stream, entry(&danchor_vn, b"/kythe/edge/defines/binding", &decl_vn, b"/", b""));
        // DEF is a function node too.
        push_entry(&mut stream, entry(&def_vn, b"", b"", b"/kythe/node/kind", b"function"));
        // Definition anchor → DEF (defines/binding = DECL role on the DEF
        // node — its location in foo.cpp).
        push_entry(&mut stream, entry(&fanchor_vn, b"", b"", b"/kythe/node/kind", b"anchor"));
        push_entry(&mut stream, entry(&fanchor_vn, b"", b"", b"/kythe/loc/start", b"200"));
        push_entry(&mut stream, entry(&fanchor_vn, b"/kythe/edge/defines/binding", &def_vn, b"/", b""));
        // The bridge, emitted LAST: DEF completes DECL.
        push_entry(&mut stream, entry(&def_vn, b"/kythe/edge/completes", &decl_vn, b"/", b""));

        let mut builder = IndexBuilder::new();
        let fids = FileIdAllocator::default();
        let stats = ingest(&stream[..], &mut builder, &fids).unwrap();
        // The DEF's one xref row was remapped (1 applied remap).
        assert_eq!(stats.completes_bridges, 1, "exactly the def xref remapped to decl");
        fids.drain_into(&mut builder);

        // Round-trip through the on-disk format, exactly as the `def` verb.
        let dir = std::env::temp_dir().join(format!(
            "scry2-completes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let s2db = dir.join("completes.s2db");
        builder.finish(&s2db).unwrap();
        let idx = Index::open(&s2db).unwrap();

        // `def Foo::bar` resolves via the alias to the DECL sym.
        let decl_sym = sym_of(&parse_vname(&decl_vn).unwrap().to_symbol_string());
        let def_sym  = sym_of(&parse_vname(&def_vn).unwrap().to_symbol_string());
        assert_eq!(idx.sym_for_name("Foo::bar"), Some(decl_sym),
            "FQN resolves to the declaration sym");

        // The DECL sym now carries BOTH locations: the foo.h declaration
        // (offset 100) and — via the bridge — the foo.cpp definition
        // (offset 200, originally bound under the DEF sym).
        let mut paths: Vec<(String, u32)> = idx.xrefs(decl_sym, role::DECL, role::DEF)
            .map(|(_, _, file, off)| (idx.file_path(file).unwrap().to_string(), off))
            .collect();
        paths.sort();
        assert_eq!(paths, vec![
            ("foo.cpp".to_string(), 200),
            ("foo.h".to_string(),   100),
        ], "decl sym holds both the .h decl and the bridged .cpp def");

        // The DEF sym has NO xrefs of its own — they all moved to DECL.
        assert_eq!(idx.xrefs(def_sym, role::DECL, role::DEF).count(), 0,
            "def sym's xref was remapped away");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A Java field node — kind `variable` with subkind `field` — keeps its
    /// FQN alias (so `def`/`names` find it) and its type, and renders as
    /// FIELD. Without the subkind→FIELD promotion the writer's variable-kind
    /// alias suppression (which exists to drop C++ parameter aliases) would
    /// strip the field's `/kythe/edge/named` FQN, leaving it unqueryable.
    #[test]
    fn java_field_subkind_keeps_alias_and_type() {
        use crate::reader::Index;
        use crate::format::kind as kbyte;

        // VName proto: 1=signature, 2=corpus, 4=path, 5=language.
        fn java_vname(sig: &[u8], path: &[u8]) -> Vec<u8> {
            let mut v = Vec::new();
            ld(1, sig, &mut v);
            ld(2, b"test", &mut v);
            ld(4, path, &mut v);
            ld(5, b"java", &mut v);
            v
        }

        let field_vn = java_vname(b"#field", b"S.java");
        let type_vn  = java_vname(b"java.lang.String", b"");  // the field's type node
        // `named` edge target whose signature is the field FQN.
        let mut name_vn = Vec::new();
        ld(1, b"S.value", &mut name_vn);
        // Declaration anchor binding the field.
        let anchor_vn = java_vname(b"#a", b"S.java");

        let mut stream = Vec::new();
        // Field node: kind=variable THEN subkind=field (java_indexer order).
        push_entry(&mut stream, entry(&field_vn, b"", b"", b"/kythe/node/kind", b"variable"));
        push_entry(&mut stream, entry(&field_vn, b"", b"", b"/kythe/subkind", b"field"));
        // FQN alias + typed edge to the type node.
        push_entry(&mut stream, entry(&field_vn, b"/kythe/edge/named", &name_vn, b"/", b""));
        push_entry(&mut stream, entry(&field_vn, b"/kythe/edge/typed", &type_vn, b"/", b""));
        // The type node renders its name from a record kind + MarkedSource.
        push_entry(&mut stream, entry(&type_vn, b"", b"", b"/kythe/node/kind", b"record"));
        // MarkedSource (FQN `java.lang.String`) so render_type yields a name.
        push_entry(&mut stream, entry(&type_vn, b"", b"", b"/kythe/code", STRING_CODE));
        // Anchor binds the field (a DECL location).
        push_entry(&mut stream, entry(&anchor_vn, b"", b"", b"/kythe/node/kind", b"anchor"));
        push_entry(&mut stream, entry(&anchor_vn, b"", b"", b"/kythe/loc/start", b"42"));
        push_entry(&mut stream, entry(&anchor_vn, b"/kythe/edge/defines/binding", &field_vn, b"/", b""));

        let mut builder = IndexBuilder::new();
        let fids = FileIdAllocator::default();
        ingest(&stream[..], &mut builder, &fids).unwrap();
        fids.drain_into(&mut builder);

        let dir = std::env::temp_dir().join(format!(
            "scry2-jfield-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let s2db = dir.join("jfield.s2db");
        builder.finish(&s2db).unwrap();
        let idx = Index::open(&s2db).unwrap();

        // The FQN alias survived → `def`/`names` resolve the field.
        let field_sym = idx.sym_for_name("S.value")
            .expect("field FQN alias survives subkind→FIELD promotion");
        // The sym renders as FIELD, not VARIABLE.
        let (_, k, _) = idx.sym_meta(field_sym).expect("field sym present");
        assert_eq!(k, kbyte::FIELD, "variable+subkind=field promoted to FIELD");
        // The field's type was captured.
        assert_eq!(idx.type_of(field_sym), Some("java.lang.String"),
            "field keeps its typed edge");

        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- innermost_containing ----

    /// Brute-force reference: the smallest-span body anchor in `file`
    /// that contains `off`. Mirrors the pre-optimization forward scan.
    fn innermost_brute(
        anchors: &[(u32, u32, u32, u64)], file: u32, off: u32,
    ) -> Option<u64> {
        let mut best: Option<(u32, u64)> = None;
        for &(f, start, end, sym) in anchors {
            if f == file && start <= off && end > off {
                let span = end - start;
                if best.is_none_or(|(b, _)| span < b) {
                    best = Some((span, sym));
                }
            }
        }
        best.map(|(_, sym)| sym)
    }

    #[test]
    fn innermost_containing_nested_returns_inner() {
        // Properly-nested outer [0,100) sym=1 and inner [40,60) sym=2,
        // both in file 7. An offset inside the inner body returns inner.
        let anchors = vec![(7u32, 0u32, 100u32, 1u64), (7, 40, 60, 2)];
        assert_eq!(innermost_containing(&anchors, 7, 50), Some(2));
    }

    #[test]
    fn innermost_containing_outer_only_returns_outer() {
        // An offset inside the outer but outside the inner returns outer.
        let anchors = vec![(7u32, 0u32, 100u32, 1u64), (7, 40, 60, 2)];
        assert_eq!(innermost_containing(&anchors, 7, 10), Some(1));
        assert_eq!(innermost_containing(&anchors, 7, 70), Some(1));
    }

    #[test]
    fn innermost_containing_flat_siblings_returns_container() {
        // Flat sibling bodies [0,10) sym=1 and [20,30) sym=2: an offset
        // returns whichever sibling actually contains it, or None in the
        // gap between them.
        let anchors = vec![(3u32, 0u32, 10u32, 1u64), (3, 20, 30, 2)];
        assert_eq!(innermost_containing(&anchors, 3, 5), Some(1));
        assert_eq!(innermost_containing(&anchors, 3, 25), Some(2));
        assert_eq!(innermost_containing(&anchors, 3, 15), None, "gap between siblings");
    }

    #[test]
    fn innermost_containing_no_body_returns_none() {
        let anchors = vec![(1u32, 0u32, 10u32, 1u64), (1, 20, 30, 2)];
        // Past the last body.
        assert_eq!(innermost_containing(&anchors, 1, 40), None);
        // Wrong file entirely.
        assert_eq!(innermost_containing(&anchors, 9, 5), None);
        // Empty input.
        assert_eq!(innermost_containing(&[], 1, 5), None);
    }

    #[test]
    fn innermost_containing_picks_correct_file_at_boundary() {
        // Two files share an offset range. file_start/upper bounds must
        // keep the scan inside file 5 and not bleed into file 4 or 6.
        let mut anchors = vec![
            (4u32, 0u32, 100u32, 10u64),  // file 4 outer
            (5,    0,    100,    20),      // file 5 outer
            (5,    30,   70,     21),      // file 5 inner
            (6,    0,    100,    30),      // file 6 outer
        ];
        anchors.sort();
        assert_eq!(innermost_containing(&anchors, 5, 50), Some(21));
        assert_eq!(innermost_containing(&anchors, 5, 10), Some(20));
        assert_eq!(innermost_containing(&anchors, 4, 50), Some(10));
        assert_eq!(innermost_containing(&anchors, 6, 50), Some(30));
    }

    #[test]
    fn innermost_containing_agrees_with_brute_force_random() {
        // Deterministic xorshift PRNG — no external crates. Generate
        // many small, properly-nested fixtures and assert the optimized
        // fn agrees with the brute-force smallest-span reference at every
        // offset, for every file (including out-of-range files).
        let mut state: u64 = 0x9e3779b97f4a7c15;
        let mut rng = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..400 {
            let n_files = 1 + (rng() % 3) as u32;
            let mut anchors: Vec<(u32, u32, u32, u64)> = Vec::new();
            let mut sym: u64 = 1;
            for file in 0..n_files {
                // Build a properly-nested stack of bodies in this file by
                // shrinking [lo, hi) inward at each level.
                let depth = (rng() % 5) as u32; // 0..=4 nested levels
                let mut lo = (rng() % 20) as u32;
                let mut hi = lo + 20 + (rng() % 60) as u32;
                for _ in 0..depth {
                    if hi <= lo + 2 { break; }
                    anchors.push((file, lo, hi, sym));
                    sym += 1;
                    let half = u64::from((hi - lo) / 2).max(1);
                    let shrink_l = 1 + (rng() % half) as u32;
                    let shrink_r = 1 + (rng() % half) as u32;
                    lo += shrink_l;
                    hi = hi.saturating_sub(shrink_r);
                }
            }
            anchors.sort();
            // Query every file id 0..=n_files (n_files is intentionally
            // out of range) across a sweep of offsets.
            for file in 0..=n_files {
                for off in 0..120u32 {
                    assert_eq!(
                        innermost_containing(&anchors, file, off),
                        innermost_brute(&anchors, file, off),
                        "mismatch file={file} off={off} anchors={anchors:?}",
                    );
                }
            }
        }
    }

    #[test]
    fn seed_paths_assigns_dense_sorted_ids() {
        // Deterministic pre-seed: a path's id is its rank in the seed
        // order, regardless of the order it is later interned.
        let a = FileIdAllocator::default();
        a.seed_paths(["", "a.cpp", "b.h", "z.cc"]);
        assert_eq!(a.intern("a.cpp"), 1);
        assert_eq!(a.intern(""),      0);
        assert_eq!(a.intern("z.cc"),  3);
        assert_eq!(a.intern("b.h"),   2);
    }

    #[test]
    fn seed_paths_makes_intern_order_independent() {
        // The whole point: two allocators seeded identically hand out the
        // same id for the same path even when interned in opposite orders.
        let seed = ["", "alpha", "beta", "gamma"];
        let a = FileIdAllocator::default();
        a.seed_paths(seed);
        let b = FileIdAllocator::default();
        b.seed_paths(seed);
        // Intern in opposite orders.
        let ida: Vec<u32> = ["gamma", "alpha", "", "beta"].iter().map(|p| a.intern(p)).collect();
        let mut order_b = ["beta", "", "gamma", "alpha"];
        order_b.reverse();
        let _ = order_b.iter().map(|p| b.intern(p)).collect::<Vec<_>>();
        // Same path → same id in both, independent of intern order.
        for p in ["", "alpha", "beta", "gamma"] {
            assert_eq!(a.intern(p), b.intern(p), "path {p:?} got different ids");
        }
        // The ids are the sorted-seed ranks.
        assert_eq!(ida, vec![3, 1, 0, 2]);
    }

    #[test]
    fn seed_paths_then_new_path_gets_fresh_noncolliding_id() {
        // A path not in the seed (an interned path the superset missed)
        // still gets a fresh id past the seeded range — no collision with a
        // seeded id. (Such a path is the only non-deterministic case, which
        // the from-kzip seed is built to avoid by being a superset.)
        let a = FileIdAllocator::default();
        a.seed_paths(["x", "y"]); // ids 0,1
        let fresh = a.intern("brand-new");
        assert_eq!(fresh, 2, "fresh id continues past the seeded range");
        assert_ne!(fresh, a.intern("x"));
        assert_ne!(fresh, a.intern("y"));
    }
}
