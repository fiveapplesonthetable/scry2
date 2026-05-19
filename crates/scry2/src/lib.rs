//! scry2 — a super-lean Kythe wrapper for AOSP. Single mmap'd index file,
//! microsecond-scale queries on the verbs an LLM uses to walk code:
//! `def`, `ref`, `callers`, `super`, `sub`.

pub mod format;
pub mod kythe;
pub mod reader;
pub mod reply;
pub mod server;
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
    fn alias_resolves_to_same_sym() {
        let path = tmp("alias");
        let mut b = IndexBuilder::new();
        // Use a raw VName-style string as canonical, plus an alias
        // representing the human-typeable FQN (what a `/kythe/edge/named`
        // edge would give us).
        let canon = "kythe:java:android##core/java/android/os/Binder.java#clearCallingIdentity()";
        let alias = "android.os.Binder.clearCallingIdentity";
        let s = sym_of(canon);
        b.upsert_sym(s, kind::FUNCTION, lang::JAVA, canon);
        b.add_alias(s, alias);
        b.upsert_file(1, "/aosp/.../Binder.java");
        b.add_xref(s, role::DECL, 1, 12345);
        b.finish(&path).unwrap();

        let ix = Index::open(&path).unwrap();
        assert_eq!(ix.sym_for_name(canon), Some(s),
            "canonical name still resolves");
        assert_eq!(ix.sym_for_name(alias), Some(s),
            "alias resolves to the same sym");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn callgraph_round_trip_both_directions() {
        // Three-function call chain: foo() → bar() → baz()
        let path = tmp("callgraph");
        let mut b = IndexBuilder::new();
        let s_foo = sym_of("kythe:c++:test###foo");
        let s_bar = sym_of("kythe:c++:test###bar");
        let s_baz = sym_of("kythe:c++:test###baz");
        b.upsert_sym(s_foo, kind::FUNCTION, lang::CXX, "foo");
        b.upsert_sym(s_bar, kind::FUNCTION, lang::CXX, "bar");
        b.upsert_sym(s_baz, kind::FUNCTION, lang::CXX, "baz");
        b.add_call(s_foo, s_bar, role::CALL);
        b.add_call(s_bar, s_baz, role::CALL);
        b.finish(&path).unwrap();

        let ix = Index::open(&path).unwrap();
        // Down from foo:
        let down: Vec<_> = ix.calls_from(s_foo).into_iter().map(|(s, _)| s).collect();
        assert_eq!(down, vec![s_bar], "foo calls bar");
        // Down from bar:
        let down: Vec<_> = ix.calls_from(s_bar).into_iter().map(|(s, _)| s).collect();
        assert_eq!(down, vec![s_baz], "bar calls baz");
        // Down from baz:
        assert!(ix.calls_from(s_baz).is_empty(), "baz calls nothing");
        // Up: who calls baz?
        let up: Vec<_> = ix.called_by(s_baz).into_iter().map(|(s, _)| s).collect();
        assert_eq!(up, vec![s_bar], "baz called by bar");
        // Up: who calls bar?
        let up: Vec<_> = ix.called_by(s_bar).into_iter().map(|(s, _)| s).collect();
        assert_eq!(up, vec![s_foo], "bar called by foo");
        // Up: who calls foo? Nobody.
        assert!(ix.called_by(s_foo).is_empty(), "nobody calls foo");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn inheritance_substr_unions_multiple_syms() {
        // Two classes A and B both extend a common parent P. Asking
        // `super` with substr that matches both should return P once
        // (deduped), not twice.
        let path = tmp("inh_substr");
        let mut b = IndexBuilder::new();
        let s_a = sym_of("foo.bar.Aclass");
        let s_b = sym_of("foo.bar.Bclass");
        let s_p = sym_of("foo.bar.Parent");
        b.upsert_sym(s_a, kind::TYPE, lang::JAVA, "foo.bar.Aclass");
        b.upsert_sym(s_b, kind::TYPE, lang::JAVA, "foo.bar.Bclass");
        b.upsert_sym(s_p, kind::TYPE, lang::JAVA, "foo.bar.Parent");
        b.add_inherit(s_a, s_p);
        b.add_inherit(s_b, s_p);
        b.finish(&path).unwrap();

        let ix = Index::open(&path).unwrap();
        // Sanity: exact-match super of A returns P.
        assert_eq!(ix.inherits_of(s_a), vec![s_p]);
        // Two syms match "class" substring; their union of supertypes is just P.
        let hits = ix.syms_matching_substring("class", 16);
        assert_eq!(hits.len(), 2, "Aclass + Bclass both match 'class'");
        let mut all_supers: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for s in hits { all_supers.extend(ix.inherits_of(s)); }
        assert_eq!(all_supers, std::collections::HashSet::from([s_p]),
            "deduped supertype union");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn callgraph_multi_root_forest() {
        // Two distinct call chains, both seeded by a substring match.
        // `foo_a` → `helper_a`,  `foo_b` → `helper_b`. Asking
        // callgraph(--substr "foo_") --direction down --depth 1
        // should give a 4-node FOREST: id=0 (foo_a), id=1 (foo_b),
        // id=2 (helper_a, parent=0), id=3 (helper_b, parent=1).
        let path = tmp("cg_forest");
        let mut b = IndexBuilder::new();
        let foo_a = sym_of("foo_a");
        let foo_b = sym_of("foo_b");
        let h_a = sym_of("helper_a");
        let h_b = sym_of("helper_b");
        for (s, n) in [(foo_a,"foo_a"),(foo_b,"foo_b"),(h_a,"helper_a"),(h_b,"helper_b")] {
            b.upsert_sym(s, kind::FUNCTION, lang::CXX, n);
        }
        b.add_call(foo_a, h_a, role::CALL);
        b.add_call(foo_b, h_b, role::CALL);
        b.finish(&path).unwrap();

        let ix = Index::open(&path).unwrap();
        let roots = ix.syms_matching_substring("foo_", 16);
        assert_eq!(roots.len(), 2);
        // Reproduce the BFS-forest invariant we test in server.rs's
        // do_callgraph: each root has parent=None and unique id, each
        // child has parent = its discoverer.
        let mut visited: Vec<(u64, Option<u32>, u32)> = Vec::new();
        for &r in &roots {
            let id = visited.len() as u32;
            visited.push((r, None, id));
        }
        for i in 0..roots.len() {
            let (cur, _, cur_id) = visited[i];
            for (callee, _) in ix.calls_from(cur) {
                let id = visited.len() as u32;
                visited.push((callee, Some(cur_id), id));
            }
        }
        assert_eq!(visited.len(), 4, "2 roots + 2 callees = 4 nodes");
        assert_eq!(visited[0].1, None, "root has no parent");
        assert_eq!(visited[1].1, None, "second root has no parent");
        assert_eq!(visited[2].1, Some(0), "helper_a's parent is foo_a (id=0)");
        assert_eq!(visited[3].1, Some(1), "helper_b's parent is foo_b (id=1)");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn callgraph_many_callees_dedup() {
        // One caller, many callees — confirms dedup of duplicate edges
        // and that calls_from returns ALL distinct callees, not just one.
        let path = tmp("multi");
        let mut b = IndexBuilder::new();
        let caller = sym_of("caller");
        b.upsert_sym(caller, kind::FUNCTION, lang::JAVA, "caller");
        for i in 0..50 {
            let callee = sym_of(&format!("callee_{i}"));
            b.upsert_sym(callee, kind::FUNCTION, lang::JAVA, &format!("callee_{i}"));
            b.add_call(caller, callee, role::CALL);
            // Add duplicates that should dedup.
            b.add_call(caller, callee, role::CALL);
        }
        b.finish(&path).unwrap();
        let ix = Index::open(&path).unwrap();
        let callees = ix.calls_from(caller);
        assert_eq!(callees.len(), 50, "50 distinct callees (50 dups dropped)");
        // Up direction from one of them:
        let one = sym_of("callee_17");
        let up: Vec<_> = ix.called_by(one).into_iter().map(|(s, _)| s).collect();
        assert_eq!(up, vec![caller]);
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
