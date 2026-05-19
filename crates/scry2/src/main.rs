//! `scry2` CLI — minimal verbs for LLM-driven code walks.
//!
//! Every query verb builds a `Reply` shape (see `reply.rs`) and emits
//! it via the same code path the `serve` daemon uses. The CLI either
//! opens the index in-process (fast for one-shot) or forwards the
//! request over a Unix socket to a long-lived daemon (zero startup
//! overhead for batch queries). `--json` toggles machine-readable
//! output in both modes.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use scry2::{Index, IndexBuilder, kythe, kzip, reply::{Reply, emit}, server::{self, Request}};
use std::path::PathBuf;
use std::time::Instant;

/// Expand a leading `~/` (and bare `~`) in `s` against `home`. Pure
/// — no env access — so tests can drive every branch without mutating
/// process-global state. With `home = None` the tilde is preserved
/// verbatim (matches what most shells do when `$HOME` is unset).
fn expand_tilde(s: &str, home: Option<&std::ffi::OsStr>) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(h) = home {
            let mut p = PathBuf::from(h);
            p.push(rest);
            return p;
        }
    } else if s == "~" {
        if let Some(h) = home { return PathBuf::from(h); }
    }
    PathBuf::from(s)
}

/// Clap value parser for every path-valued argument. Thin shim over
/// [`expand_tilde`] that reads `$HOME` from the environment so the
/// CLI accepts shell-style home references like `~/scry2-setup/...`.
fn path_arg(s: &str) -> Result<PathBuf, String> {
    Ok(expand_tilde(s, std::env::var_os("HOME").as_deref()))
}

#[derive(Parser, Debug)]
#[command(name = "scry2", version, about = "lean Kythe wrapper for AOSP")]
struct Cli {
    /// Path to the .s2db index file. Defaults to ./scry2.s2db. Ignored
    /// when --socket is set — the daemon owns the index.
    #[arg(long, global = true, default_value = "scry2.s2db", value_parser = path_arg)]
    index: PathBuf,

    /// If set, send the query to the `scry2 serve` daemon listening
    /// on this Unix socket instead of opening the index in-process.
    /// Eliminates the ~10 ms process-startup + mmap cost per query.
    #[arg(long, global = true, value_parser = path_arg)]
    socket: Option<PathBuf>,

    /// Emit machine-readable JSON. Same wire shape the daemon returns.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Stats about an index file: row counts, size, sanity check.
    Stat,

    /// Build an .s2db from one or more delimited Kythe Entry proto
    /// streams. Use `-` to read from stdin.
    Index {
        #[arg(long = "entries", required = true, value_parser = path_arg)]
        entries: Vec<PathBuf>,
        #[arg(short, long, default_value = "scry2.s2db", value_parser = path_arg)]
        out: PathBuf,
    },

    /// `def NAME` — print the definition site(s) of a symbol.
    Def {
        name: String,
        #[arg(long)] substr: bool,
        #[arg(long, default_value = "16")] limit: usize,
        #[arg(long = "in", value_name = "SUBSTR")] in_: Option<String>,
        #[arg(long = "not-in", value_name = "SUBSTR")] not_in: Option<String>,
    },

    /// `ref NAME` — print every reference of a symbol.
    Ref {
        name: String,
        #[arg(long)] substr: bool,
        #[arg(long, default_value = "16")] limit: usize,
        #[arg(long, default_value = "200")] max_hits: usize,
        #[arg(long = "in", value_name = "SUBSTR")] in_: Option<String>,
        #[arg(long = "not-in", value_name = "SUBSTR")] not_in: Option<String>,
        #[arg(long = "def-in", value_name = "SUBSTR")] def_in: Option<String>,
    },

    /// `callers NAME` — print every call site of a function.
    Callers {
        name: String,
        #[arg(long)] substr: bool,
        #[arg(long, default_value = "16")] limit: usize,
        #[arg(long, default_value = "200")] max_hits: usize,
        #[arg(long = "in", value_name = "SUBSTR")] in_: Option<String>,
        #[arg(long = "not-in", value_name = "SUBSTR")] not_in: Option<String>,
        #[arg(long = "def-in", value_name = "SUBSTR")] def_in: Option<String>,
    },

    /// `super NAME` — direct supertypes (extends / overrides / satisfies).
    Super {
        name: String,
        #[arg(long)] substr: bool,
        #[arg(long, default_value = "16")] limit: usize,
        /// Restrict to supertypes whose def-file path contains SUBSTR.
        #[arg(long = "in", value_name = "SUBSTR")] in_: Option<String>,
        /// Drop supertypes whose def-file path contains SUBSTR.
        #[arg(long = "not-in", value_name = "SUBSTR")] not_in: Option<String>,
    },

    /// `sub NAME` — direct subtypes.
    Sub {
        name: String,
        #[arg(long)] substr: bool,
        #[arg(long, default_value = "16")] limit: usize,
        /// Restrict to subtypes whose def-file path contains SUBSTR.
        #[arg(long = "in", value_name = "SUBSTR")] in_: Option<String>,
        /// Drop subtypes whose def-file path contains SUBSTR.
        #[arg(long = "not-in", value_name = "SUBSTR")] not_in: Option<String>,
    },

    /// `callgraph NAME` — transitive walk of the call graph.
    Callgraph {
        name: String,
        #[arg(long, value_parser = ["up", "down", "both"], default_value = "up")]
        direction: String,
        #[arg(long, default_value = "3")] depth: usize,
        #[arg(long, default_value = "200")] max_syms: usize,
        /// Match `name` as a substring; every match seeds the BFS as
        /// a separate root in the output forest. parent=None marks
        /// each root; ids are unique across the whole reply.
        #[arg(long)] substr: bool,
        /// Cap roots when --substr is on. Default 16.
        #[arg(long, default_value = "16")] root_limit: usize,
        /// Restrict expansion: drop any discovered sym whose def-file
        /// path doesn't contain SUBSTR. Applied at every BFS level.
        #[arg(long = "in", value_name = "SUBSTR")] in_: Option<String>,
        /// Symmetric to `--in`. Drop syms whose def-file path contains
        /// SUBSTR — useful for pruning whole subtrees (e.g. `/tests/`).
        #[arg(long = "not-in", value_name = "SUBSTR")] not_in: Option<String>,
        /// Root-level narrowing only (matches scry semantics):
        /// drop seed roots whose def-file path doesn't contain SUBSTR.
        /// Deeper levels are NOT narrowed.
        #[arg(long = "def-in", value_name = "SUBSTR")] def_in: Option<String>,
    },

    /// `serve --socket PATH` — long-lived daemon over a Unix socket.
    /// For when N unrelated processes share one warm index. Most
    /// callers want `repl` instead.
    Serve {
        /// Socket path. Defaults to a stable per-index path under
        /// $XDG_RUNTIME_DIR (or /tmp).
        #[arg(long, value_parser = path_arg)] socket: Option<PathBuf>,
    },

    /// `repl` — stdin/stdout JSON loop. One request per line in, one
    /// reply per line out. The leanest way for an LLM (or a script)
    /// to amortize startup across many queries.
    Repl,

    /// `normalize-kzip` — read a mixed-encoding (`pbunits/` + `units/`)
    /// kzip and write a proto-only kzip that every stock Kythe
    /// indexer accepts. AOSP's `build_kzip.bash` produces mixed-
    /// encoding output that crashes stock `cxx_indexer` with
    /// "Malformed kzip: multiple unit encodings but different entries".
    /// Run this once before `from-kzip`.
    NormalizeKzip {
        #[arg(long = "in",  value_name = "PATH", value_parser = path_arg)] in_:  PathBuf,
        #[arg(long = "out", value_name = "PATH", value_parser = path_arg)] out: PathBuf,
    },

    /// `from-kzip` — build an .s2db by running each Kythe indexer
    /// against KZIP and ingesting all entries.
    FromKzip {
        #[arg(long, value_parser = path_arg)] kzip: PathBuf,
        #[arg(long = "kythe-root", value_parser = path_arg)] kythe_root: PathBuf,
        #[arg(short, long, default_value = "scry2.s2db", value_parser = path_arg)] out: PathBuf,
        #[arg(long, default_value = "cxx,java,jvm,go,proto,textproto")]
        langs: String,
        #[arg(long, default_value = "8g")] jvm_heap: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Build-side verbs don't go through Reply.
    match cli.cmd {
        Cmd::Index { entries, out }  => return cmd_index(&entries, &out),
        Cmd::NormalizeKzip { in_, out } => return cmd_normalize_kzip(&in_, &out),
        Cmd::FromKzip { kzip, kythe_root, out, langs, jvm_heap } =>
            return cmd_from_kzip(&kzip, &kythe_root, &out, &langs, &jvm_heap),
        Cmd::Serve { socket } => {
            let sock = socket.unwrap_or_else(|| server::default_socket_for(&cli.index));
            return server::serve(&cli.index, &sock);
        }
        Cmd::Repl => return server::repl(&cli.index),
        _ => {}
    }
    // Query-side: build a Request, dispatch in-process or via socket.
    let req = match cli.cmd {
        Cmd::Stat => Request::Stat,
        Cmd::Def { name, substr, limit, in_, not_in }
            => Request::Def { name, substr, limit, in_, not_in },
        Cmd::Ref { name, substr, limit, max_hits, in_, not_in, def_in }
            => Request::Ref { name, substr, limit, max_hits, in_, not_in, def_in },
        Cmd::Callers { name, substr, limit, max_hits, in_, not_in, def_in }
            => Request::Callers { name, substr, limit, max_hits, in_, not_in, def_in },
        Cmd::Super { name, substr, limit, in_, not_in }
            => Request::Super { name, substr, limit, in_, not_in },
        Cmd::Sub   { name, substr, limit, in_, not_in }
            => Request::Sub   { name, substr, limit, in_, not_in },
        Cmd::Callgraph { name, direction, depth, max_syms, substr, root_limit,
                         in_, not_in, def_in }
            => Request::Callgraph { name, direction, depth, max_syms, substr,
                                    root_limit, in_, not_in, def_in },
        _ => unreachable!(),
    };
    let reply: Reply = if let Some(sock) = cli.socket {
        server::client_call(&sock, &req)?
    } else {
        let ix = Index::open(&cli.index)?;
        server::dispatch(&ix, &req)
    };
    emit(&reply, cli.json);
    Ok(())
}

/// One-line-per-second progress printer, used by every long-running
/// CLI subcommand that takes a `Progress`. Throttles to once-per-
/// second within a phase, but always emits the first line of a new
/// phase so the user sees the transition (read → write-units →
/// write-files).
struct CliProgress {
    label: &'static str,
    phase: String,
    phase_t0: Instant,
    last_tick: Instant,
}

impl CliProgress {
    fn new(label: &'static str) -> Self {
        let now = Instant::now();
        Self { label, phase: String::new(), phase_t0: now, last_tick: now }
    }
}

impl kzip::Progress for CliProgress {
    fn report(&mut self, phase: &str, done: usize, total: usize) {
        let new_phase = self.phase != phase;
        if new_phase {
            self.phase.clear();
            self.phase.push_str(phase);
            self.phase_t0 = Instant::now();
        }
        if !new_phase && self.last_tick.elapsed().as_secs() < 1 { return; }
        self.last_tick = Instant::now();
        let pct = if total == 0 { 0.0 } else { 100.0 * done as f64 / total as f64 };
        eprintln!("[{}] {:>11} {:>7}/{:<7} ({:>5.1}%)  +{:.1}s",
            self.label, phase, done, total, pct,
            self.phase_t0.elapsed().as_secs_f64());
    }
}

fn cmd_normalize_kzip(in_: &std::path::Path, out: &std::path::Path) -> Result<()> {
    let t0 = Instant::now();
    eprintln!("[normalize] reading {}", in_.display());
    let (n_units, n_files) =
        kzip::normalize_progress(in_, out, CliProgress::new("normalize"))?;
    eprintln!(
        "[normalize] done in {:.1}s — {} units, {} unique file blobs → {}",
        t0.elapsed().as_secs_f64(), n_units, n_files, out.display(),
    );
    Ok(())
}

fn cmd_index(entries: &[PathBuf], out: &std::path::Path) -> Result<()> {
    let mut builder = IndexBuilder::new();
    let mut file_ids = kythe::FileIdAllocator::default();
    let t0 = Instant::now();
    for path in entries {
        let label = path.display();
        let stats = if path.as_os_str() == "-" {
            eprintln!("[index] reading from stdin");
            kythe::ingest(std::io::stdin().lock(), &mut builder, &mut file_ids)?
        } else {
            eprintln!("[index] reading {label}");
            let f = std::fs::File::open(path)
                .with_context(|| format!("open {label}"))?;
            kythe::ingest(f, &mut builder, &mut file_ids)?
        };
        eprintln!(
            "[index]   {label}: entries={} anchors={} xrefs={} inherits={} aliases={} calls={} completes={}",
            stats.entries, stats.anchors_flushed, stats.xrefs_emitted,
            stats.inherits_emitted, stats.aliases_emitted, stats.calls_emitted,
            stats.completes_bridges,
        );
        eprintln!(
            "[index]   {label}: diag bodies={} pending={} unresolved={}",
            stats.diag_defines_seen, stats.diag_pending, stats.diag_unresolved,
        );
    }
    file_ids.drain_into(&mut builder);
    eprintln!(
        "[index] writing — xrefs={} syms={} files={} inhs={} calls={}",
        builder.n_xrefs(), builder.n_syms(), builder.n_files(),
        builder.n_inh(), builder.n_calls(),
    );
    let bytes = builder.finish(out)?;
    eprintln!(
        "[index] done in {:.2}s → {} ({:.2} GB)",
        t0.elapsed().as_secs_f64(), out.display(), bytes as f64 / 1e9,
    );
    Ok(())
}

fn cmd_from_kzip(
    kzip: &std::path::Path,
    kythe_root: &std::path::Path,
    out: &std::path::Path,
    langs: &str,
    jvm_heap: &str,
) -> Result<()> {
    use std::process::{Command, Stdio};
    let want: std::collections::HashSet<&str> = langs.split(',').map(|s| s.trim()).collect();
    let mut builder = IndexBuilder::new();
    let mut file_ids = kythe::FileIdAllocator::default();
    let t0 = Instant::now();

    let cxx        = kythe_root.join("indexers/cxx_indexer");
    let java_jar   = kythe_root.join("indexers/java_indexer.jar");
    let jvm_jar    = kythe_root.join("indexers/jvm_indexer.jar");
    let go         = kythe_root.join("indexers/go_indexer");
    let proto      = kythe_root.join("indexers/proto_indexer");
    let textproto  = kythe_root.join("indexers/textproto_indexer");

    let make_jvm = |jar: &std::path::Path| -> Command {
        let mut c = Command::new("java");
        c.arg(format!("-Xmx{jvm_heap}"))
            .arg("-jar").arg(jar)
            .arg("--ignore_empty_kzip")
            .arg(kzip);
        c
    };
    let make_proto = |bin: &std::path::Path, dash: &str| -> Command {
        let mut c = Command::new(bin);
        c.arg(format!("{dash}index_file={}", kzip.display()));
        c
    };

    let mut to_run: Vec<(&'static str, Command)> = Vec::new();
    if want.contains("cxx") && cxx.exists()       { let mut c = Command::new(&cxx); c.arg(kzip); to_run.push(("cxx", c)); }
    if want.contains("java") && java_jar.exists() { to_run.push(("java", make_jvm(&java_jar))); }
    if want.contains("jvm")  && jvm_jar.exists()  { to_run.push(("jvm",  make_jvm(&jvm_jar))); }
    if want.contains("go")   && go.exists()       { let mut c = Command::new(&go); c.arg(kzip); to_run.push(("go",   c)); }
    if want.contains("proto") && proto.exists()   { to_run.push(("proto", make_proto(&proto, "-"))); }
    if want.contains("textproto") && textproto.exists() {
        to_run.push(("textproto", make_proto(&textproto, "--")));
    }
    if to_run.is_empty() {
        anyhow::bail!("from-kzip: no indexer binaries found under {} for langs={langs}",
            kythe_root.display());
    }
    for (label, mut cmd) in to_run {
        let phase_t = Instant::now();
        eprintln!("[from-kzip] running {label}");
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::null()).spawn()
            .with_context(|| format!("spawn {label} indexer"))?;
        let stdout = child.stdout.take().unwrap();
        let stats = scry2::kythe::ingest_tolerant(stdout, &mut builder, &mut file_ids, true)?;
        let exit = child.wait()?;
        eprintln!(
            "[from-kzip]   {label}: entries={} anchors={} xrefs={} inherits={} aliases={} calls={} (wall={:.1}s, exit={:?})",
            stats.entries, stats.anchors_flushed, stats.xrefs_emitted,
            stats.inherits_emitted, stats.aliases_emitted, stats.calls_emitted,
            phase_t.elapsed().as_secs_f64(), exit.code(),
        );
    }
    file_ids.drain_into(&mut builder);
    eprintln!(
        "[from-kzip] writing — xrefs={} syms={} files={} inhs={} calls={}",
        builder.n_xrefs(), builder.n_syms(), builder.n_files(),
        builder.n_inh(), builder.n_calls(),
    );
    let bytes = builder.finish(out)?;
    eprintln!(
        "[from-kzip] done in {:.2}s → {} ({:.2} GB)",
        t0.elapsed().as_secs_f64(), out.display(), bytes as f64 / 1e9,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::expand_tilde;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    fn h(s: &str) -> Option<&OsStr> { Some(OsStr::new(s)) }

    #[test]
    fn no_tilde_is_passthrough() {
        assert_eq!(expand_tilde("/abs/path", h("/home/u")), PathBuf::from("/abs/path"));
        assert_eq!(expand_tilde("rel/path",  h("/home/u")), PathBuf::from("rel/path"));
        assert_eq!(expand_tilde("",          h("/home/u")), PathBuf::from(""));
    }

    #[test]
    fn tilde_slash_expands() {
        assert_eq!(expand_tilde("~/scry2-setup/aosp.s2db", h("/home/test-user")),
                   PathBuf::from("/home/test-user/scry2-setup/aosp.s2db"));
    }

    #[test]
    fn bare_tilde_is_home() {
        assert_eq!(expand_tilde("~", h("/home/test-user")),
                   PathBuf::from("/home/test-user"));
    }

    #[test]
    fn tilde_only_at_start() {
        // `foo/~/bar` is a literal path; only leading `~/` expands.
        assert_eq!(expand_tilde("foo/~/bar", h("/home/u")),
                   PathBuf::from("foo/~/bar"));
        // Embedded tilde without slash is also literal.
        assert_eq!(expand_tilde("~user/foo", h("/home/u")),
                   PathBuf::from("~user/foo"));
    }

    #[test]
    fn no_home_falls_back_to_verbatim() {
        // No `$HOME` → tilde stays in the path verbatim. Don't crash,
        // don't guess.
        assert_eq!(expand_tilde("~/foo", None), PathBuf::from("~/foo"));
        assert_eq!(expand_tilde("~",     None), PathBuf::from("~"));
    }
}
