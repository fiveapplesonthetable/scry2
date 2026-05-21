//! Long-lived daemon: opens the .s2db mmap once, listens on a Unix
//! domain socket, serves line-delimited JSON requests. Clients eat
//! ~10 ms process-startup once; subsequent queries land in
//! microseconds.
//!
//! ## Protocol
//!
//! Both directions are **one JSON object per line** terminated by `\n`.
//!
//! Request:
//! ```json
//! {"cmd":"def","name":"foo","substr":true,"limit":16,"in":"src/","not_in":"test/","def_in":null}
//! {"cmd":"ref","name":"foo","substr":false,"limit":16,"max_hits":200, ...}
//! {"cmd":"callers","name":"foo", ...}
//! {"cmd":"super","name":"foo"}
//! {"cmd":"sub","name":"foo"}
//! {"cmd":"callgraph","name":"foo","direction":"up","depth":3,"max_syms":200}
//! {"cmd":"stat"}
//! ```
//!
//! Response is one of the `Reply` shapes from `reply.rs`. The wire
//! shape is exactly what `--json` emits — call sites do not branch.

use crate::format::role;
use crate::reader::Index;
use crate::reply::{CallNode, InhHit, MemberHit, Reply, SigHit, SymbolGroup,
                   TypeHit, XrefHit, kind_str, lang_str, role_str};
use crate::format::kind;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// Wire-format request envelope. One JSON object per request line.
/// Same shape for the socket daemon and the stdin/stdout REPL.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Stat,
    Def { name: String, #[serde(default)] substr: bool, #[serde(default = "lim16")] limit: usize,
          #[serde(default, rename = "in")]  in_:    Option<String>,
          #[serde(default)] not_in: Option<String> },
    Type { name: String, #[serde(default)] substr: bool,
           #[serde(default = "lim16")] limit: usize },
    Sig  { name: String, #[serde(default)] substr: bool,
           #[serde(default = "lim16")] limit: usize },
    Members { name: String, #[serde(default)] substr: bool,
              #[serde(default = "lim16")] limit: usize },
    Ref { name: String, #[serde(default)] substr: bool, #[serde(default = "lim16")] limit: usize,
          #[serde(default = "lim_max_hits")] max_hits: usize,
          #[serde(default, rename = "in")] in_: Option<String>,
          #[serde(default)] not_in: Option<String>,
          #[serde(default)] def_in: Option<String> },
    Callers { name: String, #[serde(default)] substr: bool, #[serde(default = "lim16")] limit: usize,
              #[serde(default = "lim_max_hits")] max_hits: usize,
              #[serde(default, rename = "in")] in_: Option<String>,
              #[serde(default)] not_in: Option<String>,
              #[serde(default)] def_in: Option<String> },
    Super { name: String, #[serde(default)] substr: bool,
            #[serde(default = "lim16")] limit: usize,
            #[serde(default, rename = "in")] in_: Option<String>,
            #[serde(default)] not_in: Option<String> },
    Sub   { name: String, #[serde(default)] substr: bool,
            #[serde(default = "lim16")] limit: usize,
            #[serde(default, rename = "in")] in_: Option<String>,
            #[serde(default)] not_in: Option<String> },
    Callgraph { name: String,
                #[serde(default = "default_direction")] direction: String,
                #[serde(default = "default_depth")] depth: usize,
                #[serde(default = "default_max_syms")] max_syms: usize,
                /// When true, `name` is matched as a substring against
                /// every sym; each match becomes a root in the output
                /// forest. parent=None marks each root; ids are
                /// unique across the whole reply.
                #[serde(default)] substr: bool,
                /// When `substr` is true, cap how many roots seed the
                /// BFS. Default 16. Cheap roots are fine — the BFS
                /// dedupes downstream discoveries via the `seen` set.
                #[serde(default = "default_root_limit")] root_limit: usize,
                /// Restrict every discovered sym by def-file path.
                #[serde(default, rename = "in")] in_: Option<String>,
                /// Drop every discovered sym by def-file path.
                #[serde(default)] not_in: Option<String>,
                /// Root-level only: drop seed roots whose def-file
                /// path doesn't contain SUBSTR. Matches scry semantics.
                #[serde(default)] def_in: Option<String> },
    /// Transitive inheritance walk — the inheritance-graph analogue of
    /// `callgraph`. up = supertypes (walk `inh`: child→parent), down =
    /// subtypes (walk `inhrev`: parent→child), both = union. Same
    /// BFS-forest reply shape as `callgraph`.
    Inheritance { name: String,
                  #[serde(default = "default_direction")] direction: String,
                  #[serde(default = "default_depth")] depth: usize,
                  #[serde(default = "default_max_syms")] max_syms: usize,
                  #[serde(default)] substr: bool,
                  #[serde(default = "default_root_limit")] root_limit: usize,
                  #[serde(default, rename = "in")] in_: Option<String>,
                  #[serde(default)] not_in: Option<String>,
                  #[serde(default)] def_in: Option<String> },
}
fn default_root_limit() -> usize { 16 }
fn lim16() -> usize { 16 }
fn lim_max_hits() -> usize { 200 }
fn default_direction() -> String { "up".into() }
fn default_depth() -> usize { 3 }
fn default_max_syms() -> usize { 200 }

/// Path-substring filter shared by every query verb. Matches on
/// Kythe's stored `path` field with no normalization (see
/// docs/USAGE.md for path semantics).
///
/// Empty-string filters are no-ops: `Some("")` for `in_` matches
/// everything, `Some("")` for `not_in` rejects nothing. Matches what
/// callers expect when an upstream pipes Option<String> through from
/// the CLI without trimming.
#[derive(Debug, Default, Clone, Copy)]
struct PathFilter<'a> {
    /// File path of the ref/call site must contain this.
    pub in_: Option<&'a str>,
    /// File path of the ref/call site must NOT contain this.
    pub not_in: Option<&'a str>,
    /// Target symbol's decl/def path must contain this — used to
    /// drop matches whose definitions live outside a subtree of interest.
    pub def_in: Option<&'a str>,
}

impl PathFilter<'_> {
    /// One canonical `--in` / `--not-in` check. Used by every query
    /// verb that path-filters its result rows or BFS frontier — never
    /// inline this logic at a call site.
    pub fn passes(&self, path: &str) -> bool {
        if let Some(s) = self.in_ {
            if !s.is_empty() && !path.contains(s) { return false; }
        }
        if let Some(s) = self.not_in {
            if !s.is_empty() && path.contains(s) { return false; }
        }
        true
    }

    /// True iff at least one of `--in` / `--not-in` would actually
    /// reject a path. Lets hot paths skip the path lookup entirely
    /// when neither filter is set (or both are empty strings).
    pub fn has_in_out(&self) -> bool {
        self.in_.is_some_and(|s| !s.is_empty())
            || self.not_in.is_some_and(|s| !s.is_empty())
    }
}

/// Run a single request against a borrowed Index and return its Reply.
/// The pure-function shape means the daemon and the in-process CLI
/// share one code path.
pub fn dispatch(ix: &Index, req: &Request) -> Reply {
    match req {
        Request::Stat => Reply::Stat {
            xrefs: ix.n_xrefs(),
            syms:  ix.n_syms(),
            files: ix.n_files(),
            inhs:  ix.n_inh(),
            calls: ix.n_calls(),
        },
        Request::Def { name, substr, limit, in_, not_in } => do_xrefs(
            ix, name, *substr, *limit, (role::DECL, role::DEF), usize::MAX,
            PathFilter { in_: in_.as_deref(), not_in: not_in.as_deref(), def_in: None },
            /*with_type=*/true,
        ),
        Request::Type { name, substr, limit } => do_type(ix, name, *substr, *limit),
        Request::Sig { name, substr, limit } => do_sig(ix, name, *substr, *limit),
        Request::Members { name, substr, limit } => do_members(ix, name, *substr, *limit),
        // ref/callers with --substr must aggregate edges across *all*
        // name matches, not just the first `--limit` symbols: capping the
        // symbol set at 16 made `callers clearCallingIdentity --substr`
        // return 0 because the 16 alpha-first matches were Java stubs with
        // no call edges while the C++ definition (which has the callers)
        // sorted later. Gather broadly; the output is bounded by max_hits.
        Request::Ref { name, substr, limit, max_hits, in_, not_in, def_in } => do_xrefs(
            ix, name, *substr, (*limit).max(SUBSTR_AGG_CAP), (0, u8::MAX), *max_hits,
            PathFilter { in_: in_.as_deref(), not_in: not_in.as_deref(), def_in: def_in.as_deref() },
            /*with_type=*/false,
        ),
        Request::Callers { name, substr, limit, max_hits, in_, not_in, def_in } => do_xrefs(
            ix, name, *substr, (*limit).max(SUBSTR_AGG_CAP), (role::CALL, role::CALL), *max_hits,
            PathFilter { in_: in_.as_deref(), not_in: not_in.as_deref(), def_in: def_in.as_deref() },
            /*with_type=*/false,
        ),
        Request::Super { name, substr, limit, in_, not_in } => do_inh(
            ix, name, *substr, *limit, /*sub=*/false,
            PathFilter { in_: in_.as_deref(), not_in: not_in.as_deref(), def_in: None },
        ),
        Request::Sub   { name, substr, limit, in_, not_in } => do_inh(
            ix, name, *substr, *limit, /*sub=*/true,
            PathFilter { in_: in_.as_deref(), not_in: not_in.as_deref(), def_in: None },
        ),
        Request::Callgraph { name, direction, depth, max_syms, substr, root_limit,
                             in_, not_in, def_in } => do_callgraph(
            ix, name, direction, *depth, *max_syms, *substr, *root_limit,
            PathFilter { in_: in_.as_deref(), not_in: not_in.as_deref(), def_in: def_in.as_deref() },
        ),
        Request::Inheritance { name, direction, depth, max_syms, substr, root_limit,
                               in_, not_in, def_in } => do_inheritance(
            ix, name, direction, *depth, *max_syms, *substr, *root_limit,
            PathFilter { in_: in_.as_deref(), not_in: not_in.as_deref(), def_in: def_in.as_deref() },
        ),
    }
}

/// Symbol-set cap for `ref`/`callers --substr`: gather edges across this
/// many name matches before relying on `max_hits` to bound the output.
/// Large enough that an ambiguous leaf (`clearCallingIdentity`) still
/// reaches the definition that actually carries the edges.
const SUBSTR_AGG_CAP: usize = 4096;

#[allow(clippy::too_many_arguments)]
fn do_xrefs(
    ix: &Index, name: &str, substr: bool, sym_cap: usize,
    roles: (u8, u8), max_hits: usize,
    filt: PathFilter<'_>,
    with_type: bool,
) -> Reply {
    let (role_lo, role_hi) = roles;
    let syms: Vec<u64> = if substr {
        ix.syms_matching_substring(name, sym_cap)
    } else if let Some(s) = ix.sym_for_name(name) { vec![s] } else { Vec::new() };
    let mut groups: Vec<SymbolGroup> = Vec::new();
    let mut total = 0usize;
    // If the substring matched as many symbols as we were willing to
    // gather, there may be more we didn't reach — flag it so callers know
    // the result is a (possibly partial) aggregate, not the whole truth.
    let mut truncated = substr && syms.len() >= sym_cap;
    'outer: for sym in &syms {
        if let Some(needle) = filt.def_in {
            if !needle.is_empty() {
                let def_ok = ix.sym_def_path(*sym).is_some_and(|p| p.contains(needle));
                if !def_ok { continue; }
            }
        }
        let (sname, knd, lng) = ix.sym_meta(*sym).unwrap_or(("?", 0, 0));
        let typed = if with_type {
            ix.type_of(*sym).map(str::to_string)
        } else { None };
        let sig = if with_type {
            ix.sig_of(*sym).map(str::to_string)
        } else { None };
        let mut rows: Vec<XrefHit> = Vec::new();
        for (_, r, file, off) in ix.xrefs(*sym, role_lo, role_hi) {
            let path = ix.file_path(file).unwrap_or("?");
            if !filt.passes(path) { continue; }
            rows.push(XrefHit { role: role_str(r).to_string(), file: path.to_string(), off });
            total += 1;
            if total >= max_hits {
                truncated = true;
                groups.push(SymbolGroup {
                    name: sname.to_string(),
                    kind: kind_str(knd).to_string(),
                    lang: lang_str(lng).to_string(),
                    typed,
                    sig,
                    rows,
                });
                break 'outer;
            }
        }
        if !rows.is_empty() {
            groups.push(SymbolGroup {
                name: sname.to_string(),
                kind: kind_str(knd).to_string(),
                lang: lang_str(lng).to_string(),
                typed,
                sig,
                rows,
            });
        }
    }
    Reply::Xrefs { groups, total, truncated }
}

/// `type NAME` — the resolved type of a symbol. Resolves NAME → sym via
/// the same exact / `--substr` path the other verbs use, then reads the
/// `typed` section. Symbols with no resolved type are dropped (honest
/// emptiness), so the result holds only the syms that actually carry one.
fn do_type(ix: &Index, name: &str, substr: bool, limit: usize) -> Reply {
    let syms: Vec<u64> = if substr {
        ix.syms_matching_substring(name, limit.max(SUBSTR_AGG_CAP))
    } else if let Some(s) = ix.sym_for_name(name) { vec![s] } else { Vec::new() };
    let mut hits: Vec<TypeHit> = Vec::new();
    for sym in &syms {
        let Some(ty) = ix.type_of(*sym) else { continue };
        let (sname, knd, lng) = ix.sym_meta(*sym).unwrap_or(("?", 0, 0));
        hits.push(TypeHit {
            name: sname.to_string(),
            kind: kind_str(knd).to_string(),
            lang: lang_str(lng).to_string(),
            typed: ty.to_string(),
        });
        if hits.len() >= limit { break; }
    }
    let truncated = hits.len() >= limit;
    let total = hits.len();
    Reply::Type { hits, total, truncated }
}

/// `sig NAME` — a symbol's full rendered signature with parameter names.
/// Resolves NAME → sym via the same exact / `--substr` path, then reads
/// the `sig` section. Symbols with no rendered signature are dropped
/// (honest emptiness), so the result holds only the syms that carry one.
fn do_sig(ix: &Index, name: &str, substr: bool, limit: usize) -> Reply {
    let syms: Vec<u64> = if substr {
        ix.syms_matching_substring(name, limit.max(SUBSTR_AGG_CAP))
    } else if let Some(s) = ix.sym_for_name(name) { vec![s] } else { Vec::new() };
    let mut hits: Vec<SigHit> = Vec::new();
    for sym in &syms {
        let Some(sg) = ix.sig_of(*sym) else { continue };
        let (sname, knd, lng) = ix.sym_meta(*sym).unwrap_or(("?", 0, 0));
        hits.push(SigHit {
            name: sname.to_string(),
            kind: kind_str(knd).to_string(),
            lang: lang_str(lng).to_string(),
            sig:  sg.to_string(),
        });
        if hits.len() >= limit { break; }
    }
    let truncated = hits.len() >= limit;
    let total = hits.len();
    Reply::Sig { hits, total, truncated }
}

/// `members NAME` — the direct members of a container (a class's fields
/// and methods, a package's types). Resolves NAME → sym, then lists the
/// `childrev` rows for that sym. The childrev table holds every
/// `/kythe/edge/childof` edge, so we filter HERE by the container sym's
/// kind: only a type / record / interface / package expands. That keeps
/// function-local children (params/locals childof a function) out of the
/// result without a separate ingest-time filter.
fn do_members(ix: &Index, name: &str, substr: bool, limit: usize) -> Reply {
    let syms: Vec<u64> = if substr {
        ix.syms_matching_substring(name, limit.max(SUBSTR_AGG_CAP))
    } else if let Some(s) = ix.sym_for_name(name) { vec![s] } else { Vec::new() };
    let is_container = |k: u8| matches!(k, kind::TYPE | kind::PACKAGE);
    let mut members: Vec<MemberHit> = Vec::new();
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut container_name = name.to_string();
    let mut truncated = false;
    'outer: for sym in &syms {
        let (cname, ckind, _) = ix.sym_meta(*sym).unwrap_or(("?", 0, 0));
        // Only a real container lists members — never a function (whose
        // childrev rows are its params/locals).
        if !is_container(ckind) { continue; }
        container_name = cname.to_string();
        for child in ix.members(*sym) {
            if !seen.insert(child) { continue; }
            let (mname, mkind, mlang) = ix.sym_meta(child).unwrap_or(("?", 0, 0));
            let sig = ix.sig_of(child).map(str::to_string);
            members.push(MemberHit {
                name: mname.to_string(),
                kind: kind_str(mkind).to_string(),
                lang: lang_str(mlang).to_string(),
                sig,
            });
            if members.len() >= limit { truncated = true; break 'outer; }
        }
    }
    let total = members.len();
    Reply::Members { container: container_name, members, total, truncated }
}

fn do_inh(ix: &Index, name: &str, substr: bool, limit: usize, sub: bool,
          filt: PathFilter<'_>) -> Reply {
    // super/sub relate TYPES; a substring also matches type-application
    // syms (`const(T)`, `T&`) and same-named members. Keep only type-kind
    // roots so the result isn't polluted by non-type matches.
    let syms: Vec<u64> = if substr {
        ix.syms_matching_substring(name, limit).into_iter()
            .filter(|&s| ix.sym_meta(s).is_some_and(|(_, k, _)| k == kind::TYPE))
            .collect()
    } else {
        ix.sym_for_name(name).into_iter().collect()
    };
    let name_of = |s: u64| ix.sym_meta(s).map(|(n,_,_)| n.to_string())
        .unwrap_or_else(|| format!("<sym {:016x}>", s));
    let mut hits: Vec<InhHit> = Vec::new();
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for sym in syms {
        let related = if sub { ix.inherited_by(sym) } else { ix.inherits_of(sym) };
        for r in related {
            if !seen.insert(r) { continue; }
            if filt.has_in_out() {
                let p = ix.sym_def_path(r).unwrap_or("");
                if !filt.passes(p) { continue; }
            }
            hits.push(InhHit { name: name_of(r) });
        }
    }
    let total = hits.len();
    Reply::Inh { hits, total }
}

// One callgraph BFS, supporting an arbitrary number of seed roots.
// When `substr` is true the seeds are every sym whose name contains
// `name`, capped at `root_limit`. Each seed is a separate root in
// the output forest (`parent: None`); BFS visits roots in seed order
// and any downstream node reachable from multiple roots is attributed
// to whichever root saw it first.
#[allow(clippy::too_many_arguments)]
fn do_callgraph(
    ix: &Index, name: &str, direction: &str,
    depth: usize, max_syms: usize,
    substr: bool, root_limit: usize,
    filt: PathFilter<'_>,
) -> Reply {
    let roots: Vec<u64> = if substr {
        ix.syms_matching_substring(name, root_limit)
    } else {
        ix.sym_for_name(name).into_iter().collect()
    };
    // --def-in narrows the seed roots only (scry semantics): the
    // walker doesn't carry per-frame def context, so deeper levels
    // can't be filtered the same way.
    let roots: Vec<u64> = match filt.def_in {
        Some(s) if !s.is_empty() => roots.into_iter()
            .filter(|r| ix.sym_def_path(*r).is_some_and(|p| p.contains(s)))
            .collect(),
        _ => roots,
    };
    // --in / --not-in apply at every level (including roots).
    let has_in_out = filt.has_in_out();
    let pass = |s: u64| -> bool {
        if !has_in_out { return true; }
        filt.passes(ix.sym_def_path(s).unwrap_or(""))
    };
    let roots: Vec<u64> = roots.into_iter().filter(|r| pass(*r)).collect();
    if roots.is_empty() {
        return Reply::Callgraph { nodes: Vec::new(), total: 0, truncated: false };
    }
    let name_of = |s: u64| ix.sym_meta(s).map(|(n,_,_)| n.to_string())
        .unwrap_or_else(|| format!("<sym {:016x}>", s));
    // BFS spanning forest. `seen` maps a sym to the dense node-id it
    // got when first discovered; `nodes` is the result in BFS order
    // (across roots and hops).
    let mut seen: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
    let mut nodes: Vec<CallNode> = Vec::new();
    let mut frontier: Vec<(u64, u32)> = Vec::new();  // (sym, node_id)
    for &root in &roots {
        if seen.contains_key(&root) { continue; }   // dedup overlap
        let id = nodes.len() as u32;
        seen.insert(root, id);
        nodes.push(CallNode { id, parent: None, hop: 0,
                              dir: "root".into(), name: name_of(root) });
        frontier.push((root, id));
    }
    let go_up   = direction == "up"   || direction == "both";
    let go_down = direction == "down" || direction == "both";
    let mut truncated = false;
    'depth: for hop in 1..=depth {
        let mut next: Vec<(u64, u32)> = Vec::new();
        for &(cur_sym, cur_id) in &frontier {
            if go_up {
                for (caller, _) in ix.called_by(cur_sym) {
                    if !pass(caller) { continue; }
                    if let std::collections::hash_map::Entry::Vacant(v) = seen.entry(caller) {
                        let id = nodes.len() as u32;
                        v.insert(id);
                        nodes.push(CallNode { id, parent: Some(cur_id), hop,
                                              dir: "up".into(), name: name_of(caller) });
                        next.push((caller, id));
                        if nodes.len() >= max_syms { truncated = true; break 'depth; }
                    }
                }
            }
            if go_down {
                for (callee, _) in ix.calls_from(cur_sym) {
                    if !pass(callee) { continue; }
                    if let std::collections::hash_map::Entry::Vacant(v) = seen.entry(callee) {
                        let id = nodes.len() as u32;
                        v.insert(id);
                        nodes.push(CallNode { id, parent: Some(cur_id), hop,
                                              dir: "down".into(), name: name_of(callee) });
                        next.push((callee, id));
                        if nodes.len() >= max_syms { truncated = true; break 'depth; }
                    }
                }
            }
        }
        if next.is_empty() { break; }
        frontier = next;
    }
    // total = non-root edges discovered. With N roots, there are N
    // root entries which don't count as hits.
    let total = nodes.len().saturating_sub(roots.len());
    Reply::Callgraph { nodes, total, truncated }
}

/// One inheritance BFS — the inheritance-graph analogue of
/// `do_callgraph`, sharing its BFS-forest shape verbatim. up walks
/// supertypes (`inherits_of`: child→parent); down walks subtypes
/// (`inherited_by`: parent→child, via the O(log n) `inhrev` section);
/// both is the union. Each match seeds a root; downstream nodes are
/// attributed to whichever root saw them first.
#[allow(clippy::too_many_arguments)]
fn do_inheritance(
    ix: &Index, name: &str, direction: &str,
    depth: usize, max_syms: usize,
    substr: bool, root_limit: usize,
    filt: PathFilter<'_>,
) -> Reply {
    let roots: Vec<u64> = if substr {
        // A hierarchy roots on TYPES. A substring also matches the
        // type-application syms (`const(T)`, `T&`, `T&&`) and same-named
        // members/ctors/dtors; keep only type-kind syms as roots.
        ix.syms_matching_substring(name, root_limit).into_iter()
            .filter(|&s| ix.sym_meta(s).is_some_and(|(_, k, _)| k == kind::TYPE))
            .collect()
    } else {
        ix.sym_for_name(name).into_iter().collect()
    };
    // --def-in narrows the seed roots only (matches callgraph semantics).
    let roots: Vec<u64> = match filt.def_in {
        Some(s) if !s.is_empty() => roots.into_iter()
            .filter(|r| ix.sym_def_path(*r).is_some_and(|p| p.contains(s)))
            .collect(),
        _ => roots,
    };
    let has_in_out = filt.has_in_out();
    let pass = |s: u64| -> bool {
        if !has_in_out { return true; }
        filt.passes(ix.sym_def_path(s).unwrap_or(""))
    };
    let roots: Vec<u64> = roots.into_iter().filter(|r| pass(*r)).collect();
    if roots.is_empty() {
        return Reply::Inheritance { nodes: Vec::new(), total: 0, truncated: false };
    }
    let name_of = |s: u64| ix.sym_meta(s).map(|(n,_,_)| n.to_string())
        .unwrap_or_else(|| format!("<sym {:016x}>", s));
    let mut seen: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
    let mut nodes: Vec<CallNode> = Vec::new();
    let mut frontier: Vec<(u64, u32)> = Vec::new();
    for &root in &roots {
        if seen.contains_key(&root) { continue; }
        let id = nodes.len() as u32;
        seen.insert(root, id);
        nodes.push(CallNode { id, parent: None, hop: 0,
                              dir: "root".into(), name: name_of(root) });
        frontier.push((root, id));
    }
    let go_up   = direction == "up"   || direction == "both";
    let go_down = direction == "down" || direction == "both";
    let mut truncated = false;
    'depth: for hop in 1..=depth {
        let mut next: Vec<(u64, u32)> = Vec::new();
        for &(cur_sym, cur_id) in &frontier {
            if go_up {
                for parent in ix.inherits_of(cur_sym) {
                    if !pass(parent) { continue; }
                    if let std::collections::hash_map::Entry::Vacant(v) = seen.entry(parent) {
                        let id = nodes.len() as u32;
                        v.insert(id);
                        nodes.push(CallNode { id, parent: Some(cur_id), hop,
                                              dir: "up".into(), name: name_of(parent) });
                        next.push((parent, id));
                        if nodes.len() >= max_syms { truncated = true; break 'depth; }
                    }
                }
            }
            if go_down {
                for child in ix.inherited_by(cur_sym) {
                    if !pass(child) { continue; }
                    if let std::collections::hash_map::Entry::Vacant(v) = seen.entry(child) {
                        let id = nodes.len() as u32;
                        v.insert(id);
                        nodes.push(CallNode { id, parent: Some(cur_id), hop,
                                              dir: "down".into(), name: name_of(child) });
                        next.push((child, id));
                        if nodes.len() >= max_syms { truncated = true; break 'depth; }
                    }
                }
            }
        }
        if next.is_empty() { break; }
        frontier = next;
    }
    let total = nodes.len().saturating_sub(roots.len());
    Reply::Inheritance { nodes, total, truncated }
}

/// One request → one reply over a `BufRead` + `Write` pair. Shared by
/// the Unix-socket daemon and the stdin/stdout REPL.
fn handle_lines<R: BufRead, W: Write>(mut r: R, mut w: W, ix: &Index) -> Result<()> {
    let mut line = String::new();
    while r.read_line(&mut line)? > 0 {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            let reply = match serde_json::from_str::<Request>(trimmed) {
                Ok(req) => dispatch(ix, &req),
                Err(e)  => Reply::Error { error: format!("bad request: {e}") },
            };
            writeln!(w, "{}", serde_json::to_string(&reply)?)?;
            w.flush()?;
        }
        line.clear();
    }
    Ok(())
}

/// Long-lived daemon. Listens on a Unix domain socket, services each
/// connection's line-delimited JSON requests against one in-process
/// Index. Sequential (not multi-threaded) on purpose — queries are
/// microseconds and concurrency would only add complexity.
pub fn serve(index_path: &Path, socket_path: &Path) -> Result<()> {
    let _ = std::fs::remove_file(socket_path);
    let ix = Index::open(index_path)
        .with_context(|| format!("open index {}", index_path.display()))?;
    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("bind {}", socket_path.display()))?;
    eprintln!("[serve] listening at {} (PID {})",
              socket_path.display(), std::process::id());
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let read_side = match stream.try_clone() {
                    Ok(s) => s, Err(e) => { eprintln!("[serve] clone: {e}"); continue; }
                };
                if let Err(e) = handle_lines(BufReader::new(read_side), stream, &ix) {
                    eprintln!("[serve] connection error: {e}");
                }
            }
            Err(e) => eprintln!("[serve] accept error: {e}"),
        }
    }
    Ok(())
}

/// REPL on stdin/stdout. Same wire shape as `serve` but no socket, no
/// system-wide state — dies when the parent closes stdin. The most
/// common scry2 use case (one LLM, many queries) wants this.
pub fn repl(index_path: &Path) -> Result<()> {
    let ix = Index::open(index_path)
        .with_context(|| format!("open index {}", index_path.display()))?;
    let stdin  = std::io::stdin().lock();
    let stdout = std::io::stdout().lock();
    handle_lines(BufReader::new(stdin), stdout, &ix)
}

/// Client side: send one request, read one reply. Used by the CLI
/// when `--socket PATH` is set.
pub fn client_call(socket_path: &Path, req: &Request) -> Result<Reply> {
    let mut s = UnixStream::connect(socket_path)
        .with_context(|| format!("connect {}", socket_path.display()))?;
    let line = serde_json::to_string(req)? + "\n";
    s.write_all(line.as_bytes())?;
    s.flush()?;
    let mut reader = BufReader::new(s);
    let mut reply = String::new();
    reader.read_line(&mut reply)?;
    if reply.trim().is_empty() { return Err(anyhow!("daemon closed connection")); }
    Ok(serde_json::from_str(reply.trim())?)
}

/// Resolve the default socket path for `--index FOO.s2db`: a stable
/// per-index path under `$XDG_RUNTIME_DIR` (or `/tmp`) so two scry2
/// processes pointing at the same index talk to the same daemon.
pub fn default_socket_for(index: &Path) -> PathBuf {
    use std::hash::Hasher;
    let mut h = twox_hash::XxHash64::with_seed(0xCAFE);
    h.write(index.as_os_str().as_encoded_bytes());
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(format!("{runtime}/scry2-{:016x}.sock", h.finish()))
}

#[cfg(test)]
mod tests {
    use super::PathFilter;

    fn f<'a>(in_: Option<&'a str>, not_in: Option<&'a str>) -> PathFilter<'a> {
        PathFilter { in_, not_in, def_in: None }
    }

    #[test]
    fn passes_no_filters_matches_everything() {
        let pf = f(None, None);
        assert!(pf.passes(""));
        assert!(pf.passes("frameworks/base/core/java/X.java"));
        assert!(!pf.has_in_out());
    }

    #[test]
    fn passes_in_substring_match() {
        let pf = f(Some("frameworks/"), None);
        assert!(pf.passes("/aosp/frameworks/base/x.java"));
        assert!(!pf.passes("/aosp/system/core/x.cpp"));
        assert!(pf.has_in_out());
    }

    #[test]
    fn passes_not_in_substring_match() {
        let pf = f(None, Some("/tests/"));
        assert!(pf.passes("/aosp/frameworks/base/x.java"));
        assert!(!pf.passes("/aosp/frameworks/base/tests/x.java"));
        assert!(pf.has_in_out());
    }

    #[test]
    fn passes_both_in_and_not_in() {
        // `--in frameworks/ --not-in /tests/` is the canonical
        // "real code only, no test files" query.
        let pf = f(Some("frameworks/"), Some("/tests/"));
        assert!(pf.passes("/aosp/frameworks/base/x.java"));
        assert!(!pf.passes("/aosp/system/core/x.cpp"));        // wrong subtree
        assert!(!pf.passes("/aosp/frameworks/base/tests/x.java")); // excluded
    }

    #[test]
    fn empty_string_filters_are_no_ops() {
        // Conservative semantics matching scry: an upstream that
        // forwards Option<String> without trimming may produce
        // Some(""). These should NOT reject everything.
        let pf = f(Some(""), Some(""));
        assert!(pf.passes("anything"));
        assert!(pf.passes(""));
        assert!(!pf.has_in_out(), "empty filters are not in/out filters");
    }
}
