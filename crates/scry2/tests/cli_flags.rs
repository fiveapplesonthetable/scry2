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
fn inheritance_substr_roots_filter_to_types() {
    // `--substr` resolves a substring to MANY syms; an inheritance/hierarchy
    // query must root only on TYPES, not same-named functions or the
    // type-application syms (`const(T)`, `T&`) a substring also matches.
    let tid: String = format!("{:?}", std::thread::current().id())
        .chars().filter(|c| c.is_ascii_digit()).collect();
    let dir = std::env::temp_dir().join(format!("scry2-cli-inh-{}-{}", std::process::id(), tid));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s2db = dir.join("inh.s2db");
    let mut b = IndexBuilder::new();
    let base = sym_of("z.Shape");            // TYPE
    let imp  = sym_of("z.ShapeImpl");        // TYPE, extends base
    let func = sym_of("z.ShapeHelper.run");  // FUNCTION, also contains "Shape"
    b.upsert_sym(base, kind::TYPE,     lang::JAVA, "z.Shape");
    b.upsert_sym(imp,  kind::TYPE,     lang::JAVA, "z.ShapeImpl");
    b.upsert_sym(func, kind::FUNCTION, lang::JAVA, "z.ShapeHelper.run");
    b.add_inherit(imp, base);                // ShapeImpl extends Shape
    b.finish(&s2db).unwrap();
    let v = run(&s2db, &["inheritance", "Shape", "--substr", "--direction", "down"]);
    let dump = v.to_string();
    assert!(dump.contains("z.Shape"), "a type root should be present: {dump}");
    assert!(!dump.contains("ShapeHelper"), "non-type leaked as a root: {dump}");
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
