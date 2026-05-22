//! Structured reply shapes used by every query verb. The same types
//! drive `--json` output on stdout AND the line-delimited JSON
//! protocol behind `scry2 serve` / `--socket`, so what the CLI prints
//! is byte-equal to what the daemon returns.

use crate::format::{kind, lang, role};
use serde::{Deserialize, Serialize};

/// One row of an xref / ref / callers result.
#[derive(Serialize, Deserialize, Debug)]
pub struct XrefHit {
    /// "decl" | "def" | "ref" | "call"
    pub role: String,
    pub file: String,
    pub off:  u32,
}

/// All xref rows for one matched symbol, plus that symbol's metadata.
#[derive(Serialize, Deserialize, Debug)]
pub struct SymbolGroup {
    pub name: String,
    pub kind: String,
    pub lang: String,
    /// The symbol's compiler-resolved type, rendered to a string (e.g.
    /// "const Box<int> &", "java.lang.String"), when the index has one.
    /// Omitted from JSON when absent so `def` output stays clean for the
    /// (common) symbols that have no typed edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typed: Option<String>,
    /// The symbol's full rendered signature with parameter names (e.g.
    /// "void setEnabled(bool enabled)"), when the index has one. Omitted
    /// from JSON when absent so `def` output stays clean for symbols
    /// with no rendered signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
    pub rows: Vec<XrefHit>,
}

/// One `type NAME` result: a symbol and its resolved type.
#[derive(Serialize, Deserialize, Debug)]
pub struct TypeHit {
    pub name: String,
    pub kind: String,
    pub lang: String,
    pub typed: String,
}

/// One `sig NAME` result: a symbol and its full rendered signature
/// (with parameter names).
#[derive(Serialize, Deserialize, Debug)]
pub struct SigHit {
    pub name: String,
    pub kind: String,
    pub lang: String,
    pub sig:  String,
}

/// One `members NAME` result: a direct member of a container, with its
/// own kind/name (and signature when it's a function we've rendered).
#[derive(Serialize, Deserialize, Debug)]
pub struct MemberHit {
    pub name: String,
    pub kind: String,
    pub lang: String,
    /// The member's full signature when it's a function we rendered one
    /// for. Omitted otherwise so the output stays clean for fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

/// One node in a callgraph BFS — produced by `callgraph`. Each node
/// carries an `id` (dense, unique within the reply) and a `parent`
/// pointing at the node that *discovered* it. The set of nodes is
/// the BFS spanning tree from the query root:
///
/// * `parent: None` → this is the root the user asked about.
/// * `parent: Some(p)` → this node was reached from node `p` in one
///   `up` or `down` hop. Re-walking parent pointers from any node
///   gives the exact path back to the root.
///
/// Nodes are emitted in BFS order (parents always before children),
/// so a streaming consumer can build the tree on the fly.
#[derive(Serialize, Deserialize, Debug)]
pub struct CallNode {
    pub id:     u32,
    pub parent: Option<u32>,
    pub hop:    usize,
    /// "up" (this node calls `parent`) or "down" (this node is called
    /// by `parent`). For the root, `dir` is "root".
    pub dir:    String,
    pub name:   String,
    /// The node's definition site as `path@off`, when the index has a
    /// DECL/DEF for it. The inheritance forest sets this so each hop
    /// shows a concrete location next to its (often ticket-shaped) name;
    /// the callgraph forest leaves it None. Omitted from JSON when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub def: Option<String>,
}

/// One inheritance hit — produced by `super` / `sub`. `name` is the
/// related sym's stored name (an FQN alias when one was emitted, else
/// the raw Kythe ticket); `def` is its definition site as `path@off`
/// when the index has a DECL/DEF, giving a concrete locator even when
/// `name` is a bare ticket.
#[derive(Serialize, Deserialize, Debug)]
pub struct InhHit {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub def: Option<String>,
}

/// Top-level reply envelope. Tag = command, payload depends on it.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Reply {
    Stat   { xrefs: u64, syms: u64, files: u64, inhs: u64, calls: u64 },
    Xrefs  { groups: Vec<SymbolGroup>, total: usize, truncated: bool },
    Inh    { hits: Vec<InhHit>, total: usize },
    Callgraph { nodes: Vec<CallNode>, total: usize, truncated: bool },
    /// `inheritance NAME` — reuses the callgraph BFS-forest node shape
    /// (id/parent/hop), but each node is a type reached by walking the
    /// inheritance graph (up = supertypes, down = subtypes).
    Inheritance { nodes: Vec<CallNode>, total: usize, truncated: bool },
    Type   { hits: Vec<TypeHit>, total: usize, truncated: bool },
    Sig    { hits: Vec<SigHit>, total: usize, truncated: bool },
    Members { container: String, members: Vec<MemberHit>, total: usize, truncated: bool },
    Error  { error: String },
}

pub fn role_str(r: u8) -> &'static str {
    match r {
        role::DECL => "decl",
        role::DEF  => "def",
        role::REF  => "ref",
        role::CALL => "call",
        _ => "?",
    }
}
pub fn kind_str(k: u8) -> &'static str {
    match k {
        kind::FUNCTION => "fn",
        kind::TYPE     => "type",
        kind::VARIABLE => "var",
        kind::FIELD    => "field",
        kind::PACKAGE  => "pkg",
        _              => "?",
    }
}
pub fn lang_str(l: u8) -> &'static str {
    match l {
        lang::CXX    => "cxx",
        lang::JAVA   => "java",
        lang::JVM    => "jvm",
        lang::GO     => "go",
        lang::PROTO  => "proto",
        lang::RUST   => "rust",
        lang::KOTLIN => "kt",
        _            => "?",
    }
}

/// The cap-reached note printed (to stderr) when a result is truncated.
/// `shown` is how many rows actually printed. Spells out that the count
/// is capped, not the whole truth, so `--limit`-bounded results aren't
/// silently mistaken for complete ones.
fn cap_note(shown: usize) -> String {
    format!("(showing {shown}; --limit cap reached, more exist — raise --limit)")
}

/// Print a Reply to stdout in either human or JSON form. `truncated`
/// notes appear on stderr in both modes so a `| jq` pipeline still
/// sees clean JSON on stdout.
pub fn emit(reply: &Reply, as_json: bool) {
    if as_json {
        println!("{}", serde_json::to_string(reply).expect("Reply serializes"));
        return;
    }
    match reply {
        Reply::Stat { xrefs, syms, files, inhs, calls } => {
            println!("xrefs:  {xrefs}");
            println!("syms:   {syms}");
            println!("files:  {files}");
            println!("inhs:   {inhs}");
            println!("calls:  {calls}");
        }
        Reply::Xrefs { groups, total, truncated } => {
            for g in groups {
                match &g.typed {
                    Some(t) => println!("# {}  [{}/{}]  : {}", g.name, g.kind, g.lang, t),
                    None    => println!("# {}  [{}/{}]", g.name, g.kind, g.lang),
                }
                if let Some(s) = &g.sig {
                    println!("  sig: {s}");
                }
                for r in &g.rows {
                    println!("  {} {}@{}", r.role, r.file, r.off);
                }
            }
            if *truncated { eprintln!("{}", cap_note(*total)); }
            eprintln!("hits={total}");
        }
        Reply::Type { hits, total, truncated } => {
            for h in hits {
                println!("# {}  [{}/{}]", h.name, h.kind, h.lang);
                println!("  {}", h.typed);
            }
            if *truncated { eprintln!("{}", cap_note(*total)); }
            eprintln!("hits={total}");
        }
        Reply::Sig { hits, total, truncated } => {
            for h in hits {
                println!("# {}  [{}/{}]", h.name, h.kind, h.lang);
                println!("  {}", h.sig);
            }
            if *truncated { eprintln!("{}", cap_note(*total)); }
            eprintln!("hits={total}");
        }
        Reply::Members { container, members, total, truncated } => {
            println!("# {container}");
            for m in members {
                match &m.sig {
                    Some(s) => println!("  {} [{}/{}]  {}", m.name, m.kind, m.lang, s),
                    None    => println!("  {} [{}/{}]", m.name, m.kind, m.lang),
                }
            }
            if *truncated { eprintln!("{}", cap_note(*total)); }
            eprintln!("members={total}");
        }
        Reply::Inheritance { nodes, total, truncated } => {
            for n in nodes {
                let p = n.parent.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
                match &n.def {
                    Some(loc) => println!("  id={:<3} parent={:<3} hop={} {:<4} {}  {}",
                                          n.id, p, n.hop, n.dir, n.name, loc),
                    None      => println!("  id={:<3} parent={:<3} hop={} {:<4} {}",
                                          n.id, p, n.hop, n.dir, n.name),
                }
            }
            if *truncated {
                eprintln!("(showing {}; --max-syms cap reached, more exist — raise --max-syms)",
                          nodes.len());
            }
            eprintln!("hits={total}");
        }
        Reply::Inh { hits, total } => {
            for h in hits {
                match &h.def {
                    Some(loc) => println!("{}  {}", h.name, loc),
                    None      => println!("{}", h.name),
                }
            }
            eprintln!("hits={total}");
        }
        Reply::Callgraph { nodes, total, truncated } => {
            // Print as `id=N parent=P  hop=H  dir  name` so a human
            // can trace `parent` back to the root or pipe the output
            // into a tree-rendering tool. The structured `--json`
            // shape is the source of truth for programmatic use.
            for n in nodes {
                let p = n.parent.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
                println!("  id={:<3} parent={:<3} hop={} {:<4} {}",
                         n.id, p, n.hop, n.dir, n.name);
            }
            if *truncated { eprintln!("(callgraph truncated)"); }
            eprintln!("hits={total}");
        }
        Reply::Error { error } => {
            eprintln!("error: {error}");
        }
    }
}
