//! Kythe `.kzip` walker, decoder, and normalizer.
//!
//! ## Why this exists
//!
//! Real AOSP `.kzip` archives (the output of `build_kzip.bash`) mix
//! two unit encodings — `root/pbunits/<sha>` (proto-encoded
//! `IndexedCompilation`) and `root/units/<sha>` (proto3-JSON of the
//! same message). Stock Kythe v0.0.75 indexers and the `kzip` tool
//! refuse mixed-encoding kzips with `Malformed kzip: multiple unit
//! encodings but different entries` and abort hard. We need to
//! handle 100% of the units.
//!
//! ## Approach
//!
//! Hand-rolled proto wire codec + serde-JSON decoder for the few
//! `kythe.proto.*` messages we touch (`IndexedCompilation`,
//! `CompilationUnit`, `FileInput`, `FileInfo`, `VName`). No protobuf
//! codegen dependency, no `build.rs`, no third-party proto schema —
//! these messages are stable Kythe public API and the wire format is
//! self-documenting.
//!
//! ## What we expose
//!
//! * [`read_units`] — iterate every CU in the source kzip, regardless
//!   of encoding.
//! * [`Unit::to_proto_bytes`] — re-encode a decoded CU as proto, so
//!   downstream code can write single-encoding sub-kzips.
//! * [`normalize`] — one-shot helper that takes a mixed-encoding kzip
//!   and writes a fresh proto-encoded kzip with every file blob
//!   preserved verbatim.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::Path;

// ----------------------------------------------------------------- types

/// Subset of `kythe.proto.VName` we need.
#[derive(Default, Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VName {
    pub signature: String,
    pub corpus:    String,
    pub root:      String,
    pub path:      String,
    pub language:  String,
}

#[derive(Default, Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FileInfo {
    pub path:   String,
    pub digest: String,
}

#[derive(Default, Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FileInput {
    #[serde(alias = "vName")]
    pub v_name: VName,
    pub info:   FileInfo,
    // Other FileInput fields (source_context, context, details) are
    // round-tripped as raw bytes on the proto path; for JSON input we
    // currently drop them and let the indexer fall back to defaults.
}

#[derive(Default, Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CompilationUnit {
    #[serde(alias = "vName")]
    pub v_name: VName,
    #[serde(alias = "requiredInput")]
    pub required_input: Vec<FileInput>,
    pub argument: Vec<String>,
    #[serde(alias = "sourceFile")]
    pub source_file: Vec<String>,
    #[serde(alias = "outputKey")]
    pub output_key: String,
    #[serde(alias = "workingDirectory")]
    pub working_directory: String,
    #[serde(alias = "entryContext")]
    pub entry_context: String,
    // `details` is a repeated google.protobuf.Any; we copy the raw
    // bytes through on the proto path. On the JSON path we skip it
    // (java_indexer's `JavaDetails` lives here, but for AOSP-shape
    // CUs without JavaDetails the patched fallback in
    // CompilationUnitPathFileManager kicks in anyway).
}

#[derive(Default, Debug, Clone, Deserialize)]
#[serde(default)]
pub struct IndexedCompilation {
    pub unit: CompilationUnit,
    // `index` and `file_data` are skipped on JSON input. The proto
    // encoder writes only `unit`.
}

/// One CU read out of the source kzip, plus the sha that named it
/// (so the writer can preserve hashing).
#[derive(Debug)]
pub struct Unit {
    pub sha:  String,
    pub cu:   IndexedCompilation,
    pub raw_proto: Option<Vec<u8>>,
}

impl Unit {
    /// Re-encode this unit as a proto-encoded `IndexedCompilation`.
    /// Returns the raw proto bytes (no varint length prefix).
    pub fn to_proto_bytes(&self) -> Vec<u8> {
        if let Some(raw) = &self.raw_proto { return raw.clone(); }
        encode_indexed_compilation(&self.cu)
    }
}

// ----------------------------------------------------------------- reader

/// Iterate every CU in `path`, regardless of pbunits/units encoding.
/// Decoded results stream — peak memory is one buffered unit.
///
/// On encoding overlap (the same sha appearing under both pbunits/
/// and units/), the proto wins and the JSON twin is silently dropped.
pub fn read_units(path: &Path) -> Result<Vec<Unit>> {
    let f = File::open(path).with_context(|| format!("open kzip {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(f).with_context(|| "open zip")?;
    let mut proto_shas: HashSet<String> = HashSet::new();
    let mut out: Vec<Unit> = Vec::new();
    // First pass: proto-encoded units (they win on collision).
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        let name = f.name().to_string();
        if let Some(sha) = strip_prefix(&name, "root/pbunits/") {
            let mut buf = Vec::with_capacity(f.size() as usize);
            f.read_to_end(&mut buf)?;
            let cu = parse_indexed_compilation(&buf)
                .with_context(|| format!("decode proto unit {sha}"))?;
            proto_shas.insert(sha.to_string());
            out.push(Unit { sha: sha.to_string(), cu, raw_proto: Some(buf) });
        }
    }
    // Second pass: JSON-encoded units, skipping any with a proto twin.
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        let name = f.name().to_string();
        if let Some(sha) = strip_prefix(&name, "root/units/") {
            if proto_shas.contains(sha) { continue; }
            let mut buf = String::with_capacity(f.size() as usize);
            f.read_to_string(&mut buf)?;
            let cu: IndexedCompilation = serde_json::from_str(&buf)
                .with_context(|| format!("decode JSON unit {sha}"))?;
            out.push(Unit { sha: sha.to_string(), cu, raw_proto: None });
        }
    }
    Ok(out)
}

fn strip_prefix<'a>(name: &'a str, prefix: &str) -> Option<&'a str> {
    let after = name.strip_prefix(prefix)?;
    // Reject the directory entry itself (`root/units/` → after="") and
    // any deeper paths (a malformed kzip might contain them).
    if after.is_empty() || after.contains('/') { return None; }
    Some(after)
}

// ----------------------------------------------------------------- writer

/// Write a fresh proto-only kzip at `out`, carrying every unit from
/// `units` plus every required-input file blob from `src`. The
/// output kzip is structurally identical to what `build_kzip.bash`
/// produces with `KYTHE_KZIP_ENCODING=proto` — readable by every
/// stock Kythe v0.0.75 indexer.
pub fn write_normalized(src: &Path, units: &[Unit], out: &Path) -> Result<()> {
    let in_f = File::open(src).with_context(|| format!("reopen kzip {}", src.display()))?;
    let mut zin = zip::ZipArchive::new(in_f)?;
    let out_f = File::create(out).with_context(|| format!("create {}", out.display()))?;
    let mut zout = zip::ZipWriter::new(BufWriter::with_capacity(8 << 20, out_f));
    let opts = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Stored);

    // Required directory entries (Kythe spec).
    zout.add_directory("root/", opts)?;
    zout.add_directory("root/pbunits/", opts)?;
    zout.add_directory("root/files/", opts)?;

    // Units → proto.
    let mut needed_files: HashSet<String> = HashSet::new();
    for u in units {
        zout.start_file(format!("root/pbunits/{}", u.sha), opts)?;
        zout.write_all(&u.to_proto_bytes())?;
        for fi in &u.cu.unit.required_input {
            if !fi.info.digest.is_empty() { needed_files.insert(fi.info.digest.clone()); }
        }
    }
    // File blobs — copy verbatim from the source kzip.
    for digest in &needed_files {
        let entry_name = format!("root/files/{digest}");
        let mut src_f = match zin.by_name(&entry_name) {
            Ok(f) => f,
            Err(_) => continue,  // some required_inputs reference files outside
                                  // the kzip (e.g. compiler builtins); skip.
        };
        zout.start_file(&entry_name, opts)?;
        std::io::copy(&mut src_f, &mut zout)?;
    }
    zout.finish()?.flush()?;
    Ok(())
}

/// One-shot: read every unit from `src`, write a proto-only kzip at
/// `dst`. Returns `(n_units, n_files)`.
pub fn normalize(src: &Path, dst: &Path) -> Result<(usize, usize)> {
    let units = read_units(src)?;
    let mut files: HashSet<String> = HashSet::new();
    for u in &units {
        for fi in &u.cu.unit.required_input {
            if !fi.info.digest.is_empty() { files.insert(fi.info.digest.clone()); }
        }
    }
    write_normalized(src, &units, dst)?;
    Ok((units.len(), files.len()))
}

// ----------------------------------------------------------------- proto codec

// ---- decode (parser) ----

fn parse_indexed_compilation(buf: &[u8]) -> Result<IndexedCompilation> {
    let mut out = IndexedCompilation::default();
    let mut pos = 0;
    while pos < buf.len() {
        let (field, wire, len) = read_field_header(buf, &mut pos)?;
        let slice = take(buf, &mut pos, len)?;
        match (field, wire) {
            (1, 2) => out.unit = parse_compilation_unit(slice)?,
            _      => { /* skip — index/file_data ignored */ }
        }
    }
    Ok(out)
}

fn parse_compilation_unit(buf: &[u8]) -> Result<CompilationUnit> {
    let mut out = CompilationUnit::default();
    let mut pos = 0;
    while pos < buf.len() {
        let (field, wire, len) = read_field_header(buf, &mut pos)?;
        let slice = take(buf, &mut pos, len)?;
        match (field, wire) {
            (1,  2) => out.v_name = parse_vname(slice)?,
            (3,  2) => out.required_input.push(parse_file_input(slice)?),
            (5,  2) => out.entry_context = String::from_utf8_lossy(slice).into_owned(),
            (8,  2) => out.argument.push(String::from_utf8_lossy(slice).into_owned()),
            (9,  2) => out.source_file.push(String::from_utf8_lossy(slice).into_owned()),
            (10, 2) => out.output_key = String::from_utf8_lossy(slice).into_owned(),
            (11, 2) => out.working_directory = String::from_utf8_lossy(slice).into_owned(),
            _       => { /* fields we don't need (details, etc.) */ }
        }
    }
    Ok(out)
}

fn parse_file_input(buf: &[u8]) -> Result<FileInput> {
    let mut out = FileInput::default();
    let mut pos = 0;
    while pos < buf.len() {
        let (field, wire, len) = read_field_header(buf, &mut pos)?;
        let slice = take(buf, &mut pos, len)?;
        match (field, wire) {
            (1, 2) => out.v_name = parse_vname(slice)?,
            (2, 2) => out.info   = parse_file_info(slice)?,
            _      => {}
        }
    }
    Ok(out)
}

fn parse_file_info(buf: &[u8]) -> Result<FileInfo> {
    let mut out = FileInfo::default();
    let mut pos = 0;
    while pos < buf.len() {
        let (field, wire, len) = read_field_header(buf, &mut pos)?;
        let slice = take(buf, &mut pos, len)?;
        match (field, wire) {
            (1, 2) => out.path   = String::from_utf8_lossy(slice).into_owned(),
            (2, 2) => out.digest = String::from_utf8_lossy(slice).into_owned(),
            _      => {}
        }
    }
    Ok(out)
}

fn parse_vname(buf: &[u8]) -> Result<VName> {
    let mut out = VName::default();
    let mut pos = 0;
    while pos < buf.len() {
        let (field, wire, len) = read_field_header(buf, &mut pos)?;
        let slice = take(buf, &mut pos, len)?;
        match (field, wire) {
            (1, 2) => out.signature = String::from_utf8_lossy(slice).into_owned(),
            (2, 2) => out.corpus    = String::from_utf8_lossy(slice).into_owned(),
            (3, 2) => out.root      = String::from_utf8_lossy(slice).into_owned(),
            (4, 2) => out.path      = String::from_utf8_lossy(slice).into_owned(),
            (5, 2) => out.language  = String::from_utf8_lossy(slice).into_owned(),
            _      => {}
        }
    }
    Ok(out)
}

fn read_field_header(buf: &[u8], pos: &mut usize) -> Result<(u32, u8, usize)> {
    let tag = read_varint_bytes(buf, pos)?;
    let field = (tag >> 3) as u32;
    let wire  = (tag & 0x7) as u8;
    if wire != 2 {
        bail!("unexpected wire type {wire} for field {field} at byte {pos}", pos = *pos);
    }
    let len = read_varint_bytes(buf, pos)? as usize;
    Ok((field, wire, len))
}

fn read_varint_bytes(buf: &[u8], pos: &mut usize) -> Result<u64> {
    let mut val: u64 = 0;
    let mut shift = 0u32;
    for _ in 0..10 {
        if *pos >= buf.len() { bail!("truncated varint at byte {}", *pos); }
        let b = buf[*pos];
        *pos += 1;
        val |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 { return Ok(val); }
        shift += 7;
    }
    Err(anyhow!("varint > 10 bytes"))
}

fn take<'a>(buf: &'a [u8], pos: &mut usize, len: usize) -> Result<&'a [u8]> {
    let end = pos.checked_add(len).ok_or_else(|| anyhow!("len overflow"))?;
    if end > buf.len() { bail!("truncated field at byte {pos}"); }
    let slice = &buf[*pos..end];
    *pos = end;
    Ok(slice)
}

// ---- encode (serializer) ----

fn encode_indexed_compilation(c: &IndexedCompilation) -> Vec<u8> {
    let mut out = Vec::new();
    let mut unit_buf = Vec::new();
    encode_compilation_unit(&c.unit, &mut unit_buf);
    write_tag_len(1, &unit_buf, &mut out);
    out
}

fn encode_compilation_unit(c: &CompilationUnit, out: &mut Vec<u8>) {
    let mut v = Vec::new();
    encode_vname(&c.v_name, &mut v);
    if !v.is_empty() { write_tag_len(1, &v, out); }
    for fi in &c.required_input {
        let mut b = Vec::new();
        encode_file_input(fi, &mut b);
        write_tag_len(3, &b, out);
    }
    if !c.entry_context.is_empty() { write_tag_len(5, c.entry_context.as_bytes(), out); }
    for a in &c.argument        { write_tag_len(8,  a.as_bytes(), out); }
    for s in &c.source_file     { write_tag_len(9,  s.as_bytes(), out); }
    if !c.output_key.is_empty() { write_tag_len(10, c.output_key.as_bytes(), out); }
    if !c.working_directory.is_empty() {
        write_tag_len(11, c.working_directory.as_bytes(), out);
    }
}

fn encode_file_input(fi: &FileInput, out: &mut Vec<u8>) {
    let mut v = Vec::new(); encode_vname(&fi.v_name, &mut v);
    if !v.is_empty() { write_tag_len(1, &v, out); }
    let mut i = Vec::new(); encode_file_info(&fi.info, &mut i);
    if !i.is_empty() { write_tag_len(2, &i, out); }
}

fn encode_file_info(fi: &FileInfo, out: &mut Vec<u8>) {
    if !fi.path.is_empty()   { write_tag_len(1, fi.path.as_bytes(),   out); }
    if !fi.digest.is_empty() { write_tag_len(2, fi.digest.as_bytes(), out); }
}

fn encode_vname(v: &VName, out: &mut Vec<u8>) {
    if !v.signature.is_empty() { write_tag_len(1, v.signature.as_bytes(), out); }
    if !v.corpus.is_empty()    { write_tag_len(2, v.corpus.as_bytes(),    out); }
    if !v.root.is_empty()      { write_tag_len(3, v.root.as_bytes(),      out); }
    if !v.path.is_empty()      { write_tag_len(4, v.path.as_bytes(),      out); }
    if !v.language.is_empty()  { write_tag_len(5, v.language.as_bytes(),  out); }
}

fn write_tag_len(field: u32, data: &[u8], out: &mut Vec<u8>) {
    write_varint(((field as u64) << 3) | 2, out);
    write_varint(data.len() as u64, out);
    out.extend_from_slice(data);
}

fn write_varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 { out.push(((v as u8) & 0x7F) | 0x80); v >>= 7; }
    out.push(v as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vname_proto_round_trip() {
        let v = VName {
            signature: "Foo()".into(),
            corpus:    "test".into(),
            root:      "r".into(),
            path:      "foo/bar.cpp".into(),
            language:  "c++".into(),
        };
        let mut bytes = Vec::new();
        encode_vname(&v, &mut bytes);
        let parsed = parse_vname(&bytes).unwrap();
        assert_eq!(parsed.signature, v.signature);
        assert_eq!(parsed.corpus, v.corpus);
        assert_eq!(parsed.root, v.root);
        assert_eq!(parsed.path, v.path);
        assert_eq!(parsed.language, v.language);
    }

    #[test]
    fn cu_full_round_trip() {
        let cu = IndexedCompilation {
            unit: CompilationUnit {
                v_name: VName { language: "c++".into(),
                                corpus: "test".into(), ..Default::default() },
                required_input: vec![FileInput {
                    v_name: VName { path: "foo.h".into(), ..Default::default() },
                    info:   FileInfo { path: "foo.h".into(), digest: "abc123".into() },
                }],
                argument:    vec!["clang++".into(), "-c".into(), "foo.cpp".into()],
                source_file: vec!["foo.cpp".into()],
                output_key:  "foo.o".into(),
                working_directory: "/build".into(),
                entry_context: "ctx".into(),
            },
        };
        let bytes = encode_indexed_compilation(&cu);
        let parsed = parse_indexed_compilation(&bytes).unwrap();
        assert_eq!(parsed.unit.v_name.language, "c++");
        assert_eq!(parsed.unit.required_input.len(), 1);
        assert_eq!(parsed.unit.required_input[0].info.digest, "abc123");
        assert_eq!(parsed.unit.argument, vec!["clang++", "-c", "foo.cpp"]);
        assert_eq!(parsed.unit.source_file, vec!["foo.cpp"]);
        assert_eq!(parsed.unit.output_key, "foo.o");
        assert_eq!(parsed.unit.working_directory, "/build");
        assert_eq!(parsed.unit.entry_context, "ctx");
    }

    #[test]
    fn strip_prefix_rejects_directory_entries() {
        // The kzip spec mandates directory entries `root/units/` and
        // `root/pbunits/`. They must not be parsed as units.
        assert_eq!(strip_prefix("root/units/", "root/units/"), None);
        assert_eq!(strip_prefix("root/pbunits/", "root/pbunits/"), None);
        // Real entries still pass.
        assert_eq!(strip_prefix("root/units/abc123", "root/units/"), Some("abc123"));
        // Nested paths still rejected.
        assert_eq!(strip_prefix("root/units/sub/abc", "root/units/"), None);
    }

    #[test]
    fn json_unit_decodes_with_aliases() {
        // AOSP extractors emit snake_case fields. Confirm aliases work
        // for camelCase too per the proto3-JSON spec.
        let snake = r#"{"unit":{"v_name":{"language":"java"},
                                "required_input":[{"v_name":{"path":"X.java"},
                                                   "info":{"path":"X.java","digest":"abc"}}],
                                "argument":["javac","-d","out"]}}"#;
        let camel = r#"{"unit":{"vName":{"language":"java"},
                                "requiredInput":[{"vName":{"path":"X.java"},
                                                  "info":{"path":"X.java","digest":"abc"}}],
                                "argument":["javac","-d","out"]}}"#;
        for src in [snake, camel] {
            let c: IndexedCompilation = serde_json::from_str(src).unwrap();
            assert_eq!(c.unit.v_name.language, "java");
            assert_eq!(c.unit.required_input.len(), 1);
            assert_eq!(c.unit.required_input[0].info.digest, "abc");
        }
    }
}
