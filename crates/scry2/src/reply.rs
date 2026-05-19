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
    pub rows: Vec<XrefHit>,
}

/// One edge in a callgraph BFS — produced by `callgraph`.
#[derive(Serialize, Deserialize, Debug)]
pub struct CallEdge {
    pub hop:  usize,
    /// "up" or "down"
    pub dir:  String,
    pub from: String,
    pub to:   String,
}

/// One inheritance hit — produced by `super` / `sub`.
#[derive(Serialize, Deserialize, Debug)]
pub struct InhHit { pub name: String }

/// Top-level reply envelope. Tag = command, payload depends on it.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Reply {
    Stat   { xrefs: u64, syms: u64, files: u64, inhs: u64, calls: u64 },
    Xrefs  { groups: Vec<SymbolGroup>, total: usize, truncated: bool },
    Inh    { hits: Vec<InhHit>, total: usize },
    Callgraph { edges: Vec<CallEdge>, total: usize, truncated: bool },
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
                println!("# {}  [{}/{}]", g.name, g.kind, g.lang);
                for r in &g.rows {
                    println!("  {} {}@{}", r.role, r.file, r.off);
                }
            }
            if *truncated { eprintln!("(truncated)"); }
            eprintln!("hits={total}");
        }
        Reply::Inh { hits, total } => {
            for h in hits { println!("{}", h.name); }
            eprintln!("hits={total}");
        }
        Reply::Callgraph { edges, total, truncated } => {
            for e in edges {
                let arrow = if e.dir == "up" { "←" } else { "→" };
                println!("hop={} {} {}  {}  {}", e.hop, e.dir, e.from, arrow, e.to);
            }
            if *truncated { eprintln!("(callgraph truncated)"); }
            eprintln!("hits={total}");
        }
        Reply::Error { error } => {
            eprintln!("error: {error}");
        }
    }
}
