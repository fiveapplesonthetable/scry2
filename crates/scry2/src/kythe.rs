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
    file_ids: &mut FileIdAllocator,
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
    file_ids: &mut FileIdAllocator,
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
    pub completes_bridges: usize,
    // Diagnostic: side-table sizes at resolution time. Useful for
    // spotting "0 calls emitted" failures — typically a sign that an
    // indexer used a different edge kind for childof than expected.
    pub diag_defines_seen: u64,
    pub diag_childof_seen: u64,
    pub diag_pending:      u64,
    pub diag_unresolved:   u64,
}

/// Maps file path → u32 id. The mapping is stable for the lifetime of
/// the allocator: two CUs that reference the same file get the same id.
#[derive(Default)]
pub struct FileIdAllocator {
    map: HashMap<String, u32>,
}

impl FileIdAllocator {
    pub fn intern(&mut self, path: &str) -> u32 {
        if let Some(&id) = self.map.get(path) { return id; }
        let id = self.map.len() as u32;
        self.map.insert(path.to_string(), id);
        id
    }
    pub fn drain_into(self, builder: &mut IndexBuilder) {
        for (path, id) in self.map {
            builder.upsert_file(id, &path);
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
    file_ids: &mut FileIdAllocator,
    stats: &mut IngestStats,
) {
    // Local aliases so the body code stays terse; the compiler elides
    // these on release builds.
    let anchors           = &mut state.anchors;
    let completes_bridges = &mut state.completes_bridges;
    let body_anchors      = &mut state.body_anchors;
    let call_sites        = &mut state.call_sites;
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
        // VName whose signature is the human FQN.
        if is_named_edge(&e.edge_kind) && !e.target.signature.is_empty() {
            let sym = sym_of(&source_key);
            builder.add_alias(sym, &e.target.signature);
            stats.aliases_emitted += 1;
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
    file_ids: &mut FileIdAllocator,
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
        if pos + len > buf.len() {
            bail!("Entry: field {} len {} extends past buffer (pos {} buf {})",
                field, len, pos, buf.len());
        }
        let slice = &buf[pos..pos + len];
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
        if pos + len > buf.len() {
            bail!("VName: field {} len {} extends past buffer", field, len);
        }
        let slice = &buf[pos..pos + len];
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
        let mut fids = FileIdAllocator::default();
        let stats = ingest(&stream[..], &mut builder, &mut fids).unwrap();
        assert_eq!(stats.entries, 4);
        assert_eq!(stats.anchors_flushed, 1);
        assert_eq!(stats.xrefs_emitted, 1);
        assert_eq!(builder.n_xrefs(), 1);
    }
}
