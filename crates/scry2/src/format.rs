//! Wire format for an `.s2db` index file. The whole thing is one mmap.
//!
//! Layout — every section is page-aligned (4 KB) so the kernel can map
//! it with `MADV_RANDOM` independently:
//!
//! ```text
//!   +-- offset 0 --------------------------+
//!   | Header (256 bytes, see below)        |
//!   +-- align 4 KB ------------------------+
//!   | xrefs[n_xrefs]    (17 B each)        |  sorted by (sym, role, file, offset)
//!   +-- align 4 KB ------------------------+
//!   | syms[n_syms]      (16 B each)        |  sorted by sym
//!   +-- align 4 KB ------------------------+
//!   | names_by_sort[n_syms] (16 B each)    |  sorted by name (alpha)
//!   +-- align 4 KB ------------------------+
//!   | files[n_files]    (10 B each)        |  sorted by file_id
//!   +-- align 4 KB ------------------------+
//!   | inherits[n_inh]   (16 B each)        |  sorted by (child, parent)
//!   +-- align 4 KB ------------------------+
//!   | blob (UTF-8 strings, no separators)  |  referenced by (off, len)
//!   +--------------------------------------+
//! ```
//!
//! All multi-byte integers are stored LE (host order). The mmap reader
//! does zero parsing — it casts byte slices to fixed-width record structs
//! and bsearches by raw byte compare on the BE-packed primary key.

use std::mem::size_of;

pub const MAGIC: [u8; 8]   = *b"S2DBv1\0\0";
pub const VERSION: u32     = 1;
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

    pub blob_off:     u64,
    pub blob_len:     u64,

    pub _reserved:    [u8; 256 - 8 - 4 - 4 - 8*12],
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
            blob_off:  0, blob_len: 0,
            _reserved: [0; 256 - 8 - 4 - 4 - 8*12],
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

/// One sym row. Sorted by `sym`. Total 16 bytes.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SymRow {
    pub sym:       [u8; 8],   // BE
    pub kind:      u8,        // function/type/var/...
    pub lang:      u8,        // 0=cxx 1=java 2=jvm 3=go 4=proto 5=rs ...
    pub name_off:  [u8; 4],   // BE offset into blob
    pub name_len:  [u8; 2],   // BE length in blob
}
pub const SYM_LEN: usize = 16;
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
/// Total 16 bytes per entry.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NameRow {
    pub name_off:  [u8; 4],   // BE
    pub name_len:  [u8; 2],   // BE
    pub _pad:      [u8; 2],
    pub sym:       [u8; 8],   // BE
}
pub const NAME_LEN: usize = 16;
const _: () = assert!(size_of::<NameRow>() == NAME_LEN);

/// One file row. 10 bytes — sorted by `file`.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileRow {
    pub file:      [u8; 4],   // BE
    pub path_off:  [u8; 4],   // BE
    pub path_len:  [u8; 2],   // BE
}
pub const FILE_LEN: usize = 10;
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
