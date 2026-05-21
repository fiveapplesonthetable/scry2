//! scry2 — a super-lean Kythe wrapper for AOSP. Single mmap'd index file,
//! microsecond-scale queries on the verbs an LLM uses to walk code:
//! `def`, `ref`, `callers`, `super`, `sub`.

pub mod format;
pub mod kythe;
pub mod kzip;
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
    fn merge_from_combines_two_workers() {
        // Per-worker builder shape: each worker has its own
        // IndexBuilder, the accumulator drains them via merge_from.
        // First-wins on syms/files; append-only on xrefs/calls/etc.
        let path = tmp("merge");
        let mut a = IndexBuilder::new();
        let mut b = IndexBuilder::new();
        let s_foo = sym_of("kythe:c++:test###foo");
        let s_bar = sym_of("kythe:c++:test###bar");
        a.upsert_sym(s_foo, kind::FUNCTION, lang::CXX, "foo");
        a.upsert_file(1, "/a/foo.cpp");
        a.add_xref(s_foo, role::DECL, 1, 100);
        a.add_alias(s_foo, "ns::foo");
        b.upsert_sym(s_bar, kind::FUNCTION, lang::CXX, "bar");
        b.upsert_file(2, "/b/bar.cpp");
        b.add_xref(s_bar, role::DECL, 2, 200);
        b.add_call(s_foo, s_bar, role::CALL);
        b.add_inherit(s_bar, s_foo);

        let mut acc = IndexBuilder::new();
        acc.merge_from(a);
        acc.merge_from(b);
        acc.finish(&path).unwrap();

        let ix = Index::open(&path).unwrap();
        assert_eq!(ix.sym_for_name("foo"), Some(s_foo));
        assert_eq!(ix.sym_for_name("bar"), Some(s_bar));
        assert_eq!(ix.sym_for_name("ns::foo"), Some(s_foo));
        assert_eq!(ix.file_path(1), Some("/a/foo.cpp"));
        assert_eq!(ix.file_path(2), Some("/b/bar.cpp"));
        assert_eq!(ix.inherits_of(s_bar), vec![s_foo]);
        let calls: Vec<_> = ix.calls_from(s_foo).into_iter().map(|(s,_)|s).collect();
        assert_eq!(calls, vec![s_bar]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn merge_from_first_wins_on_sym_metadata() {
        // Two workers see the same sym; the first one's kind/lang/name
        // wins, matching `upsert_sym`'s in-builder semantics.
        let path = tmp("merge-first");
        let s = sym_of("kythe:c++:test###same");
        let mut a = IndexBuilder::new();
        let mut b = IndexBuilder::new();
        a.upsert_sym(s, kind::FUNCTION, lang::CXX, "first");
        b.upsert_sym(s, kind::TYPE, lang::JAVA, "second");
        a.upsert_file(7, "/first/path.cpp");
        b.upsert_file(7, "/second/path.java");
        a.add_xref(s, role::DECL, 7, 1);
        a.finish(&path).unwrap();
        let mut acc = IndexBuilder::new();
        acc.merge_from(IndexBuilder::new());
        // a first, then b
        let mut a2 = IndexBuilder::new();
        a2.upsert_sym(s, kind::FUNCTION, lang::CXX, "first");
        a2.upsert_file(7, "/first/path.cpp");
        a2.add_xref(s, role::DECL, 7, 1);
        let mut b2 = IndexBuilder::new();
        b2.upsert_sym(s, kind::TYPE, lang::JAVA, "second");
        b2.upsert_file(7, "/second/path.java");
        acc.merge_from(a2);
        acc.merge_from(b2);
        acc.finish(&path).unwrap();
        let ix = Index::open(&path).unwrap();
        let (name, k, l) = ix.sym_meta(s).unwrap();
        assert_eq!(name, "first");
        assert_eq!(k, kind::FUNCTION);
        assert_eq!(l, lang::CXX);
        assert_eq!(ix.file_path(7), Some("/first/path.cpp"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn merge_refines_unk_kind_regardless_of_order() {
        // A sym referenced (kind::UNK) in one CU and defined (FUNCTION)
        // in another must end up FUNCTION no matter which sink drains
        // first — otherwise `def`/stat shows kind "?" and `index` vs
        // `from-kzip` diverge.
        let s = sym_of("kythe:c++:test###refine");
        for unk_first in [true, false] {
            let path = tmp(if unk_first { "refine-unk-first" } else { "refine-def-first" });
            let mut referenced = IndexBuilder::new();
            referenced.upsert_sym(s, kind::UNK, lang::UNK, "");
            let mut defined = IndexBuilder::new();
            defined.upsert_sym(s, kind::FUNCTION, lang::CXX, "ns::fn");
            let mut acc = IndexBuilder::new();
            if unk_first {
                acc.merge_from(referenced);
                acc.merge_from(defined);
            } else {
                acc.merge_from(defined);
                acc.merge_from(referenced);
            }
            acc.finish(&path).unwrap();
            let ix = Index::open(&path).unwrap();
            let (name, k, l) = ix.sym_meta(s).unwrap();
            assert_eq!(k, kind::FUNCTION, "unk_first={unk_first}");
            assert_eq!(l, lang::CXX, "unk_first={unk_first}");
            assert_eq!(name, "ns::fn", "unk_first={unk_first}");
            let _ = std::fs::remove_file(&path);
        }
    }

    #[test]
    fn name_index_tie_breaks_equal_names_by_sym() {
        // Two distinct syms with an identical qualified name must land in
        // the alphabetical index in a deterministic order (ascending sym),
        // not whatever the sort happened to leave — otherwise the same
        // name query can resolve differently across builds.
        let path = tmp("name-tiebreak");
        let lo = sym_of("kythe:c++:test###aaa");
        let hi = sym_of("kythe:c++:test###zzz");
        let (lo, hi) = (lo.min(hi), lo.max(hi));
        let mut b = IndexBuilder::new();
        // Insert high-id first so insertion order can't accidentally pass.
        b.upsert_sym(hi, kind::FUNCTION, lang::CXX, "dup::name");
        b.upsert_sym(lo, kind::FUNCTION, lang::CXX, "dup::name");
        b.add_xref(lo, role::DECL, 1, 1);
        b.add_xref(hi, role::DECL, 1, 2);
        b.finish(&path).unwrap();
        let ix = Index::open(&path).unwrap();
        let syms: Vec<u64> = ix.names_with_prefix("dup::name", 10)
            .into_iter().map(|(_, s)| s).collect();
        assert_eq!(syms, vec![lo, hi]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn resume_seeds_file_id_namespace_without_collision() {
        // Resume bug: a fresh FileIdAllocator restarts file ids at 0,
        // colliding with the prior shards' ids when the final merge
        // dedups file tables by id. seed_from must continue the prior
        // namespace: existing paths keep their id, new paths get ids
        // above the prior max.
        let path = tmp("fileid-seed");
        let mut b = IndexBuilder::new();
        b.upsert_file(0, "frameworks/base/.../Binder.java");
        b.upsert_file(1, "frameworks/base/.../Parcel.java");
        b.upsert_file(2, "frameworks/base/.../Context.java");
        let s = sym_of("kythe:java:x###s");
        b.upsert_sym(s, kind::FUNCTION, lang::JAVA, "X.s");
        b.add_xref(s, role::DECL, 1, 10);
        b.finish(&path).unwrap();
        let ix = Index::open(&path).unwrap();

        let alloc = crate::kythe::FileIdAllocator::default();
        alloc.seed_from(&ix);
        // Existing paths keep their prior ids.
        assert_eq!(alloc.intern("frameworks/base/.../Binder.java"), 0);
        assert_eq!(alloc.intern("frameworks/base/.../Parcel.java"), 1);
        // A new path gets an id ABOVE the seeded max — never colliding
        // with a seeded id (the bug was a new path reusing id 0).
        let nid = alloc.intern("frameworks/native/.../Other.cpp");
        assert!(nid >= 3, "new id {nid} collided with seeded 0..2");
        assert_eq!(alloc.intern("frameworks/native/.../Other.cpp"), nid);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn snapshot_then_resume_round_trips_all_rows() {
        // Populate a builder, snapshot to disk, open the snapshot as
        // an Index, replay it into a FRESH builder, then finish and
        // verify the resumed `.s2db` is point-for-point equivalent
        // to the original. This is the contract that `from-kzip
        // --resume` relies on.
        let snap_path = tmp("snap");
        let resumed_path = tmp("resumed");

        let mut b = IndexBuilder::new();
        let s_foo = sym_of("kythe:c++:test###foo");
        let s_bar = sym_of("kythe:c++:test###bar");
        b.upsert_sym(s_foo, kind::FUNCTION, lang::CXX, "kythe:c++:test###foo");
        b.upsert_sym(s_bar, kind::FUNCTION, lang::CXX, "kythe:c++:test###bar");
        b.add_alias(s_foo, "ns::foo");
        b.upsert_file(1, "/aosp/foo.cpp");
        b.add_xref(s_foo, role::DECL, 1, 100);
        b.add_xref(s_bar, role::REF,  1, 200);
        b.add_inherit(s_bar, s_foo);
        b.add_call(s_foo, s_bar, role::CALL);
        b.add_type(s_bar, "const Box<int> &");

        // Snapshot (non-consuming) — produces a usable .s2db.
        b.snapshot(&snap_path).unwrap();

        // Now resume: fresh builder + populate_from_index, then
        // finish to a new path.
        let ix = Index::open(&snap_path).unwrap();
        let mut resumed = IndexBuilder::new();
        resumed.populate_from_index(&ix).unwrap();
        resumed.finish(&resumed_path).unwrap();

        // Verify query parity on resumed index.
        let r = Index::open(&resumed_path).unwrap();
        assert_eq!(r.sym_for_name("kythe:c++:test###foo"), Some(s_foo));
        assert_eq!(r.sym_for_name("ns::foo"), Some(s_foo),
            "alias survives the snapshot/resume round-trip");
        let refs: Vec<_> = r.xrefs(s_foo, 0, 255).collect();
        assert_eq!(refs.len(), 1);
        assert_eq!(r.inherits_of(s_bar), vec![s_foo]);
        let calls: Vec<_> = r.calls_from(s_foo).into_iter().map(|(s,_)|s).collect();
        assert_eq!(calls, vec![s_bar]);
        assert_eq!(r.type_of(s_bar), Some("const Box<int> &"),
            "resolved type survives the snapshot/resume round-trip");
        assert_eq!(r.type_of(s_foo), None, "no typed edge → None after resume");

        let _ = std::fs::remove_file(&snap_path);
        let _ = std::fs::remove_file(&resumed_path);
    }

    #[test]
    fn streaming_merge_matches_reference_finish() {
        // The streaming-merge path takes a prior on-disk index plus an
        // in-memory delta and produces a new index byte-for-byte
        // equivalent (in query semantics) to one produced by feeding
        // the union of all rows through finish() in one shot. We
        // exercise every table — xrefs, syms, files, inherits, calls,
        // aliases — with both disjoint and overlapping content,
        // including a sym in both halves to check first-wins.
        let reference_path = tmp("merge-reference");
        let merged_path    = tmp("merge-streaming");
        let prior_path     = tmp("merge-prior");

        let s_a    = sym_of("kythe:c++:test###fnA");
        let s_b    = sym_of("kythe:c++:test###fnB");
        let s_c    = sym_of("kythe:c++:test###fnC");
        let s_iface = sym_of("kythe:c++:test###Iface");
        let s_impl  = sym_of("kythe:c++:test###Impl");
        let s_var   = sym_of("kythe:c++:test###gVar");

        let mut reference = IndexBuilder::new();
        reference.upsert_sym(s_a, kind::FUNCTION, lang::CXX, "kythe:c++:test###fnA");
        reference.upsert_sym(s_b, kind::FUNCTION, lang::CXX, "kythe:c++:test###fnB");
        reference.upsert_sym(s_c, kind::FUNCTION, lang::CXX, "kythe:c++:test###fnC");
        reference.upsert_sym(s_iface, kind::TYPE, lang::CXX, "kythe:c++:test###Iface");
        reference.upsert_sym(s_impl,  kind::TYPE, lang::CXX, "kythe:c++:test###Impl");
        reference.upsert_sym(s_var, kind::VARIABLE, lang::CXX, "kythe:c++:test###gVar");
        reference.add_alias(s_a, "ns::fnA");
        reference.add_alias(s_b, "ns::fnB");
        reference.add_alias(s_c, "ns::fnC");
        reference.add_alias(s_iface, "ns::Iface");
        reference.add_alias(s_var, "ns::gVar");           // suppressed (VARIABLE)
        reference.upsert_file(1, "/aosp/A.cpp");
        reference.upsert_file(2, "/aosp/B.cpp");
        reference.upsert_file(3, "/aosp/C.cpp");
        reference.add_xref(s_a, role::DECL, 1, 100);
        reference.add_xref(s_a, role::CALL, 2, 200);
        reference.add_xref(s_b, role::DEF,  2, 50);
        reference.add_xref(s_c, role::REF,  3, 300);
        reference.add_inherit(s_impl, s_iface);
        reference.add_call(s_a, s_b, role::CALL);
        reference.add_call(s_b, s_c, role::CALL);
        reference.add_call(s_a, s_c, role::REF);
        reference.add_type(s_a, "int");
        reference.add_type(s_var, "const Box<int> &");
        reference.finish(&reference_path).unwrap();

        // Streaming: split the same rows across prior + delta with
        // overlap on s_a (same kind), s_b (delta sees UNK — prior wins),
        // and duplicate alias + xref + call entries to exercise dedup.
        let mut prior_builder = IndexBuilder::new();
        prior_builder.upsert_sym(s_a, kind::FUNCTION, lang::CXX, "kythe:c++:test###fnA");
        prior_builder.upsert_sym(s_b, kind::FUNCTION, lang::CXX, "kythe:c++:test###fnB");
        prior_builder.upsert_sym(s_iface, kind::TYPE, lang::CXX, "kythe:c++:test###Iface");
        prior_builder.upsert_sym(s_var, kind::VARIABLE, lang::CXX, "kythe:c++:test###gVar");
        prior_builder.add_alias(s_a, "ns::fnA");
        prior_builder.add_alias(s_iface, "ns::Iface");
        prior_builder.add_alias(s_var, "ns::gVar");
        prior_builder.upsert_file(1, "/aosp/A.cpp");
        prior_builder.upsert_file(2, "/aosp/B.cpp");
        prior_builder.add_xref(s_a, role::DECL, 1, 100);
        prior_builder.add_xref(s_b, role::DEF,  2, 50);
        prior_builder.add_inherit(s_impl, s_iface);
        prior_builder.add_call(s_a, s_b, role::CALL);
        prior_builder.add_type(s_var, "const Box<int> &");   // var's type in prior
        prior_builder.finish(&prior_path).unwrap();

        let prior = Index::open(&prior_path).unwrap();
        let mut delta = IndexBuilder::new();
        delta.upsert_sym(s_a, kind::FUNCTION, lang::CXX, "kythe:c++:test###fnA");
        delta.upsert_sym(s_b, kind::UNK, lang::CXX, "kythe:c++:test###fnB");
        delta.upsert_sym(s_c, kind::FUNCTION, lang::CXX, "kythe:c++:test###fnC");
        delta.upsert_sym(s_impl, kind::TYPE, lang::CXX, "kythe:c++:test###Impl");
        delta.add_alias(s_a, "ns::fnA");                  // dup of prior
        delta.add_alias(s_b, "ns::fnB");
        delta.add_alias(s_c, "ns::fnC");
        delta.upsert_file(3, "/aosp/C.cpp");
        delta.add_xref(s_a, role::CALL, 2, 200);
        delta.add_xref(s_b, role::DEF,  2, 50);           // dup of prior
        delta.add_xref(s_c, role::REF,  3, 300);
        delta.add_call(s_a, s_b, role::CALL);             // dup of prior
        delta.add_call(s_b, s_c, role::CALL);
        delta.add_call(s_a, s_c, role::REF);
        delta.add_type(s_a, "int");                       // a's type in delta only
        delta.add_type(s_var, "const Box<int> &");        // dup of prior's type
        delta.write_merged_snapshot(&[&prior], &merged_path).unwrap();
        drop(prior);

        let r1 = Index::open(&reference_path).unwrap();
        let r2 = Index::open(&merged_path).unwrap();
        assert_eq!(r1.n_xrefs(), r2.n_xrefs(), "xref count diverges");
        assert_eq!(r1.n_syms(),  r2.n_syms(),  "sym count diverges");
        assert_eq!(r1.n_files(), r2.n_files(), "file count diverges");
        assert_eq!(r1.n_inh(),   r2.n_inh(),   "inherits count diverges");
        assert_eq!(r1.n_calls(), r2.n_calls(), "calls count diverges");
        assert_eq!(r1.n_names(), r2.n_names(), "names count diverges");
        for name in ["kythe:c++:test###fnA", "kythe:c++:test###fnB",
                     "kythe:c++:test###fnC", "kythe:c++:test###Iface",
                     "kythe:c++:test###Impl", "kythe:c++:test###gVar",
                     "ns::fnA", "ns::fnB", "ns::fnC", "ns::Iface"] {
            assert_eq!(r1.sym_for_name(name), r2.sym_for_name(name),
                "name '{name}' resolves differently between reference and streaming");
        }
        assert_eq!(r2.sym_for_name("ns::gVar"), None,
            "VARIABLE-kind alias should be suppressed in streaming merge");
        for sym in [s_a, s_b, s_c, s_iface, s_impl, s_var] {
            assert_eq!(r1.sym_meta(sym), r2.sym_meta(sym),
                "sym meta diverges for {sym:x}");
        }
        for sym in [s_a, s_b, s_c] {
            let x1: Vec<_> = r1.xrefs(sym, 0, u8::MAX).collect();
            let x2: Vec<_> = r2.xrefs(sym, 0, u8::MAX).collect();
            assert_eq!(x1, x2, "xrefs diverge for {sym:x}");
        }
        assert_eq!(r1.inherits_of(s_impl), r2.inherits_of(s_impl));
        assert_eq!(r1.inherited_by(s_iface), r2.inherited_by(s_iface));
        for caller in [s_a, s_b] {
            let c1: Vec<_> = r1.calls_from(caller).into_iter().collect();
            let c2: Vec<_> = r2.calls_from(caller).into_iter().collect();
            assert_eq!(c1, c2, "calls_from diverge for caller {caller:x}");
        }
        for f in 1u32..=3 {
            assert_eq!(r1.file_path(f), r2.file_path(f),
                "file_path({f}) diverges");
        }
        assert_eq!(r1.n_typed(), r2.n_typed(), "typed count diverges");
        for sym in [s_a, s_b, s_c, s_iface, s_impl, s_var] {
            assert_eq!(r1.type_of(sym), r2.type_of(sym),
                "type_of diverges for {sym:x}");
        }
        assert_eq!(r2.type_of(s_a), Some("int"), "a's type from delta");
        assert_eq!(r2.type_of(s_var), Some("const Box<int> &"), "var's type folded");
        assert_eq!(r2.type_of(s_b), None, "no typed edge for b");

        let _ = std::fs::remove_file(&reference_path);
        let _ = std::fs::remove_file(&merged_path);
        let _ = std::fs::remove_file(&prior_path);
    }

    #[test]
    fn streaming_merge_handles_empty_prior() {
        // No prior partial yet (first snapshot of a run). The merge
        // reduces to a single-source write, exercising the
        // `prior = None` arms of every helper.
        let direct_path  = tmp("merge-empty-prior-direct");
        let merged_path  = tmp("merge-empty-prior-streamed");

        let mk_data = || {
            let mut b = IndexBuilder::new();
            let s = sym_of("kythe:c++:t###fn");
            b.upsert_sym(s, kind::FUNCTION, lang::CXX, "kythe:c++:t###fn");
            b.add_alias(s, "ns::fn");
            b.upsert_file(1, "/x.cpp");
            b.add_xref(s, role::DECL, 1, 10);
            b.add_call(s, s, role::CALL);
            b
        };

        mk_data().finish(&direct_path).unwrap();
        mk_data().write_merged_snapshot(&[], &merged_path).unwrap();

        let r1 = Index::open(&direct_path).unwrap();
        let r2 = Index::open(&merged_path).unwrap();
        assert_eq!(r1.n_xrefs(), r2.n_xrefs());
        assert_eq!(r1.n_syms(),  r2.n_syms());
        assert_eq!(r1.n_names(), r2.n_names());
        assert_eq!(r2.sym_for_name("ns::fn"),
                   Some(sym_of("kythe:c++:t###fn")));

        let _ = std::fs::remove_file(&direct_path);
        let _ = std::fs::remove_file(&merged_path);
    }

    #[test]
    fn streaming_merge_chains_three_snapshots() {
        // Realistic snap cycle: snap1 = delta1 → partial1
        //                       snap2 = delta2 + partial1 → partial2
        //                       snap3 = delta3 + partial2 → partial3
        // Compare partial3 to a single-shot finish() of (delta1 ∪ delta2 ∪ delta3).
        let snap1 = tmp("chain-snap1");
        let snap2 = tmp("chain-snap2");
        let snap3 = tmp("chain-snap3");
        let reference = tmp("chain-reference");

        let s_a = sym_of("kythe:test###a");
        let s_b = sym_of("kythe:test###b");
        let s_c = sym_of("kythe:test###c");

        let mut d1 = IndexBuilder::new();
        d1.upsert_sym(s_a, kind::FUNCTION, lang::CXX, "kythe:test###a");
        d1.add_alias(s_a, "a_fqn");
        d1.upsert_file(1, "/a");
        d1.add_xref(s_a, role::DECL, 1, 1);
        d1.write_merged_snapshot(&[], &snap1).unwrap();

        let prior1 = Index::open(&snap1).unwrap();
        let mut d2 = IndexBuilder::new();
        d2.upsert_sym(s_b, kind::FUNCTION, lang::CXX, "kythe:test###b");
        d2.add_alias(s_b, "b_fqn");
        d2.add_alias(s_a, "a_alt");
        d2.upsert_file(2, "/b");
        d2.add_xref(s_b, role::DEF, 2, 2);
        d2.add_call(s_a, s_b, role::CALL);
        d2.write_merged_snapshot(&[&prior1], &snap2).unwrap();
        drop(prior1);

        let prior2 = Index::open(&snap2).unwrap();
        let mut d3 = IndexBuilder::new();
        d3.upsert_sym(s_c, kind::FUNCTION, lang::CXX, "kythe:test###c");
        d3.upsert_file(3, "/c");
        d3.add_xref(s_a, role::CALL, 3, 100);
        d3.add_xref(s_c, role::DECL, 3, 1);
        d3.add_inherit(s_b, s_a);
        d3.add_call(s_b, s_c, role::CALL);
        d3.write_merged_snapshot(&[&prior2], &snap3).unwrap();
        drop(prior2);

        let mut r = IndexBuilder::new();
        r.upsert_sym(s_a, kind::FUNCTION, lang::CXX, "kythe:test###a");
        r.upsert_sym(s_b, kind::FUNCTION, lang::CXX, "kythe:test###b");
        r.upsert_sym(s_c, kind::FUNCTION, lang::CXX, "kythe:test###c");
        r.add_alias(s_a, "a_fqn");
        r.add_alias(s_a, "a_alt");
        r.add_alias(s_b, "b_fqn");
        r.upsert_file(1, "/a");
        r.upsert_file(2, "/b");
        r.upsert_file(3, "/c");
        r.add_xref(s_a, role::DECL, 1, 1);
        r.add_xref(s_a, role::CALL, 3, 100);
        r.add_xref(s_b, role::DEF, 2, 2);
        r.add_xref(s_c, role::DECL, 3, 1);
        r.add_inherit(s_b, s_a);
        r.add_call(s_a, s_b, role::CALL);
        r.add_call(s_b, s_c, role::CALL);
        r.finish(&reference).unwrap();

        let chained = Index::open(&snap3).unwrap();
        let refidx  = Index::open(&reference).unwrap();
        assert_eq!(chained.n_xrefs(), refidx.n_xrefs());
        assert_eq!(chained.n_syms(),  refidx.n_syms());
        assert_eq!(chained.n_files(), refidx.n_files());
        assert_eq!(chained.n_names(), refidx.n_names());
        for name in ["kythe:test###a", "kythe:test###b", "kythe:test###c",
                     "a_fqn", "a_alt", "b_fqn"] {
            assert_eq!(chained.sym_for_name(name), refidx.sym_for_name(name),
                "name '{name}' diverges across 3-snap chain");
        }
        for sym in [s_a, s_b, s_c] {
            let x1: Vec<_> = chained.xrefs(sym, 0, u8::MAX).collect();
            let x2: Vec<_> = refidx.xrefs(sym, 0, u8::MAX).collect();
            assert_eq!(x1, x2, "xrefs diverge for {sym:x}");
        }
        let _ = std::fs::remove_file(&snap1);
        let _ = std::fs::remove_file(&snap2);
        let _ = std::fs::remove_file(&snap3);
        let _ = std::fs::remove_file(&reference);
    }

    #[test]
    fn kway_merge_matches_reference_finish() {
        // The final merge folds N shards + an in-memory remainder in ONE
        // k-way pass (write_merged_snapshot over many sources). It must be
        // identical to a single finish() over the union of every row:
        // dedup, UNK->known refine across shards, variable-kind alias
        // suppression, consistent file-id paths, and alpha name order all
        // preserved regardless of shard count. This is the gate for the
        // single-pass merge replacing the chained fold.
        let s0 = tmp("kway-shard0");
        let s1 = tmp("kway-shard1");
        let s2 = tmp("kway-shard2");
        let merged = tmp("kway-merged");
        let reference = tmp("kway-reference");

        let a = sym_of("kythe:c++:t###a");
        let b = sym_of("kythe:c++:t###b");
        let c = sym_of("kythe:c++:t###c");
        let d = sym_of("kythe:c++:t###d");
        let iface = sym_of("kythe:c++:t###Iface");
        let var = sym_of("kythe:c++:t###gVar");

        // shard0: a defined (FUNCTION), file 1, alias, call, a var sym+alias.
        // var carries a resolved type; a gets an EMPTY type here (must lose
        // to shard1's non-empty type on the tied-sym merge).
        let mut h0 = IndexBuilder::new();
        h0.upsert_sym(a, kind::FUNCTION, lang::CXX, "kythe:c++:t###a");
        h0.upsert_sym(var, kind::VARIABLE, lang::CXX, "kythe:c++:t###gVar");
        h0.add_alias(a, "ns::a");
        h0.add_alias(var, "ns::gVar");                 // must be suppressed
        h0.upsert_file(1, "/t/a.cpp");
        h0.add_xref(a, role::DEF, 1, 10);
        h0.add_call(a, b, role::CALL);
        h0.add_type(var, "const Box<int> &");          // var's resolved type
        h0.add_type(a, "");                            // empty: dropped by add_type
        h0.finish(&s0).unwrap();

        // shard1: a as UNK (must refine to shard0's FUNCTION), b defined,
        // file 2, dup alias + dup call across shards, a new alias.
        // a now gets a non-empty type (lands), b gets one too.
        let mut h1 = IndexBuilder::new();
        h1.upsert_sym(a, kind::UNK, lang::CXX, "");
        h1.upsert_sym(b, kind::FUNCTION, lang::CXX, "kythe:c++:t###b");
        h1.add_alias(a, "ns::a");                       // dup across shards
        h1.add_alias(b, "ns::b");
        h1.upsert_file(2, "/t/b.cpp");
        h1.add_xref(b, role::DEF, 2, 20);
        h1.add_call(a, b, role::CALL);                  // dup across shards
        h1.add_type(a, "int *");                        // a's resolved type
        h1.add_type(b, "Widget");
        h1.add_type(var, "const Box<int> &");           // dup of shard0's type
        h1.finish(&s1).unwrap();

        // shard2: c defined, inherit, dup of file 1 (same id -> same path),
        // a call, an xref. c carries a type.
        let mut h2 = IndexBuilder::new();
        h2.upsert_sym(c, kind::FUNCTION, lang::CXX, "kythe:c++:t###c");
        h2.add_alias(c, "ns::c");
        h2.upsert_file(1, "/t/a.cpp");                  // consistent file id
        h2.upsert_file(3, "/t/c.cpp");
        h2.add_xref(c, role::DECL, 3, 30);
        h2.add_inherit(b, iface);
        h2.add_call(b, c, role::CALL);
        h2.add_type(c, "java.util.List<java.lang.String>");
        h2.finish(&s2).unwrap();

        // remainder (in-memory delta, = `self`): d defined, file 4, a call,
        // d carries a type.
        let mut delta = IndexBuilder::new();
        delta.upsert_sym(d, kind::FUNCTION, lang::CXX, "kythe:c++:t###d");
        delta.add_alias(d, "ns::d");
        delta.upsert_file(4, "/t/d.cpp");
        delta.add_xref(d, role::DEF, 4, 40);
        delta.add_call(c, d, role::CALL);
        delta.add_type(d, "void(int, int)");

        let i0 = Index::open(&s0).unwrap();
        let i1 = Index::open(&s1).unwrap();
        let i2 = Index::open(&s2).unwrap();
        delta.write_merged_snapshot(&[&i0, &i1, &i2], &merged).unwrap();
        drop((i0, i1, i2));

        // Reference: one builder holding the deduped union of every row.
        let mut r = IndexBuilder::new();
        r.upsert_sym(a, kind::FUNCTION, lang::CXX, "kythe:c++:t###a");
        r.upsert_sym(b, kind::FUNCTION, lang::CXX, "kythe:c++:t###b");
        r.upsert_sym(c, kind::FUNCTION, lang::CXX, "kythe:c++:t###c");
        r.upsert_sym(d, kind::FUNCTION, lang::CXX, "kythe:c++:t###d");
        r.upsert_sym(var, kind::VARIABLE, lang::CXX, "kythe:c++:t###gVar");
        r.add_alias(a, "ns::a"); r.add_alias(b, "ns::b"); r.add_alias(c, "ns::c");
        r.add_alias(d, "ns::d"); r.add_alias(var, "ns::gVar");
        r.upsert_file(1, "/t/a.cpp"); r.upsert_file(2, "/t/b.cpp");
        r.upsert_file(3, "/t/c.cpp"); r.upsert_file(4, "/t/d.cpp");
        r.add_xref(a, role::DEF, 1, 10); r.add_xref(b, role::DEF, 2, 20);
        r.add_xref(c, role::DECL, 3, 30); r.add_xref(d, role::DEF, 4, 40);
        r.add_inherit(b, iface);
        r.add_call(a, b, role::CALL); r.add_call(b, c, role::CALL);
        r.add_call(c, d, role::CALL);
        r.add_type(a, "int *"); r.add_type(b, "Widget");
        r.add_type(c, "java.util.List<java.lang.String>");
        r.add_type(d, "void(int, int)");
        r.add_type(var, "const Box<int> &");
        r.finish(&reference).unwrap();

        let m  = Index::open(&merged).unwrap();
        let rf = Index::open(&reference).unwrap();
        assert_eq!(m.n_xrefs(), rf.n_xrefs(), "xref count");
        assert_eq!(m.n_syms(),  rf.n_syms(),  "sym count");
        assert_eq!(m.n_files(), rf.n_files(), "file count");
        assert_eq!(m.n_inh(),   rf.n_inh(),   "inh count");
        assert_eq!(m.n_calls(), rf.n_calls(), "call count");
        assert_eq!(m.n_names(), rf.n_names(), "name count");
        for name in ["kythe:c++:t###a", "kythe:c++:t###b", "kythe:c++:t###c",
                     "kythe:c++:t###d", "ns::a", "ns::b", "ns::c", "ns::d"] {
            assert_eq!(m.sym_for_name(name), rf.sym_for_name(name), "name '{name}'");
        }
        assert_eq!(m.sym_for_name("ns::gVar"), None, "var-kind alias suppressed");
        assert_eq!(m.sym_meta(a).map(|(_, k, _)| k), Some(kind::FUNCTION),
            "a must refine UNK->FUNCTION across shards");
        for sym in [a, b, c, d] {
            let x1: Vec<_> = m.xrefs(sym, 0, u8::MAX).collect();
            let x2: Vec<_> = rf.xrefs(sym, 0, u8::MAX).collect();
            assert_eq!(x1, x2, "xrefs diverge for {sym:x}");
            let c1: Vec<_> = m.calls_from(sym).into_iter().collect();
            let c2: Vec<_> = rf.calls_from(sym).into_iter().collect();
            assert_eq!(c1, c2, "calls_from diverge for {sym:x}");
        }
        assert_eq!(m.inherited_by(iface), rf.inherited_by(iface), "inherited_by");
        for f in 1u32..=4 {
            assert_eq!(m.file_path(f), rf.file_path(f), "file_path({f})");
        }

        // typed: the k-way merge must fold the typed tables across shards +
        // delta identically to a single finish().
        assert_eq!(m.n_typed(), rf.n_typed(), "typed count");
        for sym in [a, b, c, d, var] {
            assert_eq!(m.type_of(sym), rf.type_of(sym), "type_of diverges for {sym:x}");
        }
        // Spot the concrete expectations: a's empty type in shard0 lost to
        // shard1's non-empty one; every sym's stored type round-trips.
        assert_eq!(m.type_of(a), Some("int *"), "a's non-empty type wins over shard0's empty");
        assert_eq!(m.type_of(b), Some("Widget"));
        assert_eq!(m.type_of(c), Some("java.util.List<java.lang.String>"));
        assert_eq!(m.type_of(d), Some("void(int, int)"));
        assert_eq!(m.type_of(var), Some("const Box<int> &"));
        // A sym with no typed edge returns None in both.
        assert_eq!(m.type_of(iface), None);
        assert_eq!(rf.type_of(iface), None);

        for p in [&s0, &s1, &s2, &merged, &reference] {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn resume_file_ids_merge_to_correct_paths() {
        // End-to-end of the resume file-id fix. A run-1 shard interns
        // files with allocator A; a resumed run seeds a fresh allocator B
        // from that shard so it CONTINUES the id namespace (reused files
        // keep their id, new files get fresh non-colliding ids). After
        // merging both shards every sym must resolve to its OWN file.
        // Without the seed, B restarts at 0, the new file collides with a
        // run-1 id, and the merge's by-id file-table dedup points a run-1
        // sym at a run-2 path — the exact Binder.java->TunerDemuxInfo.cpp
        // corruption. This test fails without seed_from and passes with it.
        use crate::kythe::FileIdAllocator;
        let s0 = tmp("resume-shard0");
        let s1 = tmp("resume-shard1");
        let merged = tmp("resume-merged");

        let a = sym_of("kythe:c++:t###a");
        let z = sym_of("kythe:c++:t###z");

        // Run 1: a defined in a.cpp, declared in a shared common.h.
        let alloc_a = FileIdAllocator::default();
        let fa = alloc_a.intern("run1/a.cpp");
        let fc = alloc_a.intern("common/common.h");
        let mut h0 = IndexBuilder::new();
        h0.upsert_sym(a, kind::FUNCTION, lang::CXX, "kythe:c++:t###a");
        h0.add_xref(a, role::DEF, fa, 1);
        h0.add_xref(a, role::DECL, fc, 2);
        alloc_a.push_to(&mut h0);   // shard carries its file table
        h0.finish(&s0).unwrap();

        // Resume: fresh allocator seeded from shard0.
        let alloc_b = FileIdAllocator::default();
        {
            let i0 = Index::open(&s0).unwrap();
            alloc_b.seed_from(&i0);
        }
        let fc2 = alloc_b.intern("common/common.h");   // must reuse fc
        let fz  = alloc_b.intern("run2/z.cpp");         // must be fresh
        assert_eq!(fc2, fc, "resumed run must reuse common.h's id");
        assert_ne!(fz, fa, "new file collided with a run-1 id");
        assert_ne!(fz, fc, "new file collided with a run-1 id");
        let mut h1 = IndexBuilder::new();
        h1.upsert_sym(z, kind::FUNCTION, lang::CXX, "kythe:c++:t###z");
        h1.add_xref(z, role::DEF, fz, 3);
        h1.add_xref(z, role::DECL, fc2, 4);
        alloc_b.push_to(&mut h1);
        h1.finish(&s1).unwrap();

        let i0 = Index::open(&s0).unwrap();
        let i1 = Index::open(&s1).unwrap();
        IndexBuilder::new().write_merged_snapshot(&[&i0, &i1], &merged).unwrap();
        drop((i0, i1));

        let m = Index::open(&merged).unwrap();
        let paths = |sym| -> Vec<(u8, String)> {
            m.xrefs(sym, 0, u8::MAX)
                .map(|(_, r, f, _)| (r, m.file_path(f).unwrap_or("?").to_string()))
                .collect()
        };
        let ax = paths(a);
        assert!(ax.contains(&(role::DEF, "run1/a.cpp".into())), "a DEF: {ax:?}");
        assert!(ax.contains(&(role::DECL, "common/common.h".into())), "a DECL: {ax:?}");
        let zx = paths(z);
        assert!(zx.contains(&(role::DEF, "run2/z.cpp".into())), "z DEF: {zx:?}");
        assert!(zx.contains(&(role::DECL, "common/common.h".into())), "z DECL: {zx:?}");

        for p in [&s0, &s1, &merged] { let _ = std::fs::remove_file(p); }
    }

    #[test]
    fn alias_suppressed_for_variable_kind_syms() {
        // cxx_indexer emits /kythe/code on parameters too; the parsed
        // FQN `Method::param` would otherwise leak into the names table
        // and make `def Method --substr` return the parameter sym as
        // well. The writer must drop alias rows whose sym is kind=VARIABLE.
        //
        // Canonical names in real Kythe ingest are VName-style strings,
        // never the FQN — only the FQN comes through `add_alias`, which
        // is what this kind-suppression filter targets.
        let path = tmp("alias-var-supp");
        let mut b = IndexBuilder::new();
        let method_canon = "kythe:c++:android##Parcel.cpp#writeAligned";
        let param_canon  = "kythe:c++:android##Parcel.cpp#writeAligned#val";
        let method = sym_of(method_canon);
        let param  = sym_of(param_canon);
        b.upsert_sym(method, kind::FUNCTION, lang::CXX, method_canon);
        b.upsert_sym(param,  kind::VARIABLE, lang::CXX, param_canon);
        b.add_alias(method, "android::Parcel::writeAligned");
        b.add_alias(param,  "android::Parcel::writeAligned::val");
        b.upsert_file(1, "/aosp/Parcel.cpp");
        b.add_xref(method, role::DECL, 1, 100);
        b.finish(&path).unwrap();

        let ix = Index::open(&path).unwrap();
        // Method's FQN alias survives.
        assert_eq!(ix.sym_for_name("android::Parcel::writeAligned"), Some(method));
        // Parameter's FQN alias is suppressed; only canonical name resolves.
        assert_eq!(ix.sym_for_name("android::Parcel::writeAligned::val"), None,
            "variable-kind sym should have no FQN alias entry");
        assert_eq!(ix.sym_for_name(param_canon), Some(param),
            "canonical name still resolves the variable sym");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn alias_dedup_collapses_redundant_pairs() {
        // Real Kythe streams emit the same `/kythe/edge/named` alias on
        // every node that shares a MarkedSource — for AOSP methods that
        // means ~30 redundant adds per CU. The writer must collapse them
        // so the names table only carries one entry per (sym, alias),
        // not 30.
        let path = tmp("alias-dedup");
        let mut b = IndexBuilder::new();
        let canon = "kythe:c++:android##frameworks/native/.../Parcel.cpp#writeStrongBinder";
        let alias = "android::Parcel::writeStrongBinder";
        let s = sym_of(canon);
        b.upsert_sym(s, kind::FUNCTION, lang::CXX, canon);
        for _ in 0..30 { b.add_alias(s, alias); }
        b.upsert_file(1, "/aosp/Parcel.cpp");
        b.add_xref(s, role::DECL, 1, 100);
        b.finish(&path).unwrap();

        let ix = Index::open(&path).unwrap();
        // 1 canonical name + 1 deduped alias = 2 entries, not 31.
        assert_eq!(ix.n_names(), 2, "alias dedup: redundant pairs collapsed");
        assert_eq!(ix.sym_for_name(alias), Some(s));
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
    fn super_sub_callgraph_honor_path_filters() {
        // Two classes in /aosp/.../tests/ and two in /aosp/frameworks/base/.
        // Filter by --in / --not-in / --def-in and confirm the dispatch
        // path (server::dispatch → do_inh / do_callgraph) applies them.
        use server::{Request, dispatch};
        use reply::Reply;

        let path = tmp("filters");
        let mut b = IndexBuilder::new();
        let parent = sym_of("Pkg.Parent");
        let child_main = sym_of("Pkg.MainChild");
        let child_test = sym_of("Pkg.TestChild");
        for (s, n) in [(parent,"Pkg.Parent"),(child_main,"Pkg.MainChild"),
                       (child_test,"Pkg.TestChild")] {
            b.upsert_sym(s, kind::TYPE, lang::JAVA, n);
        }
        b.upsert_file(1, "/aosp/frameworks/base/core/java/Pkg/Parent.java");
        b.upsert_file(2, "/aosp/frameworks/base/core/java/Pkg/MainChild.java");
        b.upsert_file(3, "/aosp/frameworks/base/core/tests/Pkg/TestChild.java");
        b.add_xref(parent,     role::DECL, 1, 0);
        b.add_xref(child_main, role::DECL, 2, 0);
        b.add_xref(child_test, role::DECL, 3, 0);
        b.add_inherit(child_main, parent);
        b.add_inherit(child_test, parent);

        // Add a callgraph: MainChild → Parent → TestChild (so all 3
        // are reachable from MainChild walking down).
        b.add_call(child_main, parent,     role::CALL);
        b.add_call(parent,     child_test, role::CALL);
        b.finish(&path).unwrap();

        let ix = Index::open(&path).unwrap();

        // sub Parent --in tests/   →  only TestChild.
        let r = dispatch(&ix, &Request::Sub {
            name: "Pkg.Parent".into(), substr: false, limit: 16,
            in_: Some("tests/".into()), not_in: None,
        });
        if let Reply::Inh { hits, .. } = r {
            let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
            assert_eq!(names, vec!["Pkg.TestChild"], "--in tests/ kept only TestChild");
        } else { panic!("expected Reply::Inh") }

        // sub Parent --not-in tests/ → only MainChild.
        let r = dispatch(&ix, &Request::Sub {
            name: "Pkg.Parent".into(), substr: false, limit: 16,
            in_: None, not_in: Some("tests/".into()),
        });
        if let Reply::Inh { hits, .. } = r {
            let names: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
            assert_eq!(names, vec!["Pkg.MainChild"], "--not-in tests/ kept only MainChild");
        } else { panic!("expected Reply::Inh") }

        // callgraph MainChild --direction down --not-in tests/ →
        // root MainChild, then Parent (in frameworks/base/), but
        // NOT TestChild (in tests/).
        let r = dispatch(&ix, &Request::Callgraph {
            name: "Pkg.MainChild".into(), direction: "down".into(),
            depth: 3, max_syms: 200, substr: false, root_limit: 16,
            in_: None, not_in: Some("tests/".into()), def_in: None,
        });
        if let Reply::Callgraph { nodes, .. } = r {
            let names: Vec<&str> = nodes.iter().map(|n| n.name.as_str()).collect();
            assert_eq!(names, vec!["Pkg.MainChild", "Pkg.Parent"],
                "BFS stops at Parent — TestChild filtered out");
        } else { panic!("expected Reply::Callgraph") }

        // callgraph --substr "Child" --def-in tests/ →
        // only TestChild seeds (root filter), then expand down (no
        // children of TestChild defined).
        let r = dispatch(&ix, &Request::Callgraph {
            name: "Child".into(), direction: "down".into(),
            depth: 3, max_syms: 200, substr: true, root_limit: 16,
            in_: None, not_in: None, def_in: Some("tests/".into()),
        });
        if let Reply::Callgraph { nodes, .. } = r {
            let root_names: Vec<&str> = nodes.iter()
                .filter(|n| n.parent.is_none()).map(|n| n.name.as_str()).collect();
            assert_eq!(root_names, vec!["Pkg.TestChild"],
                "--def-in tests/ filtered seed roots to TestChild only");
        } else { panic!("expected Reply::Callgraph") }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ref_def_in_empty_string_is_noop() {
        // Conservative semantics: --def-in="" should match every sym,
        // not reject all of them. Mirrors scry's path_matches behavior.
        use server::{Request, dispatch};
        use reply::Reply;
        let path = tmp("def_in_empty");
        let mut b = IndexBuilder::new();
        let s_foo = sym_of("foo");
        b.upsert_sym(s_foo, kind::FUNCTION, lang::CXX, "foo");
        b.upsert_file(1, "/aosp/x.cpp");
        b.add_xref(s_foo, role::DECL, 1, 100);
        b.add_xref(s_foo, role::REF,  1, 200);
        b.finish(&path).unwrap();
        let ix = Index::open(&path).unwrap();
        let r = dispatch(&ix, &Request::Ref {
            name: "foo".into(), substr: false, limit: 16, max_hits: 200,
            in_: Some("".into()), not_in: Some("".into()), def_in: Some("".into()),
        });
        if let Reply::Xrefs { total, .. } = r {
            assert!(total >= 1, "empty filters must not reject anything");
        } else { panic!("expected Reply::Xrefs") }
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
