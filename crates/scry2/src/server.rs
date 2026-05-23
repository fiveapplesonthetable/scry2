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
    Def { name: String, #[serde(default)] substr: bool,
          #[serde(default)] ignore_case: bool, #[serde(default = "lim16")] limit: usize,
          #[serde(default, rename = "in")]  in_:    Option<String>,
          #[serde(default)] not_in: Option<String> },
    Type { name: String, #[serde(default)] substr: bool,
           #[serde(default = "lim16")] limit: usize },
    Sig  { name: String, #[serde(default)] substr: bool,
           #[serde(default = "lim16")] limit: usize },
    Members { name: String, #[serde(default)] substr: bool,
              #[serde(default = "lim16")] limit: usize },
    Ref { name: String, #[serde(default)] substr: bool,
          #[serde(default)] ignore_case: bool, #[serde(default = "lim16")] limit: usize,
          #[serde(default = "lim_max_hits")] max_hits: usize,
          #[serde(default, rename = "in")] in_: Option<String>,
          #[serde(default)] not_in: Option<String>,
          #[serde(default)] def_in: Option<String> },
    Callers { name: String, #[serde(default)] substr: bool,
              #[serde(default)] ignore_case: bool, #[serde(default = "lim16")] limit: usize,
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
        Request::Def { name, substr, ignore_case, limit, in_, not_in } => do_xrefs(
            ix, name, *substr, *ignore_case, *limit, (role::DECL, role::DEF), usize::MAX,
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
        Request::Ref { name, substr, ignore_case, limit, max_hits, in_, not_in, def_in } => do_xrefs(
            ix, name, *substr, *ignore_case, (*limit).max(SUBSTR_AGG_CAP), (0, u8::MAX), *max_hits,
            PathFilter { in_: in_.as_deref(), not_in: not_in.as_deref(), def_in: def_in.as_deref() },
            /*with_type=*/false,
        ),
        Request::Callers { name, substr, ignore_case, limit, max_hits, in_, not_in, def_in } => do_xrefs(
            ix, name, *substr, *ignore_case, (*limit).max(SUBSTR_AGG_CAP), (role::CALL, role::CALL), *max_hits,
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

/// Symbol-set cap for `ref`/`callers --substr`. The scan stops after this
/// many name matches and edges are gathered only across them, so the cost
/// is bounded no matter how broad the substring is — gathering callers of
/// thousands of symbols is both slow (the 11-34s cliff) and meaningless.
/// 64 still reaches the carrier for a realistically-ambiguous leaf
/// (`clearCallingIdentity` → 12 variants); a broader match returns a fast
/// partial with `truncated` set, signalling "narrow it / use exact FQN".
/// `--limit N` above 64 raises the cap for callers who want more.
const SUBSTR_AGG_CAP: usize = 64;

#[allow(clippy::too_many_arguments)]
fn do_xrefs(
    ix: &Index, name: &str, substr: bool, ignore_case: bool, sym_cap: usize,
    roles: (u8, u8), max_hits: usize,
    filt: PathFilter<'_>,
    with_type: bool,
) -> Reply {
    let (role_lo, role_hi) = roles;
    let syms: Vec<u64> = if substr {
        // The case fold applies ONLY to substring matching; the default
        // (case-sensitive) path is unchanged. Exact-FQN lookups
        // (`!substr`) are never folded.
        if ignore_case {
            ix.syms_matching_substring_ci(name, sym_cap)
        } else {
            ix.syms_matching_substring(name, sym_cap)
        }
    } else { ix.syms_for_name(name) };
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
    } else { ix.syms_for_name(name) };
    let mut hits: Vec<TypeHit> = Vec::new();
    // Dedup by the rendered (name, typed): stub-jar copies are distinct
    // syms that render identically. Genuine distinct types survive.
    let mut seen: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    for sym in &syms {
        let Some(ty) = ix.type_of(*sym) else { continue };
        let (sname, knd, lng) = ix.sym_meta(*sym).unwrap_or(("?", 0, 0));
        if !seen.insert((logical_key(sname).to_string(), ty.to_string())) { continue; }
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
    } else { ix.syms_for_name(name) };
    let mut hits: Vec<SigHit> = Vec::new();
    // Dedup by the rendered (name, sig): the stub-jar copies of a method
    // resolve to distinct syms with byte-identical signatures. Genuine
    // overloads differ in `sig` and so survive.
    let mut seen: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    for sym in &syms {
        let Some(sg) = ix.sig_of(*sym) else { continue };
        let (sname, knd, lng) = ix.sym_meta(*sym).unwrap_or(("?", 0, 0));
        if !seen.insert((logical_key(sname).to_string(), sg.to_string())) { continue; }
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
    } else { ix.syms_for_name(name) };
    let is_container = |k: u8| matches!(k, kind::TYPE | kind::PACKAGE);
    let mut members: Vec<MemberHit> = Vec::new();
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    // Dedup by the RENDERED (name, kind, sig), not just the child sym: a
    // container resolved across several stub-jar copies yields distinct
    // child syms that render identically. Genuine overloads differ in
    // their rendered signature and so survive.
    let mut rendered: std::collections::HashSet<(String, String, Option<String>)> =
        std::collections::HashSet::new();
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
            let key = (logical_key(mname).to_string(), kind_str(mkind).to_string(), sig.clone());
            if !rendered.insert(key) { continue; }
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

/// A symbol's *logical identity* for dedup, derived from its stored name.
///
/// AOSP compiles the same source into many build-variant stub jars, so one
/// logical Java/C++ element appears as many distinct syms whose tickets
/// differ ONLY in corpus/path. A scry ticket is
/// `kythe:{lang}:{corpus}#{root}#{path}#{signature}`; the trailing
/// `signature` is Kythe's own per-element semantic id and is byte-identical
/// across those copies. So for a ticket we key on that signature (the part
/// after the last `#`); for a human FQN alias (no `kythe:` prefix) the whole
/// name already is the identity. Pairing this with the rendered sig/type
/// keeps genuine overloads (different signatures) and distinct symbols apart.
fn logical_key(name: &str) -> &str {
    if name.starts_with("kythe:") {
        name.rsplit('#').next().unwrap_or(name)
    } else {
        name
    }
}

/// Resolve a sym's definition site to a `path@off` string, preferring a
/// DEF over a DECL (the same precedence `def` uses). Returns None when the
/// sym has neither location. Shared by the inheritance verbs so each
/// related sym gets a concrete locator even when its name is a bare ticket.
fn def_loc_str(ix: &Index, sym: u64) -> Option<String> {
    let (file, off) = ix.sym_def_loc(sym)?;
    let path = ix.file_path(file)?;
    Some(format!("{path}@{off}"))
}

/// A readable display name for an inheritance/subtype hit.
///
/// Anonymous/local Java types and unnamed C++ entities never get an FQN
/// alias, so their stored name is the raw Kythe ticket
/// (`kythe:java:...#<hash>`). Surfacing that ticket as a result `name`
/// is noise — it has a real def site but no human identity. When the
/// resolved name is still a ticket we render a readable fallback instead,
/// applied uniformly to text output AND the `--json` `name` field: first a
/// concrete `anon@<path>@<off>` from its def location, else its trailing
/// VName signature (the part after the last `#`) — never the raw ticket. A
/// real FQN/named-edge name is returned verbatim.
fn display_name(ix: &Index, sym: u64, name: &str) -> String {
    if !name.starts_with("kythe:") {
        return name.to_string();
    }
    if let Some((file, off)) = ix.sym_def_loc(sym) {
        if let Some(path) = ix.file_path(file) {
            return format!("anon@{path}@{off}");
        }
    }
    // No def site: fall back to the trailing VName signature, which is
    // the element's stable per-element id — still far more useful than
    // the full ticket and never empty for a well-formed ticket.
    let sig = name.rsplit('#').next().unwrap_or(name);
    if sig.is_empty() { name.to_string() } else { sig.to_string() }
}

fn do_inh(ix: &Index, name: &str, substr: bool, limit: usize, sub: bool,
          filt: PathFilter<'_>) -> Reply {
    // No kind filter: `inh` holds method `overrides` as well as type
    // `extends`, so `super`/`sub` must work on a method (what it overrides /
    // what overrides it) — not just a type. Filtering roots to kind==TYPE
    // silently dropped every override query.
    let syms: Vec<u64> = if substr {
        ix.syms_matching_substring(name, limit)
    } else {
        ix.syms_for_name(name)
    };
    let name_of = |s: u64| ix.sym_meta(s).map(|(n,_,_)| n.to_string())
        .unwrap_or_else(|| format!("<sym {:016x}>", s));
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    // Group related syms by LOGICAL identity, not the raw sym: stub-jar copies
    // are distinct syms for one logical supertype/subtype, differing only in
    // build-variant path. `logical_key` keys on the Kythe VName signature,
    // identical across those copies. We render ONE hit per group, but pick the
    // def location and the display name PER FIELD from whichever copy carries
    // them — copies vary in whether they have a def anchor or a readable FQN.
    // First-wins-on-one-copy (the old behaviour) dropped the def@offset when
    // the first copy happened to lack an anchor, which is why `super` showed
    // FQN-only while `sub`/`inheritance` (which pick a locating rep) did not.
    let mut order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<u64>> =
        std::collections::HashMap::new();
    for sym in syms {
        let related = if sub { ix.inherited_by(sym) } else { ix.inherits_of(sym) };
        for r in related {
            if !seen.insert(r) { continue; }
            if filt.has_in_out() {
                let p = ix.sym_def_path(r).unwrap_or("");
                if !filt.passes(p) { continue; }
            }
            let lk = logical_key(&name_of(r)).to_string();
            match groups.entry(lk.clone()) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(vec![r]);
                    order.push(lk);
                }
                std::collections::hash_map::Entry::Occupied(mut e) => e.get_mut().push(r),
            }
        }
    }
    let mut hits: Vec<InhHit> = Vec::new();
    for lk in &order {
        let members = &groups[lk];
        // def: the first copy that actually has a def site (else None — the
        // element genuinely has no source location, e.g. a `.class`-only type).
        let def = members.iter().copied().find_map(|m| def_loc_str(ix, m));
        // name: prefer a copy with a readable FQN (non-`kythe:` ticket); fall
        // back to the anonymous renderer (def site / trailing VName sig) of the
        // first copy for genuinely nameless nodes.
        let rep = members
            .iter()
            .copied()
            .find(|&m| !name_of(m).starts_with("kythe:"))
            .unwrap_or(members[0]);
        let name = display_name(ix, rep, &name_of(rep));
        hits.push(InhHit { name, def });
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
        ix.syms_for_name(name)
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
    // The full set of dup syms that share a representative's logical
    // identity. AOSP compiles one logical function into many build-variant
    // stub copies; each copy is a distinct sym, and the call edges are NOT
    // mirrored across them — only some copies carry callers/callees. So
    // collapsing dup roots to one logical node (the fix below) MUST union
    // the call edges across all dups, or the surviving root would lose
    // every edge that lived on a sibling copy. Dups share the same stored
    // name string, so `syms_for_name(name)` recovers the group; we keep
    // only those with the same logical key to avoid pulling in unrelated
    // syms that merely share an alias string.
    let dup_syms = |rep: u64| -> Vec<u64> {
        let name = name_of(rep);
        let key = logical_key(&name).to_string();
        let mut group: Vec<u64> = ix.syms_for_name(&name).into_iter()
            .filter(|&s| logical_key(&name_of(s)) == key)
            .collect();
        if !group.contains(&rep) { group.push(rep); }
        group
    };
    let callers_of = |rep: u64| -> Vec<u64> {
        let mut out: Vec<u64> = dup_syms(rep).into_iter()
            .flat_map(|s| ix.called_by(s)).map(|(c, _)| c).collect();
        out.sort_unstable(); out.dedup(); out
    };
    let callees_of = |rep: u64| -> Vec<u64> {
        let mut out: Vec<u64> = dup_syms(rep).into_iter()
            .flat_map(|s| ix.calls_from(s)).map(|(c, _)| c).collect();
        out.sort_unstable(); out.dedup(); out
    };
    // BFS spanning forest. `seen` maps a sym to the dense node-id it
    // got when first discovered; `nodes` is the result in BFS order
    // (across roots and hops). `rendered` collapses stub-jar dups by
    // LOGICAL identity so the forest carries one node per logical
    // function — without it `callgraph parseInt` floods ~18 identical
    // parent=- roots and burns the --max-syms budget. A dup's edges are
    // already folded into the canonical node via the union helpers above,
    // so an aliased dup is routed to the canonical node and not retraversed.
    let mut seen: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
    let mut nodes: Vec<CallNode> = Vec::new();
    let mut frontier: Vec<(u64, u32)> = Vec::new();  // (sym, node_id)
    let mut rendered: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    let admit = |sym: u64, parent_id: Option<u32>, dir: &str, hop: usize,
                     nodes: &mut Vec<CallNode>,
                     seen: &mut std::collections::HashMap<u64, u32>,
                     rendered: &mut std::collections::HashMap<String, u32>|
        -> Option<u32> {
        if seen.contains_key(&sym) { return None; }
        let name = name_of(sym);
        let key = logical_key(&name).to_string();
        if let Some(&existing) = rendered.get(&key) {
            seen.insert(sym, existing);
            return None;
        }
        let id = nodes.len() as u32;
        seen.insert(sym, id);
        rendered.insert(key, id);
        nodes.push(CallNode { id, parent: parent_id, hop, dir: dir.into(), name, def: None });
        Some(id)
    };
    for &root in &roots {
        if let Some(id) = admit(root, None, "root", 0, &mut nodes, &mut seen, &mut rendered) {
            frontier.push((root, id));
        }
    }
    let go_up   = direction == "up"   || direction == "both";
    let go_down = direction == "down" || direction == "both";
    let mut truncated = false;
    'depth: for hop in 1..=depth {
        let mut next: Vec<(u64, u32)> = Vec::new();
        for &(cur_sym, cur_id) in &frontier {
            if go_up {
                for caller in callers_of(cur_sym) {
                    if !pass(caller) { continue; }
                    if let Some(id) = admit(caller, Some(cur_id), "up", hop,
                                            &mut nodes, &mut seen, &mut rendered) {
                        next.push((caller, id));
                        if nodes.len() >= max_syms { truncated = true; break 'depth; }
                    }
                }
            }
            if go_down {
                for callee in callees_of(cur_sym) {
                    if !pass(callee) { continue; }
                    if let Some(id) = admit(callee, Some(cur_id), "down", hop,
                                            &mut nodes, &mut seen, &mut rendered) {
                        next.push((callee, id));
                        if nodes.len() >= max_syms { truncated = true; break 'depth; }
                    }
                }
            }
        }
        if next.is_empty() { break; }
        frontier = next;
    }
    // total = non-root nodes. Count actual hop-0 nodes (roots collapse
    // under rendered-dedup, so `roots.len()` would overcount).
    let root_nodes = nodes.iter().filter(|n| n.hop == 0).count();
    let total = nodes.len().saturating_sub(root_nodes);
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
        // No kind filter: walking `inh`/`inhrev` covers method `overrides`
        // as well as type `extends`, so a method root is legitimate.
        ix.syms_matching_substring(name, root_limit)
    } else {
        ix.syms_for_name(name)
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
    // The full set of dup syms that share a representative sym's logical
    // identity. AOSP compiles one logical type into many build-variant stub
    // copies; each copy is a distinct sym, and the `inherits`/`inherited_by`
    // edges are NOT mirrored across them — typically only one copy carries
    // them. So expanding a frontier node by a single representative sym
    // (the first dup admitted) silently drops every edge that lives on a
    // sibling copy: hub types like Thread/HashMap returned EMPTY because the
    // first dup happened to have no edges. We must UNION the edges across
    // all dups, exactly as `do_inh` does for the seed name. Dups share the
    // same stored name string, so `syms_for_name(name)` recovers the group;
    // we keep only those with the same logical key (identical VName sig) to
    // avoid pulling in unrelated syms that merely share an alias string.
    let dup_syms = |rep: u64| -> Vec<u64> {
        let name = name_of(rep);
        let key = logical_key(&name).to_string();
        let mut group: Vec<u64> = ix.syms_for_name(&name).into_iter()
            .filter(|&s| logical_key(&name_of(s)) == key)
            .collect();
        if !group.contains(&rep) { group.push(rep); }
        group
    };
    // Union an inheritance frontier across all dups of `rep`, deduped by sym.
    let up_edges = |rep: u64| -> Vec<u64> {
        let mut out: Vec<u64> = dup_syms(rep).into_iter()
            .flat_map(|s| ix.inherits_of(s)).collect();
        out.sort_unstable(); out.dedup(); out
    };
    let down_edges = |rep: u64| -> Vec<u64> {
        let mut out: Vec<u64> = dup_syms(rep).into_iter()
            .flat_map(|s| ix.inherited_by(s)).collect();
        out.sort_unstable(); out.dedup(); out
    };
    let mut seen: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
    let mut nodes: Vec<CallNode> = Vec::new();
    let mut frontier: Vec<(u64, u32)> = Vec::new();
    // Dedup by LOGICAL identity, not the sym: stub-jar copies are distinct
    // syms for one logical type, so the per-sym `seen` map alone leaves
    // duplicate roots/hops. `logical_key` keys on the Kythe VName signature
    // (identical across copies). When a candidate's logical key already has
    // a node, we route this sym to that node (so `seen` stays consistent)
    // but emit no second node and don't re-traverse its identical edges.
    let mut rendered: std::collections::HashMap<String, u32> =
        std::collections::HashMap::new();
    // Try to admit `sym` as a new node under `parent_id`/`dir`/`hop`.
    // Returns Some(id) only when a NEW node was created (so the caller
    // traverses it once); None when the sym was already seen OR aliased to
    // an existing logical-equal node (a stub-jar duplicate — the canonical
    // node already drives traversal). Closures can't borrow these mutably
    // across the loop, so the tables are threaded in explicitly.
    let admit = |sym: u64, parent_id: Option<u32>, dir: &str, hop: usize,
                     nodes: &mut Vec<CallNode>,
                     seen: &mut std::collections::HashMap<u64, u32>,
                     rendered: &mut std::collections::HashMap<String, u32>|
        -> Option<u32> {
        if seen.contains_key(&sym) { return None; }
        let name = name_of(sym);
        let key = logical_key(&name).to_string();
        if let Some(&existing) = rendered.get(&key) {
            seen.insert(sym, existing);
            return None;
        }
        let def = def_loc_str(ix, sym);
        let id = nodes.len() as u32;
        seen.insert(sym, id);
        rendered.insert(key, id);
        // Dedup keys on the raw name's logical key (above); the displayed
        // name renders a readable fallback for anonymous/local types whose
        // stored name is still a bare `kythe:` ticket.
        let name = display_name(ix, sym, &name);
        nodes.push(CallNode { id, parent: parent_id, hop, dir: dir.into(), name, def });
        Some(id)
    };
    for &root in &roots {
        if let Some(id) = admit(root, None, "root", 0, &mut nodes, &mut seen, &mut rendered) {
            frontier.push((root, id));
        }
    }
    let go_up   = direction == "up"   || direction == "both";
    let go_down = direction == "down" || direction == "both";
    let mut truncated = false;
    'depth: for hop in 1..=depth {
        let mut next: Vec<(u64, u32)> = Vec::new();
        for &(cur_sym, cur_id) in &frontier {
            if go_up {
                for parent in up_edges(cur_sym) {
                    if !pass(parent) { continue; }
                    if let Some(id) = admit(parent, Some(cur_id), "up", hop,
                                            &mut nodes, &mut seen, &mut rendered) {
                        next.push((parent, id));
                        if nodes.len() >= max_syms { truncated = true; break 'depth; }
                    }
                }
            }
            if go_down {
                for child in down_edges(cur_sym) {
                    if !pass(child) { continue; }
                    if let Some(id) = admit(child, Some(cur_id), "down", hop,
                                            &mut nodes, &mut seen, &mut rendered) {
                        next.push((child, id));
                        if nodes.len() >= max_syms { truncated = true; break 'depth; }
                    }
                }
            }
        }
        if next.is_empty() { break; }
        frontier = next;
    }
    // total = non-root nodes. Count actual hop-0 nodes (roots can collapse
    // under rendered-dedup, so `roots.len()` would overcount).
    let root_nodes = nodes.iter().filter(|n| n.hop == 0).count();
    let total = nodes.len().saturating_sub(root_nodes);
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
    use super::{PathFilter, logical_key};

    fn f<'a>(in_: Option<&'a str>, not_in: Option<&'a str>) -> PathFilter<'a> {
        PathFilter { in_, not_in, def_in: None }
    }

    #[test]
    fn logical_key_collapses_stub_jar_copies() {
        // Two stub-jar copies of one logical method: same trailing VName
        // signature, different corpus/path → same logical key.
        let a = "kythe:java:corpus#root#variantA/Bundle.java#SIGHASH";
        let b = "kythe:java:corpus#root#variantB/Bundle.java#SIGHASH";
        assert_eq!(logical_key(a), "SIGHASH");
        assert_eq!(logical_key(a), logical_key(b));
        // A different element has a different trailing signature.
        let other = "kythe:java:corpus#root#variantA/Bundle.java#OTHERHASH";
        assert_ne!(logical_key(a), logical_key(other));
    }

    #[test]
    fn logical_key_passes_fqn_aliases_through() {
        // A human FQN (no `kythe:` prefix) is already the identity; it is
        // returned whole, even though it contains no `#`.
        assert_eq!(logical_key("android.os.Bundle.putByteArray"),
                   "android.os.Bundle.putByteArray");
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
