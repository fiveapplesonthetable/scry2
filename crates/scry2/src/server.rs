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
use crate::reply::{CallNode, InhHit, Reply, SymbolGroup, XrefHit,
                   kind_str, lang_str, role_str};
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
}
fn default_root_limit() -> usize { 16 }
fn lim16() -> usize { 16 }
fn lim_max_hits() -> usize { 200 }
fn default_direction() -> String { "up".into() }
fn default_depth() -> usize { 3 }
fn default_max_syms() -> usize { 200 }

/// Path-substring filter shared by `def`, `ref`, and `callers`.
/// All three matches are on Kythe's stored `path` field with no
/// normalization (see docs/USAGE.md for path semantics).
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
            ix, name, *substr, *limit, role::DECL, role::DEF, usize::MAX,
            PathFilter { in_: in_.as_deref(), not_in: not_in.as_deref(), def_in: None },
        ),
        Request::Ref { name, substr, limit, max_hits, in_, not_in, def_in } => do_xrefs(
            ix, name, *substr, *limit, 0, u8::MAX, *max_hits,
            PathFilter { in_: in_.as_deref(), not_in: not_in.as_deref(), def_in: def_in.as_deref() },
        ),
        Request::Callers { name, substr, limit, max_hits, in_, not_in, def_in } => do_xrefs(
            ix, name, *substr, *limit, role::CALL, role::CALL, *max_hits,
            PathFilter { in_: in_.as_deref(), not_in: not_in.as_deref(), def_in: def_in.as_deref() },
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
    }
}

// The shape (sym-resolution + role-window + path-filter) is genuinely
// the contract this verb implements; splitting it further would just
// rename groups, not reduce surface.
#[allow(clippy::too_many_arguments)]
fn do_xrefs(
    ix: &Index, name: &str, substr: bool, name_limit: usize,
    role_lo: u8, role_hi: u8, max_hits: usize,
    filt: PathFilter<'_>,
) -> Reply {
    let syms: Vec<u64> = if substr {
        ix.syms_matching_substring(name, name_limit)
    } else if let Some(s) = ix.sym_for_name(name) { vec![s] } else { Vec::new() };
    let mut groups: Vec<SymbolGroup> = Vec::new();
    let mut total = 0usize;
    let mut truncated = false;
    'outer: for sym in &syms {
        if let Some(p) = filt.def_in {
            let mut def_ok = false;
            for (_, _, file, _) in ix.xrefs(*sym, role::DECL, role::DEF) {
                if let Some(path) = ix.file_path(file) {
                    if path.contains(p) { def_ok = true; break; }
                }
            }
            if !def_ok { continue; }
        }
        let (sname, knd, lng) = ix.sym_meta(*sym).unwrap_or(("?", 0, 0));
        let mut rows: Vec<XrefHit> = Vec::new();
        for (_, r, file, off) in ix.xrefs(*sym, role_lo, role_hi) {
            let path = ix.file_path(file).unwrap_or("?");
            if let Some(p) = filt.not_in { if path.contains(p) { continue; } }
            if let Some(p) = filt.in_    { if !path.contains(p) { continue; } }
            rows.push(XrefHit { role: role_str(r).to_string(), file: path.to_string(), off });
            total += 1;
            if total >= max_hits {
                truncated = true;
                groups.push(SymbolGroup {
                    name: sname.to_string(),
                    kind: kind_str(knd).to_string(),
                    lang: lang_str(lng).to_string(),
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
                rows,
            });
        }
    }
    Reply::Xrefs { groups, total, truncated }
}

fn do_inh(ix: &Index, name: &str, substr: bool, limit: usize, sub: bool,
          filt: PathFilter<'_>) -> Reply {
    let syms: Vec<u64> = if substr {
        ix.syms_matching_substring(name, limit)
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
            if filt.in_.is_some() || filt.not_in.is_some() {
                let p = ix.sym_def_path(r).unwrap_or("");
                if let Some(s) = filt.in_     { if !p.contains(s) { continue; } }
                if let Some(s) = filt.not_in  { if  p.contains(s) { continue; } }
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
    let roots: Vec<u64> = if let Some(s) = filt.def_in {
        roots.into_iter()
            .filter(|r| ix.sym_def_path(*r).is_some_and(|p| p.contains(s)))
            .collect()
    } else { roots };
    // --in / --not-in apply at every level (including roots).
    let pass = |s: u64| -> bool {
        if filt.in_.is_none() && filt.not_in.is_none() { return true; }
        let p = ix.sym_def_path(s).unwrap_or("");
        if let Some(needle) = filt.in_    { if !p.contains(needle) { return false; } }
        if let Some(needle) = filt.not_in { if  p.contains(needle) { return false; } }
        true
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
