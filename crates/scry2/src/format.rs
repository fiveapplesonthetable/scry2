//! Wire format for an `.s2db` index file. The whole thing is one mmap.
//!
//! Layout — every section is page-aligned (4 KB) so each can be faulted
//! in independently:
//!
//! ```text
//!   +-- offset 0 --------------------------+
//!   | Header (256 bytes, see below)        |
//!   +-- align 4 KB ------------------------+
//!   | xrefs[n_xrefs]    (17 B each)        |  sorted by (sym, role, file, offset)
//!   +-- align 4 KB ------------------------+
//!   | syms[n_syms]      (20 B each)        |  sorted by sym
//!   +-- align 4 KB ------------------------+
//!   | names[n_names]    (18 B each)        |  sorted by name bytes (alpha), then sym
//!   +-- align 4 KB ------------------------+
//!   | files[n_files]    (14 B each)        |  sorted by file_id
//!   +-- align 4 KB ------------------------+
//!   | inherits[n_inh]   (16 B each)        |  sorted by (child, parent)
//!   +-- align 4 KB ------------------------+
//!   | calls[n_calls]    (17 B each)        |  sorted by (caller, callee, role)
//!   +-- align 4 KB ------------------------+
//!   | crev[n_calls]     (17 B each)        |  same rows sorted by callee
//!   +-- align 4 KB ------------------------+
//!   | blob (UTF-8 strings, no separators)  |  referenced by (u64 off, u16 len)
//!   +--------------------------------------+
//! ```
//!
//! Row KEYS are stored big-endian-packed, so a raw byte compare (`memcmp`)
//! equals the logical sort order — that's what makes every lookup a plain
//! binary search over a cast byte slice with zero parsing. The only
//! host-endian structure is the 256-byte Header (so the file is not
//! portable across BE/LE hosts, which is fine in practice). Blob offsets
//! are u64: the names+paths blob exceeds 4 GiB on a full corpus.

use std::mem::size_of;

pub const MAGIC: [u8; 8]   = *b"S2DBv2\0\0";
/// v6 is the comprehension layer (xrefs/syms/names/files plus the
/// inheritance, callgraph, type, membership and signature sections) PLUS
/// the trigram substring index that accelerates `--substr`. The trigram
/// index is two appended sections: a dictionary and a postings blob, whose
/// offsets are carved from the header's former `_reserved` bytes.
///
/// Each trigram's postings are BLOCK-SKIP COMPRESSED. The ids are the
/// ascending name-row-ids that contain the (lowercased) trigram; they are
/// split into fixed blocks of `TRIGRAM_BLOCK` ids, each block stores its
/// own LEB128 gap-delta varints (dense lists collapse to ~1 byte per id,
/// far below a raw `u32`), and a per-trigram skip-table holds one
/// `(max_id, block_byte_offset)` entry per block so a membership probe can
/// binary-search to the one block that could hold a given id and decode
/// ONLY that block. The dict row carries the posting `count`, so the query
/// can pick the most-selective driver trigram WITHOUT decoding anything.
///
/// Dev mode is strict: there is NO backward compat — `Index::open` accepts
/// exactly version 6 (a v6 reader rejects a v4 file). This is intentional:
/// the trigram path is load-bearing for `--substr` latency and an index
/// without it would silently fall back to the slow linear scan, which we
/// don't want to ship unnoticed.
pub const VERSION: u32     = 6;
/// Lowest on-disk version this reader understands. Equal to VERSION:
/// strict single-version, no older-format fallback.
pub const MIN_VERSION: u32 = 6;

/// Postings block size: each trigram's ascending name-row-ids are split
/// into blocks of this many ids. The skip-table carries one entry per
/// block, so a membership probe binary-searches the skip-table for the
/// block whose `max_id >= candidate`, then decodes only that one block
/// (at most `TRIGRAM_BLOCK` varints). Smaller blocks = finer skipping but
/// a larger skip-table (more overhead bytes); 128 keeps the skip-table
/// under ~1.6% of a dense list while bounding a probe's decode to 128
/// varints.
pub const TRIGRAM_BLOCK: usize = 128;
pub const PAGE: usize      = 4096;

/// File header — first 256 bytes. Numbers count rows, *not* bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub magic:        [u8; 8],
    pub version:      u32,
    pub _pad0:        u32,

    pub xrefs_off:    u64,
    pub xrefs_n:      u64,

    pub syms_off:     u64,
    pub syms_n:       u64,

    pub names_off:    u64,    // alphabetical index into syms
    pub names_n:      u64,

    pub files_off:    u64,
    pub files_n:      u64,

    pub inh_off:      u64,
    pub inh_n:        u64,

    pub calls_off:    u64,    // (caller, callee, role) — sorted by caller
    pub calls_n:      u64,

    pub crev_off:     u64,    // same rows, sorted by callee  (O(log n) reverse lookup)
    pub crev_n:       u64,

    pub blob_off:     u64,
    pub blob_len:     u64,

    // ---- comprehension layer ----
    pub typed_off:    u64,    // (sym, type-string blob ref) sorted by sym — resolved type of a sym
    pub typed_n:      u64,
    pub childrev_off: u64,    // (parent, child) reverse childof — `members NAME`
    pub childrev_n:   u64,
    pub inhrev_off:   u64,    // (parent, child) reverse inherits — O(log n) `sub`
    pub inhrev_n:     u64,
    pub sig_off:      u64,    // (sym, signature blob ref) sorted by sym — full rendered signature
    pub sig_n:        u64,

    // ---- trigram substring index ----
    // Built ONCE post-merge over the final alpha-sorted names table, so it
    // never complicates the k-way merge. Two parts:
    //   trigram_dict: array of TrigramRow (TRIGRAM_LEN bytes) sorted
    //     ascending by the 3-byte trigram, binary-searchable. `_off` is a
    //     byte offset, `_n` a ROW count.
    //   trigram_post: a varint blob of LEB128 gap-delta block-skip regions.
    //     `_off` is a byte offset, `_len` a BYTE length. Each dict row
    //     points at a contiguous region (`post_off`, `post_len` bytes)
    //     decoding to that trigram's ascending name-row-ids.
    pub trigram_dict_off: u64,
    pub trigram_dict_n:   u64,   // row count (TrigramRow rows)
    pub trigram_post_off: u64,
    pub trigram_post_len: u64,   // byte length of the postings blob

    pub _reserved:    [u8; 256 - 8 - 4 - 4 - 8*28],
}

impl Default for Header {
    fn default() -> Self {
        Self {
            magic: [0; 8], version: 0, _pad0: 0,
            xrefs_off: 0, xrefs_n: 0,
            syms_off:  0, syms_n:  0,
            names_off: 0, names_n: 0,
            files_off: 0, files_n: 0,
            inh_off:   0, inh_n:   0,
            calls_off: 0, calls_n: 0,
            crev_off:  0, crev_n:  0,
            blob_off:  0, blob_len: 0,
            typed_off: 0, typed_n: 0,
            childrev_off: 0, childrev_n: 0,
            inhrev_off: 0, inhrev_n: 0,
            sig_off:   0, sig_n:   0,
            trigram_dict_off: 0, trigram_dict_n: 0,
            trigram_post_off: 0, trigram_post_len: 0,
            _reserved: [0; 256 - 8 - 4 - 4 - 8*28],
        }
    }
}

const _: () = assert!(size_of::<Header>() == 256);

/// One xref row. Keys are BE-packed so memcmp ordering = lexicographic
/// ordering on (sym, role, file, offset). 17 bytes — no padding.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct XrefRow {
    pub sym:    [u8; 8],   // sym_u64 BE
    pub role:   u8,
    pub file:   [u8; 4],   // file_u32 BE
    pub offset: [u8; 4],   // offset_u32 BE
}
pub const XREF_LEN: usize = 17;
const _: () = assert!(size_of::<XrefRow>() == XREF_LEN);

/// Roles. We collapse Kythe edge kinds onto these.
///   `defines/binding` → Decl
///   `ref`             → Ref
///   `ref/call`        → Call
///   `defines`         → Def
///   `extends`         → handled by inherits[] table, not xrefs.
pub mod role {
    pub const DECL: u8 = 0;
    pub const DEF:  u8 = 1;
    pub const REF:  u8 = 2;
    pub const CALL: u8 = 3;
}

/// One sym row. Sorted by `sym`. Total 20 bytes.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SymRow {
    pub sym:       [u8; 8],   // BE
    pub kind:      u8,        // function/type/var/...
    pub lang:      u8,        // 0=cxx 1=java 2=jvm 3=go 4=proto 5=rs ...
    pub name_off:  [u8; 8],   // BE offset into blob (u64: blob exceeds 4 GiB)
    pub name_len:  [u8; 2],   // BE length in blob
}
pub const SYM_LEN: usize = 20;
const _: () = assert!(size_of::<SymRow>() == SYM_LEN);

/// Symbol kinds. Kept compact; only what query verbs branch on.
pub mod kind {
    pub const UNK:      u8 = 0;
    pub const FUNCTION: u8 = 1;
    pub const TYPE:     u8 = 2;
    pub const VARIABLE: u8 = 3;
    pub const FIELD:    u8 = 4;
    pub const PACKAGE:  u8 = 5;
}

pub mod lang {
    pub const UNK:    u8 = 0;
    pub const CXX:    u8 = 1;
    pub const JAVA:   u8 = 2;
    pub const JVM:    u8 = 3;
    pub const GO:     u8 = 4;
    pub const PROTO:  u8 = 5;
    pub const RUST:   u8 = 6;
    pub const KOTLIN: u8 = 7;
}

/// One name-index row. The blob slot points at the qualified name; the
/// table is sorted by **the bytes of that name** so binary search on
/// "android.os.Binder.clearCallingIdentity" lands on the right row.
///
/// Total 18 bytes per entry.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NameRow {
    pub name_off:  [u8; 8],   // BE offset into blob (u64: blob exceeds 4 GiB)
    pub name_len:  [u8; 2],   // BE
    pub sym:       [u8; 8],   // BE
}
pub const NAME_LEN: usize = 18;
const _: () = assert!(size_of::<NameRow>() == NAME_LEN);

/// One file row. 10 bytes — sorted by `file`.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileRow {
    pub file:      [u8; 4],   // BE
    pub path_off:  [u8; 8],   // BE offset into blob (u64: blob exceeds 4 GiB)
    pub path_len:  [u8; 2],   // BE
}
pub const FILE_LEN: usize = 14;
const _: () = assert!(size_of::<FileRow>() == FILE_LEN);

/// One inheritance edge: (child, parent). Sorted by (child, parent).
/// 16 bytes.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InhRow {
    pub child:   [u8; 8],
    pub parent:  [u8; 8],
}
pub const INH_LEN: usize = 16;
const _: () = assert!(size_of::<InhRow>() == INH_LEN);

// (CallRow declared below — keep the type alias `CALL_LEN` accessible
//  from the writer module without an extra import.)

/// One callgraph edge: (caller, callee, role). Sorted by
/// (caller, callee, role). 17 bytes — `caller` is the function body
/// containing the call, `callee` is the symbol being called.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallRow {
    pub caller:  [u8; 8],
    pub callee:  [u8; 8],
    pub role:    u8,        // role::CALL or role::REF
}
pub const CALL_LEN: usize = 17;
const _: () = assert!(size_of::<CallRow>() == CALL_LEN);

/// One sym→string row, sorted by `sym`. Backs both the `typed` section
/// (sym → its resolved type, pre-rendered to a string at ingest) and the
/// `sig` section (sym → its full rendered signature). The string lives in
/// the blob, referenced by (u64 off, u16 len) like every other name. 18 B.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TypeRow {
    pub sym:      [u8; 8],   // BE
    pub str_off:  [u8; 8],   // BE offset into blob
    pub str_len:  [u8; 2],   // BE length in blob
}
pub const TYPE_LEN: usize = 18;
const _: () = assert!(size_of::<TypeRow>() == TYPE_LEN);

/// One trigram-dictionary row. The `trigram` is the binary-search key —
/// it sits first and the dictionary is sorted ascending by it, so a
/// search compares only these 3 bytes. `post_off` is the BYTE offset of
/// this trigram's posting REGION within the postings blob, `post_len` the
/// BYTE length of that whole region (skip-table + packed blocks). `count`
/// is the number of postings (ascending name-row-ids) in the list, used
/// to pick the most-selective driver trigram WITHOUT decoding anything.
/// One explicit pad byte keeps the row a clean 20 bytes; it is written as
/// zero and never read.
///
/// Postings are BLOCK-SKIP COMPRESSED. Each trigram's region is laid out as:
///
/// ```text
///   [ skip-table: n_blocks * SKIP_ENTRY bytes ]
///   [ packed blocks: gap-delta varints        ]
/// ```
///
/// where `n_blocks = ceil(count / TRIGRAM_BLOCK)`. Each skip entry is
/// `max_id` (u32 BE — the LAST/largest id in that block) followed by
/// `block_off` (u32 BE — the byte offset where that block's varints start,
/// RELATIVE to the start of this trigram's region). Within a block the ids
/// are LEB128 gap-deltas: the block's first id is stored as-is (delta from
/// 0), each later id in the block is the varint of `id - prev_in_block`.
/// Dense lists (consecutive ids) collapse to ~1 byte per posting, far
/// below the 4 bytes a raw `u32` array costs. A membership probe
/// binary-searches the skip-table for the first block whose `max_id >=
/// candidate`, decodes ONLY that block, and checks for the id — so a probe
/// costs O(log n_blocks + TRIGRAM_BLOCK) regardless of total list size.
/// The ids are not memcmp-sorted — just an ascending numeric run per
/// block — so they are stored as a varint stream, distinct from the
/// BE-packed keyed tables; the skip entries ARE BE so a future memcmp pass
/// could use them, but the reader parses them numerically.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrigramRow {
    pub trigram:    [u8; 3],   // the 3 lowercased bytes (search key)
    pub _pad:       u8,        // zero; pads the row to 20 bytes
    pub post_off:   [u8; 8],   // BE byte offset of this trigram's region
    pub post_len:   [u8; 4],   // BE byte length of the whole region
    pub count:      [u8; 4],   // BE posting count (drives selectivity choice)
}
pub const TRIGRAM_LEN: usize = 20;
const _: () = assert!(size_of::<TrigramRow>() == TRIGRAM_LEN);

/// One skip-table entry: the block's `max_id` (largest/last id in the
/// block) and the byte offset (relative to the trigram's region start)
/// where the block's gap-delta varints begin. Both u32 BE. 8 bytes.
pub const SKIP_ENTRY: usize = 8;

/// Append `v` to `out` as an unsigned LEB128 varint (7 data bits per
/// byte, high bit = "more bytes follow"). Used to encode trigram posting
/// gap-deltas. A u32 takes 1–5 bytes; the dense gaps a trigram posting
/// list produces are almost always 1–2.
#[inline]
pub fn write_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Number of bytes `v` occupies as an unsigned LEB128 varint. Used by the
/// build's first (counting) pass to size each trigram's region without
/// materializing the bytes.
#[inline]
pub fn varint_len(mut v: u32) -> usize {
    let mut n = 1;
    while v >= 0x80 { v >>= 7; n += 1; }
    n
}

/// Decode one unsigned LEB128 varint from `buf` starting at `pos`,
/// returning `(value, bytes_consumed)`. The inverse of `write_varint`.
#[inline]
pub fn read_varint(buf: &[u8], pos: usize) -> (u32, usize) {
    let mut v: u32 = 0;
    let mut shift = 0u32;
    let mut i = pos;
    loop {
        let b = buf[i];
        v |= ((b & 0x7f) as u32) << shift;
        i += 1;
        if b & 0x80 == 0 { break; }
        shift += 7;
    }
    (v, i - pos)
}

/// Page-align a byte offset up to the next 4 KB boundary.
#[inline] pub fn pad_up(n: u64) -> u64 {
    let p = PAGE as u64;
    (n + p - 1) & !(p - 1)
}

/// Stable 64-bit hash of a fully-qualified name → `sym` id. Using
/// xxHash because (a) it's fast (3 GB/s on this CPU), (b) collision
/// rate at our scale is ~0 (5M syms / 2^64 = 2.7e-13), (c) it's used
/// the same way Kythe internally hashes node VNames.
pub fn sym_of(name: &str) -> u64 {
    use std::hash::Hasher;
    let mut h = twox_hash::XxHash64::with_seed(0);
    h.write(name.as_bytes());
    h.finish()
}
