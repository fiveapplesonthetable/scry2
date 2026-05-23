//! Integration test: every query verb × every flag combination,
//! driven through the CLI subprocess (not just dispatch()) so we
//! catch clap parsing regressions too.
//!
//! Fixture: a tiny .s2db built in tmpdir with three Java types in
//! a parent/child hierarchy + a call edge + refs across files. One
//! file lives under `tests/`, the other under `core/` — gives us
//! material for `--in tests/` and `--not-in tests/`.

use scry2::{IndexBuilder, format::{kind, lang, role, sym_of}};
use std::path::PathBuf;
use std::process::Command;

fn scry2_bin() -> PathBuf {
    // cargo test sets CARGO_BIN_EXE_<name> for every bin target.
    PathBuf::from(env!("CARGO_BIN_EXE_scry2"))
}

/// Returns the path to a .s2db built fresh in a per-test-thread
/// directory. cargo test runs tests in parallel within one process,
/// so process::id() alone collides; thread::id() disambiguates.
fn make_index() -> (PathBuf, PathBuf) {
    let tid = format!("{:?}", std::thread::current().id());
    // ThreadId(N) → just "N" for filesystem cleanliness.
    let tid_num: String = tid.chars().filter(|c| c.is_ascii_digit()).collect();
    let dir = std::env::temp_dir().join(format!(
        "scry2-cli-test-{}-{}", std::process::id(), tid_num));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s2db = dir.join("test.s2db");

    let mut b = IndexBuilder::new();

    // Types: Parent (core), MainChild (core), TestChild (tests).
    let parent     = sym_of("foo.Parent");
    let main_child = sym_of("foo.MainChild");
    let test_child = sym_of("foo.TestChild");
    for (s, n) in [(parent, "foo.Parent"), (main_child, "foo.MainChild"),
                   (test_child, "foo.TestChild")] {
        b.upsert_sym(s, kind::TYPE, lang::JAVA, n);
    }

    // Files.
    b.upsert_file(1, "/aosp/frameworks/base/core/java/foo/Parent.java");
    b.upsert_file(2, "/aosp/frameworks/base/core/java/foo/MainChild.java");
    b.upsert_file(3, "/aosp/frameworks/base/core/tests/foo/TestChild.java");

    // Decls.
    b.add_xref(parent,     role::DECL, 1, 100);
    b.add_xref(main_child, role::DECL, 2, 100);
    b.add_xref(test_child, role::DECL, 3, 100);
    // Refs to Parent from MainChild (core) and TestChild (tests).
    b.add_xref(parent, role::REF, 2, 200);
    b.add_xref(parent, role::REF, 3, 200);

    // Inheritance.
    b.add_inherit(main_child, parent);
    b.add_inherit(test_child, parent);

    // Methods + calls for callers / callgraph testing.
    let m_caller = sym_of("foo.MainChild.run");
    let m_callee = sym_of("foo.Parent.helper");
    let m_test   = sym_of("foo.TestChild.runTest");
    for (s, n) in [(m_caller, "foo.MainChild.run"),
                   (m_callee, "foo.Parent.helper"),
                   (m_test,   "foo.TestChild.runTest")] {
        b.upsert_sym(s, kind::FUNCTION, lang::JAVA, n);
    }
    b.add_xref(m_caller, role::DECL, 2, 500);
    b.add_xref(m_callee, role::DECL, 1, 500);
    b.add_xref(m_test,   role::DECL, 3, 500);
    // m_test calls m_callee (test calls Parent.helper)
    // m_caller calls m_callee (main calls Parent.helper)
    b.add_call(m_caller, m_callee, role::CALL);
    b.add_call(m_test,   m_callee, role::CALL);
    // Call sites also get xrefs (for `callers` queries).
    b.add_xref(m_callee, role::CALL, 2, 600);  // call from MainChild
    b.add_xref(m_callee, role::CALL, 3, 600);  // call from TestChild

    // Membership (childof): Parent.helper is a member of Parent; add a
    // field too so `members foo.Parent` lists both.
    let f_count = sym_of("foo.Parent.count");
    b.upsert_sym(f_count, kind::FIELD, lang::JAVA, "foo.Parent.count");
    b.add_xref(f_count, role::DECL, 1, 700);
    b.add_childof(m_callee, parent);   // Parent.helper childof Parent
    b.add_childof(f_count,  parent);   // Parent.count  childof Parent

    // Signature (with param names) for one method.
    b.add_sig(m_callee, "void helper(int flags)");

    b.finish(&s2db).unwrap();
    (dir, s2db)
}

/// Run `scry2 --index INDEX --json <verb> ...` and return parsed JSON.
fn run(index: &std::path::Path, args: &[&str]) -> serde_json::Value {
    let bin = scry2_bin();
    let mut cmd = Command::new(&bin);
    cmd.arg("--index").arg(index).arg("--json");
    for a in args { cmd.arg(a); }
    let out = cmd.output().expect("spawn scry2");
    assert!(out.status.success(),
        "scry2 {args:?} failed:\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e|
        panic!("non-JSON output for {args:?}: {e}\n{stdout}"))
}

fn n_rows(v: &serde_json::Value) -> usize {
    // Different verbs put rows under different keys; we just count
    // members of the first non-null array we find.
    for key in ["groups", "hits", "nodes", "members"] {
        if let Some(arr) = v.get(key).and_then(|x| x.as_array()) {
            // For `groups`, count the rows across all groups.
            if key == "groups" {
                return arr.iter()
                    .filter_map(|g| g.get("rows").and_then(|x| x.as_array()))
                    .map(|r| r.len()).sum();
            }
            return arr.len();
        }
    }
    0
}

// -------- DEF -----------------------------------------------------------

#[test]
fn def_basic() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["def", "foo.Parent"]);
    assert!(n_rows(&v) >= 1, "{v}");
}

#[test]
fn def_substr() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["def", "Child", "--substr", "--limit", "10"]);
    assert!(n_rows(&v) >= 2, "Child matches MainChild + TestChild: {v}");
}

#[test]
fn def_in_filter_includes_core_only() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["def", "foo.Parent", "--in", "core/java/"]);
    assert!(n_rows(&v) >= 1, "{v}");
    let s = serde_json::to_string(&v).unwrap();
    assert!(!s.contains("/tests/"), "should not contain /tests/: {s}");
}

#[test]
fn def_not_in_drops_tests() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["def", "Child", "--substr", "--not-in", "tests/"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(!s.contains("TestChild"), "TestChild lives under tests/: {s}");
    assert!(s.contains("MainChild"), "MainChild is under core/: {s}");
}

// -------- REF -----------------------------------------------------------

#[test]
fn ref_basic() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["ref", "foo.Parent"]);
    assert!(n_rows(&v) >= 2, "Parent has refs in MainChild + TestChild: {v}");
}

#[test]
fn ref_in_filter() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["ref", "foo.Parent", "--in", "core/java/"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(!s.contains("/tests/"), "{s}");
}

#[test]
fn ref_max_hits_truncates() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["ref", "foo.Parent", "--max-hits", "1"]);
    assert!(n_rows(&v) <= 1, "max-hits=1 caps at 1: {v}");
    assert_eq!(v.get("truncated"), Some(&serde_json::Value::Bool(true)));
}

#[test]
fn ref_def_in_narrows() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["ref", "foo.Parent", "--def-in", "Parent.java"]);
    assert!(n_rows(&v) >= 2, "def-in matches Parent.java where Parent is defined: {v}");
}

// -------- CALLERS -------------------------------------------------------

#[test]
fn callers_basic() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["callers", "foo.Parent.helper"]);
    assert!(n_rows(&v) >= 2, "MainChild.run + TestChild.runTest both call helper: {v}");
}

#[test]
fn callers_in_filter() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["callers", "foo.Parent.helper", "--in", "core/java/"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(!s.contains("/tests/"), "{s}");
}

// -------- SUPER ---------------------------------------------------------

#[test]
fn super_basic() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["super", "foo.MainChild"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("foo.Parent"), "MainChild extends Parent: {s}");
}

#[test]
fn super_substr_unions_multiple() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["super", "Child", "--substr", "--limit", "10"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("foo.Parent"), "Parent is super of both Child types: {s}");
}

#[test]
fn super_hit_carries_def_location() {
    // #3: each inheritance hit resolves the related sym's def site to a
    // `path@off` locator. Parent decls at file 1 (Parent.java) off 100.
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["super", "foo.MainChild"]);
    let hits = v.get("hits").and_then(|x| x.as_array()).expect("hits array");
    let parent = hits.iter().find(|h|
        h.get("name").and_then(|n| n.as_str()) == Some("foo.Parent"))
        .expect("Parent hit present");
    let def = parent.get("def").and_then(|d| d.as_str())
        .expect("Parent hit carries a def locator");
    assert!(def.contains("Parent.java@100"),
            "def points at Parent's decl site: {def}");
}

// -------- SUB -----------------------------------------------------------

#[test]
fn sub_basic() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["sub", "foo.Parent"]);
    assert!(n_rows(&v) >= 2, "Parent has MainChild + TestChild subs: {v}");
}

#[test]
fn sub_in_filter_keeps_tests_only() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["sub", "foo.Parent", "--in", "tests/"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("TestChild"), "{s}");
    assert!(!s.contains("MainChild"), "MainChild is under core, should be filtered out: {s}");
}

#[test]
fn sub_not_in_drops_tests() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["sub", "foo.Parent", "--not-in", "tests/"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("MainChild"), "{s}");
    assert!(!s.contains("TestChild"), "TestChild filtered: {s}");
}

// -------- CALLGRAPH -----------------------------------------------------

#[test]
fn callgraph_up_finds_callers() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["callgraph", "foo.Parent.helper",
                         "--direction", "up", "--depth", "2"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("MainChild.run"), "{s}");
    assert!(s.contains("TestChild.runTest"), "{s}");
}

#[test]
fn callgraph_down_finds_callees() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["callgraph", "foo.MainChild.run",
                         "--direction", "down", "--depth", "2"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("foo.Parent.helper"), "{s}");
}

#[test]
fn callgraph_in_filter_drops_test_subtree() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["callgraph", "foo.Parent.helper",
                         "--direction", "up", "--depth", "2",
                         "--not-in", "tests/"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("MainChild.run"), "{s}");
    assert!(!s.contains("TestChild.runTest"), "TestChild filtered out: {s}");
}

#[test]
fn callgraph_substr_multi_root_forest() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["callgraph", "run", "--substr",
                         "--direction", "down", "--root-limit", "4"]);
    // Both MainChild.run and TestChild.runTest match "run" and seed
    // separate roots in the forest.
    let n_roots = v.get("nodes").and_then(|x| x.as_array()).map(|arr|
        arr.iter().filter(|n| n.get("parent").map(|p| p.is_null()).unwrap_or(false)).count()
    ).unwrap_or(0);
    assert!(n_roots >= 2, "multi-root forest expected: {v}");
}

#[test]
fn callgraph_def_in_narrows_roots() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["callgraph", "run", "--substr",
                         "--direction", "down", "--root-limit", "4",
                         "--def-in", "tests/"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("TestChild.runTest"), "TestChild root kept: {s}");
    assert!(!s.contains("MainChild.run"), "MainChild root filtered (not in tests/): {s}");
}

// -------- STAT ----------------------------------------------------------

#[test]
fn stat_reports_nonzero_counts() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["stat"]);
    assert!(v.get("xrefs").and_then(|x| x.as_u64()).unwrap() > 0);
    assert!(v.get("syms" ).and_then(|x| x.as_u64()).unwrap() > 0);
    assert!(v.get("files").and_then(|x| x.as_u64()).unwrap() > 0);
}

// -------- INHERITANCE ---------------------------------------------------

#[test]
fn inheritance_down_finds_subtypes() {
    // Parent <- MainChild, Parent <- TestChild. `down` from Parent
    // reaches both children.
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["inheritance", "foo.Parent", "--direction", "down"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("foo.MainChild"), "down reaches MainChild: {s}");
    assert!(s.contains("foo.TestChild"), "down reaches TestChild: {s}");
}

#[test]
fn inheritance_up_finds_supertypes() {
    // `up` from MainChild reaches Parent.
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["inheritance", "foo.MainChild", "--direction", "up"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("foo.Parent"), "up reaches Parent: {s}");
}

#[test]
fn inheritance_not_in_drops_test_subtree() {
    // down from Parent, but --not-in tests/ prunes TestChild.
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["inheritance", "foo.Parent", "--direction", "down",
                         "--not-in", "tests/"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("foo.MainChild"), "MainChild kept: {s}");
    assert!(!s.contains("foo.TestChild"), "TestChild filtered: {s}");
}

// -------- MEMBERS -------------------------------------------------------

#[test]
fn members_lists_class_members() {
    // foo.Parent has a method (helper) and a field (count) childof it.
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["members", "foo.Parent"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("foo.Parent.helper"), "lists method: {s}");
    assert!(s.contains("foo.Parent.count"),  "lists field: {s}");
    assert_eq!(n_rows(&v), 2, "exactly the two direct members: {v}");
}

#[test]
fn members_of_non_container_is_empty() {
    // Querying a function lists nothing (parent-kind filter).
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["members", "foo.Parent.helper"]);
    assert_eq!(n_rows(&v), 0, "a function is not a container: {v}");
}

// -------- SIG -----------------------------------------------------------

#[test]
fn sig_shows_full_signature_with_param_names() {
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["sig", "foo.Parent.helper"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("void helper(int flags)"),
        "sig carries param names: {s}");
}

#[test]
fn sig_absent_is_empty() {
    // A function we rendered no sig for prints nothing (honest emptiness).
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["sig", "foo.MainChild.run"]);
    assert_eq!(n_rows(&v), 0, "no sig rendered → no rows: {v}");
}

#[test]
fn def_surfaces_signature() {
    // `def` output includes the signature when one exists.
    let (_dir, s2db) = make_index();
    let v = run(&s2db, &["def", "foo.Parent.helper"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("void helper(int flags)"),
        "def carries the sig: {s}");
}

// -------- DEDUP (stub-jar copies) --------------------------------------

/// A .s2db with two STUB-JAR COPIES of one logical method: distinct
/// Kythe tickets that differ only in build-variant path but share the
/// same trailing VName signature (the indexer's per-element semantic id),
/// each carrying the identical rendered sig. Plus one genuinely distinct
/// overload (different VName signature). Exercises the query-side
/// `logical_key` dedup.
fn make_stub_dup_index() -> (PathBuf, PathBuf) {
    let tid = format!("{:?}", std::thread::current().id());
    let tid_num: String = tid.chars().filter(|c| c.is_ascii_digit()).collect();
    let dir = std::env::temp_dir().join(format!(
        "scry2-cli-dedup-{}-{}", std::process::id(), tid_num));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s2db = dir.join("test.s2db");

    let mut b = IndexBuilder::new();
    // Two stub copies: same trailing VName signature `SIGHASH`, different
    // path (so different ticket / different sym).
    let copy_a = sym_of("kythe:java:corpus#root#path/variantA/Bundle.java#SIGHASH");
    let copy_b = sym_of("kythe:java:corpus#root#path/variantB/Bundle.java#SIGHASH");
    b.upsert_sym(copy_a, kind::FUNCTION, lang::JAVA,
                 "kythe:java:corpus#root#path/variantA/Bundle.java#SIGHASH");
    b.upsert_sym(copy_b, kind::FUNCTION, lang::JAVA,
                 "kythe:java:corpus#root#path/variantB/Bundle.java#SIGHASH");
    // A genuinely distinct overload: different VName signature.
    let other = sym_of("kythe:java:corpus#root#path/Other.java#OTHERHASH");
    b.upsert_sym(other, kind::FUNCTION, lang::JAVA,
                 "kythe:java:corpus#root#path/Other.java#OTHERHASH");

    b.upsert_file(1, "/aosp/variantA/Bundle.java");
    b.upsert_file(2, "/aosp/variantB/Bundle.java");
    b.upsert_file(3, "/aosp/Other.java");
    b.add_xref(copy_a, role::DEF, 1, 10);
    b.add_xref(copy_b, role::DEF, 2, 20);
    b.add_xref(other,  role::DEF, 3, 30);

    // Identical rendered sig on both stub copies; distinct sig on `other`.
    b.add_sig(copy_a, "void putByteArray(java.lang.String key, byte[] value)");
    b.add_sig(copy_b, "void putByteArray(java.lang.String key, byte[] value)");
    b.add_sig(other,  "void putByteArray(java.lang.String key, int[] value)");

    b.finish(&s2db).unwrap();
    (dir, s2db)
}

#[test]
fn sig_dedups_stub_jar_copies() {
    // #5: the two stub copies (same VName signature, same rendered sig)
    // collapse to ONE; the genuine overload survives → 2 distinct hits.
    // `--substr` matches the ticket; both stub copies and the overload
    // share the `corpus#root` ticket prefix, so the substring reaches all
    // three syms and dedup acts on the rendered result.
    let (_dir, s2db) = make_stub_dup_index();
    let v = run(&s2db, &["sig", "corpus#root", "--substr", "--limit", "50"]);
    assert_eq!(n_rows(&v), 2,
        "two stub copies collapse to 1, plus the distinct overload: {v}");
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("byte[] value"), "{s}");
    assert!(s.contains("int[] value"), "distinct overload kept: {s}");
}

// -------- TRUNCATION NOTE (#G) -----------------------------------------

#[test]
fn def_truncation_prints_cap_reached_line() {
    // #G: when --limit caps the result, the human formatter (stderr)
    // spells out that more exist, and the JSON carries truncated=true.
    let (_dir, s2db) = make_index();
    // Run WITHOUT --json so we get the human cap line on stderr.
    let bin = scry2_bin();
    let out = Command::new(&bin)
        .arg("--index").arg(&s2db)
        .arg("def").arg("foo").arg("--substr").arg("--limit").arg("1")
        .output().expect("spawn scry2");
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--limit cap reached"),
        "cap-reached note expected on stderr: {err}");
    // And the JSON shape flags it too.
    let v = run(&s2db, &["def", "foo", "--substr", "--limit", "1"]);
    assert_eq!(v.get("truncated").and_then(|t| t.as_bool()), Some(true),
        "truncated flag set in JSON: {v}");
}

// -------- TILDE ---------------------------------------------------------

#[test]
fn tilde_expansion_on_index_path() {
    // We can't depend on the system having /home/$USER/scry2.s2db,
    // but we CAN confirm that --index ~/nonexistent yields an
    // "open index" error mentioning $HOME, not the literal "~".
    let bin = scry2_bin();
    let out = Command::new(&bin)
        .env("HOME", "/var/empty-test-home")
        .arg("--index").arg("~/nonexistent-scry2-index.s2db")
        .arg("stat")
        .output().expect("spawn");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "expected failure on nonexistent index");
    assert!(err.contains("/var/empty-test-home/"),
        "tilde must expand against $HOME: {err}");
    assert!(!err.contains("~/"), "literal ~/ must not appear: {err}");
}

// Cleanup is best-effort via tmpdir naming; the test process owns the dir.

#[test]
fn super_sub_work_on_method_overrides_not_just_types() {
    // `inh` carries method `overrides` as well as type `extends`. Regression
    // guard: super/sub must resolve a method's override edge — a kind filter
    // that kept only TYPE roots silently dropped every override query.
    let tid: String = format!("{:?}", std::thread::current().id())
        .chars().filter(|c| c.is_ascii_digit()).collect();
    let dir = std::env::temp_dir().join(format!("scry2-cli-ovr-{}-{}", std::process::id(), tid));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s2db = dir.join("ovr.s2db");
    let mut b = IndexBuilder::new();
    let parent = sym_of("p.Base.run");   // FUNCTION, overridden
    let child  = sym_of("p.Impl.run");   // FUNCTION, the override
    b.upsert_sym(parent, kind::FUNCTION, lang::JAVA, "p.Base.run");
    b.upsert_sym(child,  kind::FUNCTION, lang::JAVA, "p.Impl.run");
    b.add_inherit(child, parent);        // Impl.run overrides Base.run
    b.finish(&s2db).unwrap();
    // super(child) → what it overrides (inherits_of → inh).
    let up = run(&s2db, &["super", "p.Impl.run"]);
    assert!(up.to_string().contains("p.Base.run"), "super on a method override missing: {up}");
    // sub(parent) → what overrides it (inherited_by → inhrev).
    let down = run(&s2db, &["sub", "p.Base.run"]);
    assert!(down.to_string().contains("p.Impl.run"), "sub on a method override missing: {down}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn def_exact_ambiguous_name_aggregates_all_syms() {
    // A name shared by two syms — one bare (no xrefs), one carrying a def.
    // Exact `def NAME` must aggregate BOTH, not land on the bare sym and
    // return nothing (the regression: 12 stub-variant syms shared a Java
    // FQN; sym_for_name picked an xref-less one → hits=0).
    let tid: String = format!("{:?}", std::thread::current().id())
        .chars().filter(|c| c.is_ascii_digit()).collect();
    let dir = std::env::temp_dir().join(format!("scry2-cli-amb-{}-{}", std::process::id(), tid));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s2db = dir.join("amb.s2db");
    let mut b = IndexBuilder::new();
    let bare = sym_of("kythe:java:stub#a");   // shares the FQN, has no xrefs
    let defd = sym_of("kythe:java:real#b");   // shares the FQN, has the def
    b.upsert_sym(bare, kind::FUNCTION, lang::JAVA, "kythe:java:stub#a");
    b.upsert_sym(defd, kind::FUNCTION, lang::JAVA, "kythe:java:real#b");
    b.add_alias(bare, "p.Dup.method");
    b.add_alias(defd, "p.Dup.method");
    b.upsert_file(1, "core/java/p/Dup.java");
    b.add_xref(defd, role::DEF, 1, 100);
    b.finish(&s2db).unwrap();
    let v = run(&s2db, &["def", "p.Dup.method"]);
    assert!(v["total"].as_u64().unwrap_or(0) > 0, "exact def on ambiguous name found nothing: {v}");
    assert!(v.to_string().contains("Dup.java"), "the def-bearing variant missing: {v}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn def_substr_ignore_case_opt_in_only() {
    // `--substr` defaults to case-SENSITIVE (regression guard); `-i` /
    // `--ignore-case` opts into ASCII case folding. Exact-case `Example`
    // matches with --substr; lowercase `example` matches only WITH
    // --ignore-case, and misses without it.
    let tid: String = format!("{:?}", std::thread::current().id())
        .chars().filter(|c| c.is_ascii_digit()).collect();
    let dir = std::env::temp_dir().join(format!("scry2-cli-ci-{}-{}", std::process::id(), tid));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s2db = dir.join("ci.s2db");
    let mut b = IndexBuilder::new();
    let s = sym_of("com.Example.HandleRequest");
    b.upsert_sym(s, kind::FUNCTION, lang::JAVA, "com.Example.HandleRequest");
    b.upsert_file(1, "core/java/com/Example.java");
    b.add_xref(s, role::DEF, 1, 100);
    b.finish(&s2db).unwrap();

    // Exact-case substring matches under the default (case-sensitive) path.
    let v = run(&s2db, &["def", "Example", "--substr"]);
    assert!(n_rows(&v) >= 1, "exact-case 'Example' must match with --substr: {v}");

    // Lowercase needle matches only with --ignore-case.
    let v = run(&s2db, &["def", "example", "--substr", "--ignore-case"]);
    assert!(n_rows(&v) >= 1, "lowercase 'example' must match with --ignore-case: {v}");

    // The default --substr is case-sensitive: lowercase needle misses.
    let v = run(&s2db, &["def", "example", "--substr"]);
    assert_eq!(n_rows(&v), 0, "default --substr must be case-sensitive: {v}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Build a fixture modelling AOSP's stub-jar duplication for the
/// inheritance/callgraph verbs: ONE logical type `p.Hub` exists as several
/// distinct syms that all share the alias `p.Hub`, but the `inherits` /
/// `calls` edges live on only ONE of the dup syms (the others are
/// edge-less stub copies). `syms_for_name("p.Hub")` lands on the dups in
/// id order; the BFS must UNION edges across all of them or it returns
/// empty when the first dup happens to be edge-less. Returns (dir, s2db).
fn make_dup_hub_index() -> (PathBuf, PathBuf) {
    let tid: String = format!("{:?}", std::thread::current().id())
        .chars().filter(|c| c.is_ascii_digit()).collect();
    let dir = std::env::temp_dir().join(format!(
        "scry2-cli-hub-{}-{}", std::process::id(), tid));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s2db = dir.join("hub.s2db");
    let mut b = IndexBuilder::new();

    // Three edge-less stub copies of p.Hub + one copy that carries the
    // edges. Distinct tickets (so distinct syms), all aliased to "p.Hub".
    // The edge-less copies sort BEFORE the edge-bearing one by sym id so
    // the first-dup-only bug would surface as empty results.
    let stub1 = sym_of("kythe:java:c#r#variant1/Hub.java#HUBSIG");
    let stub2 = sym_of("kythe:java:c#r#variant2/Hub.java#HUBSIG");
    let real  = sym_of("kythe:java:c#r#variant3/Hub.java#HUBSIG");
    for (s, t) in [(stub1, "kythe:java:c#r#variant1/Hub.java#HUBSIG"),
                   (stub2, "kythe:java:c#r#variant2/Hub.java#HUBSIG"),
                   (real,  "kythe:java:c#r#variant3/Hub.java#HUBSIG")] {
        b.upsert_sym(s, kind::TYPE, lang::JAVA, t);
        b.add_alias(s, "p.Hub");
    }
    // Supertypes + subtypes, all on the `real` copy only.
    let base  = sym_of("p.Base");
    let iface = sym_of("p.Iface");
    let child = sym_of("p.Child");
    for (s, n) in [(base, "p.Base"), (iface, "p.Iface"), (child, "p.Child")] {
        b.upsert_sym(s, kind::TYPE, lang::JAVA, n);
    }
    b.upsert_file(1, "core/java/p/Base.java");
    b.upsert_file(2, "core/java/p/Iface.java");
    b.upsert_file(3, "core/java/p/Child.java");
    b.add_xref(base,  role::DECL, 1, 100);
    b.add_xref(iface, role::DECL, 2, 100);
    b.add_xref(child, role::DECL, 3, 100);
    b.add_inherit(real,  base);    // Hub extends Base
    b.add_inherit(real,  iface);   // Hub implements Iface
    b.add_inherit(child, real);    // Child extends Hub

    // Call edges, also only on the `real` copy: Hub.run calls + is called.
    // Model Hub itself (a method-shaped sym) as the dup; reuse the type
    // dups as call endpoints for simplicity by adding a function dup set.
    let fstub1 = sym_of("kythe:java:c#r#variant1/Hub.java#RUNSIG");
    let fstub2 = sym_of("kythe:java:c#r#variant2/Hub.java#RUNSIG");
    let freal  = sym_of("kythe:java:c#r#variant3/Hub.java#RUNSIG");
    for (s, t) in [(fstub1, "kythe:java:c#r#variant1/Hub.java#RUNSIG"),
                   (fstub2, "kythe:java:c#r#variant2/Hub.java#RUNSIG"),
                   (freal,  "kythe:java:c#r#variant3/Hub.java#RUNSIG")] {
        b.upsert_sym(s, kind::FUNCTION, lang::JAVA, t);
        b.add_alias(s, "p.Hub.run");
    }
    let caller = sym_of("p.App.main");
    let callee = sym_of("p.Helper.do");
    b.upsert_sym(caller, kind::FUNCTION, lang::JAVA, "p.App.main");
    b.upsert_sym(callee, kind::FUNCTION, lang::JAVA, "p.Helper.do");
    b.add_call(caller, freal, role::CALL);   // App.main calls Hub.run (up)
    b.add_call(freal, callee, role::CALL);   // Hub.run calls Helper.do (down)

    b.finish(&s2db).unwrap();
    (dir, s2db)
}

// -------- ISSUE 1: inheritance unions edges across dup syms -------------

#[test]
fn inheritance_up_unions_edges_across_dup_syms() {
    // #1 regression: `inheritance --direction up` returned EMPTY for hub
    // types because do_inheritance expanded only the first dup sym, which
    // was edge-less. The fix unions inherits edges across all dups, so up
    // from p.Hub must reach BOTH supertypes (Base + Iface) — matching what
    // `super` returns.
    let (dir, s2db) = make_dup_hub_index();
    let v = run(&s2db, &["inheritance", "p.Hub", "--direction", "up"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("p.Base"),  "up must reach Base across dups: {s}");
    assert!(s.contains("p.Iface"), "up must reach Iface across dups: {s}");
    // And it matches `super` (the union path that already worked).
    let sup = run(&s2db, &["super", "p.Hub"]).to_string();
    assert!(sup.contains("p.Base") && sup.contains("p.Iface"),
            "super baseline: {sup}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn inheritance_down_unions_edges_across_dup_syms() {
    // The same union must hold for `down`: subtypes live on one dup only.
    let (dir, s2db) = make_dup_hub_index();
    let v = run(&s2db, &["inheritance", "p.Hub", "--direction", "down"]);
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("p.Child"), "down must reach Child across dups: {s}");
    let _ = std::fs::remove_dir_all(&dir);
}

// -------- ISSUE 2: callgraph dedups dup roots, unions edges -------------

#[test]
fn callgraph_dedups_dup_roots_to_one_logical_root() {
    // #2 regression: `callgraph` emitted one parent=- root per dup sym
    // (~18 for a hub method), flooding the forest. The fix collapses dups
    // by logical key to ONE root and unions the call edges, so the callees
    // still surface.
    let (dir, s2db) = make_dup_hub_index();
    let v = run(&s2db, &["callgraph", "p.Hub.run", "--direction", "down", "--depth", "2"]);
    let nodes = v.get("nodes").and_then(|x| x.as_array()).expect("nodes");
    let n_roots = nodes.iter()
        .filter(|n| n.get("parent").map(|p| p.is_null()).unwrap_or(false))
        .count();
    assert_eq!(n_roots, 1, "dup roots collapse to one logical root: {v}");
    let s = serde_json::to_string(&v).unwrap();
    assert!(s.contains("p.Helper.do"), "callee surfaces via the union: {s}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn callgraph_up_unions_callers_across_dup_roots() {
    // The dedup must not drop callers that live on a sibling dup.
    let (dir, s2db) = make_dup_hub_index();
    let v = run(&s2db, &["callgraph", "p.Hub.run", "--direction", "up", "--depth", "2"]);
    let nodes = v.get("nodes").and_then(|x| x.as_array()).expect("nodes");
    let n_roots = nodes.iter()
        .filter(|n| n.get("parent").map(|p| p.is_null()).unwrap_or(false))
        .count();
    assert_eq!(n_roots, 1, "one logical root: {v}");
    assert!(v.to_string().contains("p.App.main"), "caller surfaces: {v}");
    let _ = std::fs::remove_dir_all(&dir);
}

// -------- ISSUE 3: ticket-shaped names get a readable fallback ----------

#[test]
fn sub_anonymous_subtype_renders_readable_fallback_not_ticket() {
    // #3 regression: anonymous/local subtypes carry only a raw `kythe:`
    // ticket as their name; `sub` leaked it. The fix renders an
    // `anon@<path>@<off>` (or trailing-sig) fallback — never a raw ticket
    // — in BOTH text and the --json `name` field.
    let tid: String = format!("{:?}", std::thread::current().id())
        .chars().filter(|c| c.is_ascii_digit()).collect();
    let dir = std::env::temp_dir().join(format!("scry2-cli-anon-{}-{}", std::process::id(), tid));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s2db = dir.join("anon.s2db");
    let mut b = IndexBuilder::new();
    let base = sym_of("p.Runnable");
    // An anonymous subtype with NO alias — its only name is the ticket.
    let anon = sym_of("kythe:java:c#r#Anon.java#ANONSIG");
    b.upsert_sym(base, kind::TYPE, lang::JAVA, "p.Runnable");
    b.upsert_sym(anon, kind::TYPE, lang::JAVA, "kythe:java:c#r#Anon.java#ANONSIG");
    b.upsert_file(1, "core/java/p/Anon.java");
    b.add_xref(anon, role::DEF, 1, 4242);   // gives the anon a def site
    b.add_inherit(anon, base);              // anon extends Runnable
    b.finish(&s2db).unwrap();

    // Text output: no raw ticket, and the anon@ locator is present.
    let bin = scry2_bin();
    let out = Command::new(&bin)
        .arg("--index").arg(&s2db).arg("sub").arg("p.Runnable")
        .output().expect("spawn");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("kythe:"), "raw ticket leaked in text: {text}");
    assert!(text.contains("anon@core/java/p/Anon.java@4242"),
            "anon fallback locator missing: {text}");

    // JSON name field also carries the fallback, never the raw ticket.
    let v = run(&s2db, &["sub", "p.Runnable"]);
    let hits = v.get("hits").and_then(|x| x.as_array()).expect("hits");
    let names: Vec<&str> = hits.iter()
        .filter_map(|h| h.get("name").and_then(|n| n.as_str())).collect();
    assert!(names.iter().all(|n| !n.starts_with("kythe:")),
            "raw ticket in --json name: {names:?}");
    assert!(names.iter().any(|n| n.starts_with("anon@")),
            "anon fallback missing in --json: {names:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// -------- ISSUE 4: names honors --json ---------------------------------

#[test]
fn names_honors_json_flag() {
    // #4 regression: `names --json` emitted plain text. It must emit the
    // structured Names reply that parses as JSON, with the sym rendered as
    // a 0x-hex string, and keep the plain-text shape unchanged otherwise.
    let (_dir, s2db) = make_index();
    // --json: valid JSON with the documented shape.
    let v = run(&s2db, &["names", "foo", "--json"]);
    assert_eq!(v.get("cmd").and_then(|c| c.as_str()), Some("names"), "{v}");
    let hits = v.get("hits").and_then(|x| x.as_array()).expect("hits array");
    assert!(!hits.is_empty(), "prefix foo matches names: {v}");
    let first = &hits[0];
    assert!(first.get("name").and_then(|n| n.as_str()).is_some(), "name field: {v}");
    let sym = first.get("sym").and_then(|s| s.as_str()).expect("sym hex string");
    assert!(sym.starts_with("0x"), "sym rendered as 0x-hex: {sym}");

    // Plain text still works (whitespace-separated `0x… name`).
    let bin = scry2_bin();
    let out = Command::new(&bin)
        .arg("--index").arg(&s2db).arg("names").arg("foo")
        .output().expect("spawn");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("0x") && text.contains("foo."),
            "plain-text names shape preserved: {text}");
}

// -------- ISSUE 7: SIGPIPE does not panic ------------------------------

#[test]
fn pipe_to_head_exits_without_panic() {
    // #7 regression: the Rust runtime masks SIGPIPE, so streaming long
    // output into a reader that closes early (`| head`) made println! hit
    // EPIPE and panic (exit 101). After resetting SIGPIPE to SIG_DFL the
    // process dies on the signal (exit 141 = 128+SIGPIPE) with NO panic.
    //
    // To trigger EPIPE deterministically we need scry2 to still be writing
    // when the reader closes, so the fixture has many ref rows (well past a
    // 64 KB pipe buffer). We spawn scry2 with a piped stdout, immediately
    // drop the read end, and wait: the write then fails mid-stream.
    use std::process::Stdio;
    let tid: String = format!("{:?}", std::thread::current().id())
        .chars().filter(|c| c.is_ascii_digit()).collect();
    let dir = std::env::temp_dir().join(format!("scry2-cli-pipe-{}-{}", std::process::id(), tid));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s2db = dir.join("pipe.s2db");
    let mut b = IndexBuilder::new();
    let s = sym_of("p.HotSym");
    b.upsert_sym(s, kind::FUNCTION, lang::JAVA, "p.HotSym");
    b.upsert_file(1, "core/java/p/Hot.java");
    // ~200k ref rows so the serialized output far exceeds any pipe buffer.
    for off in 0..200_000u32 { b.add_xref(s, role::REF, 1, off); }
    b.finish(&s2db).unwrap();

    let bin = scry2_bin();
    let mut child = Command::new(&bin)
        .arg("--index").arg(&s2db)
        .arg("ref").arg("p.HotSym").arg("--max-hits").arg("1000000")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn().expect("spawn scry2");
    // Close the read end of stdout immediately: the next large write from
    // scry2 hits a broken pipe.
    drop(child.stdout.take());
    let out = child.wait_with_output().expect("wait scry2");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked"),
            "scry2 must not panic on broken pipe (got SIGPIPE? exit {:?}): {err}",
            out.status);
    // Not the Rust panic exit code; SIGPIPE death (signal) or clean exit.
    assert_ne!(out.status.code(), Some(101),
            "broken-pipe panic regressed (exit 101): {err}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- from-kzip cache control flags ----
//
// These exercise the cache-policy validation, which fires at argument
// dispatch BEFORE any kzip read or indexer spawn — so they need no kzip,
// no indexer binaries, just the CLI. The kzip/kythe-root paths are dummies;
// the mutual-exclusion checks reject before either is touched.

fn from_kzip(extra: &[&str]) -> std::process::Output {
    Command::new(scry2_bin())
        .arg("from-kzip")
        .arg("--kzip").arg("/nonexistent/x.kzip")
        .arg("--kythe-root").arg("/nonexistent/kythe")
        .arg("--out").arg("/tmp/scry2-cache-flag-test.s2db")
        .args(extra)
        .output()
        .expect("spawn scry2 from-kzip")
}

#[test]
fn from_kzip_clean_and_no_cache_conflict() {
    let out = from_kzip(&["--clean", "--no-cache"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "--clean --no-cache must be rejected");
    assert!(err.contains("mutually exclusive"),
            "expected mutual-exclusion error, got: {err}");
}

#[test]
fn from_kzip_no_cache_and_cache_dir_conflict() {
    let out = from_kzip(&["--no-cache", "--cache-dir", "/tmp/whatever"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "--no-cache --cache-dir must be rejected");
    assert!(err.contains("mutually exclusive"),
            "expected mutual-exclusion error, got: {err}");
}

#[test]
fn from_kzip_flags_parse() {
    // A lone --no-cache (or --clean / --cache-dir) is accepted by the
    // parser and the mutual-exclusion validator; the run then fails later
    // because the dummy kzip can't be opened — but NOT with a clap usage
    // error and NOT with the mutual-exclusion error. (clap usage errors say
    // "unexpected argument" / "invalid value"; we only assert the flag is
    // recognized and not falsely rejected as conflicting.)
    for flag in [&["--no-cache"][..], &["--clean"][..], &["--cache-dir", "/tmp/c"][..]] {
        let out = from_kzip(flag);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!err.contains("mutually exclusive"),
                "{flag:?} wrongly flagged as conflicting: {err}");
        assert!(!err.contains("unexpected argument") && !err.contains("invalid value"),
                "{flag:?} should be a recognized flag, got: {err}");
    }
}
