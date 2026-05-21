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
/// v5 adds the trigram substring index (two appended sections: a
/// trigram dictionary and a postings blob). Its offsets are carved from
/// the header's former `_reserved` bytes. Dev mode is strict: there is
/// NO backward compat — `Index::open` accepts exactly version 5 (a v5
/// reader rejects a v4 file). This is intentional: the trigram path is
/// load-bearing for `--substr` latency and an index without it would
/// silently fall back to the slow linear scan, which we don't want to
/// ship unnoticed.
pub const VERSION: u32     = 5;
/// Lowest on-disk version this reader understands. Equal to VERSION:
/// strict single-version, no v3/v4 fallback.
pub const MIN_VERSION: u32 = 5;
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

    // ---- v4 comprehension layer (zero in a v3 file) ----
    pub typed_off:    u64,    // (sym, type-string blob ref) sorted by sym — resolved type of a sym
    pub typed_n:      u64,
    pub childrev_off: u64,    // (parent, child) reverse childof — `members NAME`
    pub childrev_n:   u64,
    pub inhrev_off:   u64,    // (parent, child) reverse inherits — O(log n) `sub`
    pub inhrev_n:     u64,
    pub sig_off:      u64,    // (sym, signature blob ref) sorted by sym — full rendered signature
    pub sig_n:        u64,

    // ---- v5 trigram substring index ----
    // Built ONCE post-merge over the final alpha-sorted names table, so it
    // never complicates the k-way merge. Two parts:
    //   trigram_dict: array of TrigramRow (TRIGRAM_LEN bytes) sorted
    //     ascending by the 3-byte trigram, binary-searchable. `_off` is a
    //     byte offset, `_n` a ROW count.
    //   trigram_post: a flat blob of u32 (LE) name-row-ids. `_off` is a
    //     byte offset, `_len` a BYTE length. Each dict row points at a
    //     contiguous, ascending run of `postings_count` ids within it.
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
/// this trigram's posting list within the postings blob, `post_count`
/// the number of u32 ids in that list (the list is `post_count * 4`
/// bytes). One explicit pad byte keeps the row a clean 16 bytes; it is
/// written as zero and never read. Postings ids are stored little-endian
/// (a flat `u32` array, host-cheap to read), distinct from the BE-packed
/// keyed tables — they are not memcmp-sorted, just an ascending numeric
/// run per list.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrigramRow {
    pub trigram:    [u8; 3],   // the 3 lowercased bytes (search key)
    pub _pad:       u8,        // zero; pads the row to 16 bytes
    pub post_off:   [u8; 8],   // BE byte offset into the postings blob
    pub post_count: [u8; 4],   // BE count of u32 ids in this list
}
pub const TRIGRAM_LEN: usize = 16;
const _: () = assert!(size_of::<TrigramRow>() == TRIGRAM_LEN);

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
