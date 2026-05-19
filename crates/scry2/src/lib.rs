//! scry2 — a super-lean Kythe wrapper for AOSP. Single mmap'd index file,
//! microsecond-scale queries on the verbs an LLM uses to walk code:
//! `def`, `ref`, `callers`, `super`, `sub`.

pub mod format;
pub mod kythe;
pub mod reader;
pub mod writer;

pub use format::{kind, lang, role, sym_of};
pub use reader::{Index, XrefIter};
pub use writer::IndexBuilder;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("scry2-test-{name}-{}.s2db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn round_trip_minimal() {
        let path = tmp("min");
        let mut b = IndexBuilder::new();
        let s_clear  = sym_of("android.os.Binder.clearCallingIdentity");
        let s_record = sym_of("com.android.server.am.ActivityManagerService.<init>");
        let s_iface  = sym_of("android.os.IBinder");
        let s_binder = sym_of("android.os.Binder");

        b.upsert_sym(s_clear,  kind::FUNCTION, lang::JAVA, "android.os.Binder.clearCallingIdentity");
        b.upsert_sym(s_record, kind::FUNCTION, lang::JAVA, "com.android.server.am.ActivityManagerService.<init>");
        b.upsert_sym(s_iface,  kind::TYPE,     lang::JAVA, "android.os.IBinder");
        b.upsert_sym(s_binder, kind::TYPE,     lang::JAVA, "android.os.Binder");

        b.upsert_file(1, "/aosp/frameworks/base/core/java/android/os/Binder.java");
        b.upsert_file(2, "/aosp/frameworks/base/services/core/java/com/android/server/am/ActivityManagerService.java");

        // Decl at line 373 col 25 ≈ offset 12345
        b.add_xref(s_clear, role::DECL, 1, 12345);
        // Calls from ActivityManagerService
        b.add_xref(s_clear, role::CALL, 2, 8001);
        b.add_xref(s_clear, role::CALL, 2, 9050);
        b.add_xref(s_clear, role::REF,  2, 7000);

        // Binder extends IBinder
        b.add_inherit(s_binder, s_iface);

        let n = b.finish(&path).unwrap();
        assert!(n > 4096, "index too small: {n}");

        // Reopen and query
        let ix = Index::open(&path).unwrap();
        assert_eq!(ix.n_xrefs(), 4);
        assert_eq!(ix.n_syms(),  4);
        assert_eq!(ix.n_files(), 2);
        assert_eq!(ix.n_inh(),   1);

        // name → sym
        assert_eq!(ix.sym_for_name("android.os.Binder.clearCallingIdentity"), Some(s_clear));
        assert_eq!(ix.sym_for_name("does.not.exist"), None);

        // sym → meta
        let (name, knd, lng) = ix.sym_meta(s_clear).unwrap();
        assert_eq!(name, "android.os.Binder.clearCallingIdentity");
        assert_eq!(knd, kind::FUNCTION);
        assert_eq!(lng, lang::JAVA);

        // def → 1 row, role=DECL
        let defs: Vec<_> = ix.xrefs(s_clear, role::DECL, role::DECL).collect();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].2, 1);
        assert_eq!(defs[0].3, 12345);

        // callers → 2 rows
        let calls: Vec<_> = ix.xrefs(s_clear, role::CALL, role::CALL).collect();
        assert_eq!(calls.len(), 2);

        // ref (all roles) → 4 rows
        let all: Vec<_> = ix.xrefs(s_clear, 0, u8::MAX).collect();
        assert_eq!(all.len(), 4);

        // file path
        assert_eq!(ix.file_path(1).unwrap(),
            "/aosp/frameworks/base/core/java/android/os/Binder.java");

        // inheritance: super of Binder = IBinder
        assert_eq!(ix.inherits_of(s_binder), vec![s_iface]);
        // sub of IBinder = Binder
        assert_eq!(ix.inherited_by(s_iface), vec![s_binder]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn name_substring_match() {
        let path = tmp("substr");
        let mut b = IndexBuilder::new();
        let s1 = sym_of("android.os.Binder.clearCallingIdentity");
        let s2 = sym_of("android.os.Binder.restoreCallingIdentity");
        let s3 = sym_of("android.app.ActivityManager.killProcess");
        b.upsert_sym(s1, kind::FUNCTION, lang::JAVA, "android.os.Binder.clearCallingIdentity");
        b.upsert_sym(s2, kind::FUNCTION, lang::JAVA, "android.os.Binder.restoreCallingIdentity");
        b.upsert_sym(s3, kind::FUNCTION, lang::JAVA, "android.app.ActivityManager.killProcess");
        b.finish(&path).unwrap();

        let ix = Index::open(&path).unwrap();
        let hits = ix.syms_matching_substring("CallingIdentity", 10);
        assert_eq!(hits.len(), 2);
        let hits = ix.syms_matching_substring("kill", 10);
        assert_eq!(hits.len(), 1);
        let _ = std::fs::remove_file(&path);
    }
}
