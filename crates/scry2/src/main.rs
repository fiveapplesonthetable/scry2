//! `scry2` CLI — minimal verbs for LLM-driven code walks.

use anyhow::Result;
use clap::{Parser, Subcommand};
use scry2::{format::{kind, lang, role}, Index};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "scry2", version, about = "lean Kythe wrapper for AOSP")]
struct Cli {
    /// Path to the .s2db index file. Defaults to ./scry2.s2db.
    #[arg(long, global = true, default_value = "scry2.s2db")]
    index: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Stats about an index file: row counts, size, sanity check.
    Stat,

    /// `def NAME` — print the definition site (file:offset) of a symbol.
    /// NAME is a fully-qualified Kythe name, or a substring (use --substr).
    Def {
        name: String,
        /// Match `name` as a substring against any symbol's name.
        #[arg(long)] substr: bool,
        /// Cap matches when --substr is on.
        #[arg(long, default_value = "16")] limit: usize,
    },

    /// `ref NAME` — print every reference of a symbol.
    Ref {
        name: String,
        #[arg(long)] substr: bool,
        #[arg(long, default_value = "16")] limit: usize,
        /// Cap rows printed per match.
        #[arg(long, default_value = "200")] max_hits: usize,
    },

    /// `callers NAME` — print every call site of a function.
    Callers {
        name: String,
        #[arg(long)] substr: bool,
        #[arg(long, default_value = "16")] limit: usize,
        #[arg(long, default_value = "200")] max_hits: usize,
    },

    /// `super NAME` — print direct supertype(s) of a type.
    Super { name: String },

    /// `sub NAME` — print direct subtype(s) of a type.
    Sub { name: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let ix = Index::open(&cli.index)?;
    match cli.cmd {
        Cmd::Stat => cmd_stat(&ix),
        Cmd::Def { name, substr, limit }
            => cmd_xrefs(&ix, &name, substr, limit, role::DECL, role::DEF, usize::MAX, "def"),
        Cmd::Ref { name, substr, limit, max_hits }
            => cmd_xrefs(&ix, &name, substr, limit, 0, u8::MAX, max_hits, "ref"),
        Cmd::Callers { name, substr, limit, max_hits }
            => cmd_xrefs(&ix, &name, substr, limit, role::CALL, role::CALL, max_hits, "callers"),
        Cmd::Super { name } => cmd_inherits(&ix, &name, /*sub=*/false),
        Cmd::Sub   { name } => cmd_inherits(&ix, &name, /*sub=*/true),
    }
}

fn cmd_stat(ix: &Index) -> Result<()> {
    println!("xrefs:  {}", ix.n_xrefs());
    println!("syms:   {}", ix.n_syms());
    println!("files:  {}", ix.n_files());
    println!("inhs:   {}", ix.n_inh());
    Ok(())
}

fn cmd_xrefs(
    ix: &Index, name: &str, substr: bool, name_limit: usize,
    role_lo: u8, role_hi: u8, max_hits: usize, label: &str,
) -> Result<()> {
    let syms = resolve_syms(ix, name, substr, name_limit);
    if syms.is_empty() {
        eprintln!("{label}: no matches for '{name}'");
        return Ok(());
    }
    let mut total = 0usize;
    for sym in &syms {
        let (sname, knd, lng) = ix.sym_meta(*sym).unwrap_or(("?", kind::UNK, lang::UNK));
        println!("# {sname}  [{}/{}]", kind_str(knd), lang_str(lng));
        for (_, role, file, off) in ix.xrefs(*sym, role_lo, role_hi) {
            let path = ix.file_path(file).unwrap_or("?");
            println!("  {} {}@{}", role_str(role), path, off);
            total += 1;
            if total >= max_hits {
                eprintln!("({label} truncated at {max_hits} hits)");
                return Ok(());
            }
        }
    }
    eprintln!("hits={}", total);
    Ok(())
}

fn cmd_inherits(ix: &Index, name: &str, sub: bool) -> Result<()> {
    let sym = match ix.sym_for_name(name) {
        Some(s) => s,
        None => {
            eprintln!("no such symbol: '{name}'");
            return Ok(());
        }
    };
    let related = if sub { ix.inherited_by(sym) } else { ix.inherits_of(sym) };
    for s in &related {
        match ix.sym_meta(*s) {
            Some((n, _, _)) => println!("{n}"),
            None            => println!("<sym {:016x}>", s),
        }
    }
    eprintln!("hits={}", related.len());
    Ok(())
}

fn resolve_syms(ix: &Index, name: &str, substr: bool, limit: usize) -> Vec<u64> {
    if substr {
        ix.syms_matching_substring(name, limit)
    } else if let Some(s) = ix.sym_for_name(name) {
        vec![s]
    } else {
        Vec::new()
    }
}

fn role_str(r: u8) -> &'static str {
    match r {
        role::DECL => "decl",
        role::DEF  => "def",
        role::REF  => "ref",
        role::CALL => "call",
        _ => "?",
    }
}
fn kind_str(k: u8) -> &'static str {
    match k {
        kind::FUNCTION => "fn",
        kind::TYPE     => "type",
        kind::VARIABLE => "var",
        kind::FIELD    => "field",
        kind::PACKAGE  => "pkg",
        _              => "?",
    }
}
fn lang_str(l: u8) -> &'static str {
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
