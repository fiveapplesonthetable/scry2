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
use scry2::{Index, IndexBuilder, kythe, kzip, kzip::Progress as _,
            reply::{Reply, emit}, server::{self, Request}};
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

    /// Diagnostic (hidden): read MarkedSource bytes from FILE and print
    /// the FQN our parser produces. Used to verify the cxx_indexer
    /// `/kythe/code` decode path against real-world inputs.
    #[command(hide = true)]
    DebugMarkedSource {
        #[arg(value_parser = path_arg)] file: PathBuf,
    },

    /// `names PREFIX` — diagnostic: list alphabetically-sorted name
    /// index entries starting with PREFIX. Useful for confirming what
    /// aliases the indexer actually emitted (debug "why doesn't
    /// `def foo.Bar.baz` work?" — see if "foo.Bar.baz" or
    /// "foo.Bar.baz()" is in the index).
    Names {
        prefix: String,
        #[arg(long, default_value = "32")] limit: usize,
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

    /// `from-kzip` — build an .s2db by extracting each CU into a
    /// per-CU sub-kzip, running the appropriate Kythe indexer on it,
    /// and ingesting the emitted Entry proto stream. Per-CU dispatch
    /// is what makes the run robust against one bad CU killing the
    /// whole batch (cxx_indexer segfaults on malformed argv).
    FromKzip {
        #[arg(long, value_parser = path_arg)] kzip: PathBuf,
        #[arg(long = "kythe-root", value_parser = path_arg)] kythe_root: PathBuf,
        #[arg(short, long, default_value = "scry2.s2db", value_parser = path_arg)] out: PathBuf,
        /// Comma-separated languages to index. Routing is by the CU's
        /// `v_name.language`: c++ → cxx_indexer, java → java_indexer,
        /// jvm → jvm_indexer, go → go_indexer, protobuf → proto_indexer,
        /// textproto → textproto_indexer.
        #[arg(long, default_value = "cxx,java,jvm,go,proto,textproto")]
        langs: String,
        #[arg(long, default_value = "8g")] jvm_heap: String,
        /// Restrict to CUs whose primary source path (from
        /// `source_file[0]` or `required_input[0]`) contains ANY of
        /// these comma-separated substrings. Repeatable.
        #[arg(long = "in", value_name = "SUBSTR", num_args = 1.., value_delimiter = ',')]
        in_: Vec<String>,
        /// Drop CUs whose primary path contains ANY of these. Repeatable.
        #[arg(long = "not-in", value_name = "SUBSTR", num_args = 1.., value_delimiter = ',')]
        not_in: Vec<String>,
        /// Per-CU staging directory. Each sub-kzip is built here, fed
        /// to the indexer, then removed. Defaults to a process-local
        /// dir under `$SCRY_TMP_DIR` (or /mnt/agent/tmp if set, else
        /// the system tmp).
        #[arg(long = "staging", value_parser = path_arg)]
        staging: Option<PathBuf>,
        /// Number of CUs to index concurrently. Defaults to num_cpus/2
        /// (JVM-based indexers carry a 200-300 MB working set, so we
        /// cap to avoid OOM on big runs).
        #[arg(long, default_value = "0")] workers: usize,
        /// Prepend an extra javac/clang arg to any CU whose primary
        /// path starts with PREFIX. Repeatable. Format: `PREFIX::ARG`.
        /// The `::` is the separator (path prefixes don't contain it;
        /// indexer args may contain single `:` so we use a doubled
        /// form).
        ///
        /// Example (AOSP libcore needs --patch-module=java.base to
        /// index ojluni files — the base of java.base):
        ///   --inject-cu-arg 'libcore/ojluni/src/main/java/::--patch-module=java.base=libcore/ojluni/src/main/java'
        ///
        /// Skip if the rule's ARG already appears in the CU's argv.
        /// See `scripts/aosp-from-kzip.sh` for an AOSP-shaped wrapper
        /// that emits the right rule set for a Soong out/ tree.
        #[arg(long = "inject-cu-arg", value_name = "PREFIX::ARG")]
        inject_cu_args: Vec<String>,
        /// Resume a killed run. The previous run's partial state lives
        /// at `<OUT>.partial.s2db` (rolling snapshot, written every
        /// 2000 successful CUs) plus `<OUT>.partial.shas` (the list of
        /// CU shas already folded into that snapshot). With `--resume`
        /// we load the partial as the starting builder state and skip
        /// any plan entry whose sha is listed.
        #[arg(long, default_value_t = false)] resume: bool,
        /// Take a builder snapshot every N successful CUs. Lower =
        /// more durable but more wall time spent on the clone-and-
        /// write cycle. Default 2000 — at AOSP scale each snapshot is
        /// ~6 GB and ~10 s, so 2000 CUs ≈ one snapshot per 5 minutes
        /// of indexer wall time.
        #[arg(long = "snapshot-every", default_value_t = 2000)] snapshot_every: usize,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Build-side verbs don't go through Reply.
    match cli.cmd {
        Cmd::Index { entries, out }  => return cmd_index(&entries, &out),
        Cmd::NormalizeKzip { in_, out } => return cmd_normalize_kzip(&in_, &out),
        Cmd::FromKzip { kzip, kythe_root, out, langs, jvm_heap,
                        in_, not_in, staging, workers, inject_cu_args,
                        resume, snapshot_every } => {
            let rules = parse_inject_rules(&inject_cu_args)?;
            return cmd_from_kzip(FromKzipArgs {
                kzip: &kzip, kythe_root: &kythe_root, out: &out,
                langs: &langs, jvm_heap: &jvm_heap,
                in_: &in_, not_in: &not_in,
                staging: staging.as_deref(), workers,
                inject_rules: &rules,
                resume, snapshot_every,
            });
        }
        Cmd::Serve { socket } => {
            let sock = socket.unwrap_or_else(|| server::default_socket_for(&cli.index));
            return server::serve(&cli.index, &sock);
        }
        Cmd::Repl => return server::repl(&cli.index),
        Cmd::DebugMarkedSource { file } => {
            let bytes = std::fs::read(&file)
                .with_context(|| format!("read {}", file.display()))?;
            match scry2::kythe::parse_marked_source_fqn(&bytes) {
                Some(fqn) => println!("{fqn}"),
                None      => println!("(no FQN extracted)"),
            }
            return Ok(());
        }
        Cmd::Names { prefix, limit } => {
            let ix = Index::open(&cli.index)?;
            for (name, sym) in ix.names_with_prefix(&prefix, limit) {
                println!("0x{sym:016x}  {name}");
            }
            return Ok(());
        }
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
    let file_ids = kythe::FileIdAllocator::default();
    let t0 = Instant::now();
    for path in entries {
        let label = path.display();
        let stats = if path.as_os_str() == "-" {
            eprintln!("[index] reading from stdin");
            kythe::ingest(std::io::stdin().lock(), &mut builder, &file_ids)?
        } else {
            eprintln!("[index] reading {label}");
            let f = std::fs::File::open(path)
                .with_context(|| format!("open {label}"))?;
            kythe::ingest(f, &mut builder, &file_ids)?
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

/// Routing from a CU's `v_name.language` to an indexer binary +
/// invocation shape. `None` means "no indexer in this Kythe release
/// for this language" (kotlin and rust source — Google-internal only
/// in v0.0.75); those CUs are counted as skipped, not failed.
#[derive(Clone, Copy, Debug)]
enum IndexerKind {
    Cxx,
    JavaSource,
    JvmBytecode,
    Go,
    Proto,
    TextProto,
}

/// Classify by `v_name.language`. Stays in sync with the CLI
/// `--langs` filter; an unknown language returns None and the CU
/// is counted in the `skipped` bucket of the run summary.
fn route_language(lang: &str) -> Option<IndexerKind> {
    match lang {
        "c++"       => Some(IndexerKind::Cxx),
        "java"      => Some(IndexerKind::JavaSource),
        "jvm"       => Some(IndexerKind::JvmBytecode),
        "go"        => Some(IndexerKind::Go),
        "protobuf" | "proto" => Some(IndexerKind::Proto),
        "textproto" => Some(IndexerKind::TextProto),
        _           => None,
    }
}

fn lang_label(k: IndexerKind) -> &'static str {
    match k {
        IndexerKind::Cxx         => "cxx",
        IndexerKind::JavaSource  => "java",
        IndexerKind::JvmBytecode => "jvm",
        IndexerKind::Go          => "go",
        IndexerKind::Proto       => "proto",
        IndexerKind::TextProto   => "textproto",
    }
}

/// Bundled args — keeps `cmd_from_kzip` under the clippy threshold
/// for arg count without losing call-site clarity.
struct FromKzipArgs<'a> {
    kzip:         &'a std::path::Path,
    kythe_root:   &'a std::path::Path,
    out:          &'a std::path::Path,
    langs:        &'a str,
    jvm_heap:     &'a str,
    in_:          &'a [String],
    not_in:       &'a [String],
    staging:      Option<&'a std::path::Path>,
    workers:      usize,
    inject_rules: &'a [InjectRule],
    /// When true, attempt to resume from `<out>.partial.s2db` + the
    /// matching shas file. See `Cmd::FromKzip::resume` for the wire
    /// definition.
    resume:         bool,
    /// CU interval between rolling builder snapshots. See
    /// `Cmd::FromKzip::snapshot_every`.
    snapshot_every: usize,
}

/// One `--inject-cu-arg` rule: when a CU's primary path starts with
/// `path_prefix`, prepend `arg` to its compiler argv. Multiple rules
/// stack; each rule fires independently. `arg` is matched against
/// existing argv strings byte-for-byte and skipped if already present
/// (so the wrapper script can be safely re-run on already-augmented
/// kzips).
#[derive(Debug, Clone)]
struct InjectRule {
    path_prefix: String,
    arg:         String,
}

/// Parse `--inject-cu-arg PREFIX::ARG` flags into structured rules.
/// Splits on the FIRST `::`. PREFIX and ARG are both required and
/// non-empty; malformed input is rejected with a clear error so the
/// user finds the typo instead of having the rule silently no-op.
fn parse_inject_rules(raw: &[String]) -> Result<Vec<InjectRule>> {
    let mut out = Vec::with_capacity(raw.len());
    for r in raw {
        let (p, a) = r.split_once("::").ok_or_else(|| anyhow::anyhow!(
            "--inject-cu-arg: missing `::` separator in {r:?}; expected PREFIX::ARG"))?;
        if p.is_empty() {
            anyhow::bail!("--inject-cu-arg: empty PREFIX in {r:?}");
        }
        if a.is_empty() {
            anyhow::bail!("--inject-cu-arg: empty ARG in {r:?}");
        }
        out.push(InjectRule { path_prefix: p.into(), arg: a.into() });
    }
    Ok(out)
}

fn build_indexer_command(
    kind: IndexerKind,
    kythe_root: &std::path::Path,
    cu_kzip: &std::path::Path,
    jvm_heap: &str,
    jvm_temp_dir: &std::path::Path,
) -> Result<std::process::Command> {
    use std::process::Command;
    match kind {
        IndexerKind::Cxx => {
            let bin = kythe_root.join("indexers/cxx_indexer");
            if !bin.exists() { anyhow::bail!("cxx_indexer missing: {}", bin.display()); }
            let mut c = Command::new(bin); c.arg(cu_kzip);
            Ok(c)
        }
        IndexerKind::Go => {
            let bin = kythe_root.join("indexers/go_indexer");
            if !bin.exists() { anyhow::bail!("go_indexer missing: {}", bin.display()); }
            let mut c = Command::new(bin); c.arg(cu_kzip);
            Ok(c)
        }
        IndexerKind::JavaSource | IndexerKind::JvmBytecode => {
            let jar = kythe_root.join(if matches!(kind, IndexerKind::JavaSource) {
                "indexers/java_indexer.jar"
            } else {
                "indexers/jvm_indexer.jar"
            });
            if !jar.exists() { anyhow::bail!("{} missing", jar.display()); }
            let mut c = Command::new("java");
            c.arg(format!("-Xmx{jvm_heap}"))
                .arg("-jar").arg(jar)
                .arg("--ignore_empty_kzip")
                .arg("--temp_directory").arg(jvm_temp_dir)
                .arg(cu_kzip);
            Ok(c)
        }
        IndexerKind::Proto => {
            let bin = kythe_root.join("indexers/proto_indexer");
            if !bin.exists() { anyhow::bail!("proto_indexer missing: {}", bin.display()); }
            let mut c = Command::new(bin);
            c.arg(format!("-index_file={}", cu_kzip.display()));
            Ok(c)
        }
        IndexerKind::TextProto => {
            let bin = kythe_root.join("indexers/textproto_indexer");
            if !bin.exists() { anyhow::bail!("textproto_indexer missing: {}", bin.display()); }
            let mut c = Command::new(bin);
            c.arg(format!("--index_file={}", cu_kzip.display()));
            Ok(c)
        }
    }
}

/// One CU's run summary — accumulated across workers and into the
/// final per-language report.
#[derive(Default, Debug, Clone)]
struct LangStats {
    cus:       usize,
    entries:   u64,
    anchors:   u64,
    xrefs:     u64,
    inherits:  u64,
    aliases:   u64,
    calls:     u64,
    succeeded: usize,
    empty:     usize,
    failed:    usize,
    fail_tails: Vec<String>,  // first N failure stderr tails for diagnosis
}

const MAX_FAIL_TAILS: usize = 8;
const STDERR_TAIL_BYTES: usize = 4096;

/// One worker's owned ingest state. Each worker thread holds a
/// `Mutex<WorkerSink>` and is the only contender for the lock in
/// the normal path; the snapshotter occasionally takes the lock
/// briefly (microseconds when the worker is between CUs, the
/// remaining ingest time if the worker is mid-CU). Coupling
/// `builder` and `pending_shas` under one mutex is the load-bearing
/// invariant: snapshotting both atomically guarantees the on-disk
/// `.partial.shas` never lists a CU whose data isn't in
/// `.partial.s2db`.
#[derive(Default)]
struct WorkerSink {
    builder:      IndexBuilder,
    pending_shas: Vec<String>,
}

/// The accumulator carries the snapshot history: every drained
/// worker sink merges into `builder`, and the corresponding shas
/// land in `committed_shas`. On `--resume` both are seeded from the
/// partial `.s2db` + `.shas`. Lock contention is rare — only the
/// snapshotter writes; workers never touch it.
struct Accumulator {
    builder:        IndexBuilder,
    committed_shas: std::collections::HashSet<String>,
}

/// Write a fresh `<partial>.s2db` and `<partial>.shas` pair atomically
/// (via `.tmp` + rename). Crash safety: the rename order is s2db
/// first, then shas — so a crash between the two leaves the partial
/// s2db newer than the shas. On resume we require both files to be
/// present; the bounded ~µs gap is documented as the only window
/// where partial state can mismatch, in which case the user is
/// instructed to discard the partial and restart.
fn write_snapshot(
    builder: IndexBuilder,
    shas:    &[String],
    s2db_path: &std::path::Path,
    shas_path: &std::path::Path,
) -> Result<()> {
    let s2db_tmp = s2db_path.with_extension("s2db.tmp");
    builder.finish(&s2db_tmp)
        .with_context(|| format!("write snapshot {}", s2db_tmp.display()))?;
    std::fs::rename(&s2db_tmp, s2db_path)
        .with_context(|| format!("rename {} → {}", s2db_tmp.display(), s2db_path.display()))?;
    let shas_tmp = shas_path.with_extension("shas.tmp");
    {
        use std::io::Write;
        let f = std::fs::File::create(&shas_tmp)
            .with_context(|| format!("create {}", shas_tmp.display()))?;
        let mut w = std::io::BufWriter::new(f);
        for s in shas { writeln!(w, "{s}")?; }
        w.flush()?;
        w.get_mut().sync_all().context("fsync shas")?;
    }
    std::fs::rename(&shas_tmp, shas_path)
        .with_context(|| format!("rename {} → {}", shas_tmp.display(), shas_path.display()))?;
    Ok(())
}

/// Path of the rolling snapshot that `--resume` reads. Lives next to
/// the final output so it doesn't get lost across runs.
fn partial_s2db_for(out: &std::path::Path) -> std::path::PathBuf {
    let mut p = out.to_path_buf();
    let stem = p.file_name().map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    p.set_file_name(format!("{stem}.partial.s2db"));
    p
}

/// Path of the per-CU sha checkpoint. One sha per line; written
/// atomically (write to `.tmp` then rename) each time the matching
/// snapshot lands.
fn partial_shas_for(out: &std::path::Path) -> std::path::PathBuf {
    let mut p = out.to_path_buf();
    let stem = p.file_name().map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    p.set_file_name(format!("{stem}.partial.shas"));
    p
}

fn cmd_from_kzip(args: FromKzipArgs<'_>) -> Result<()> {
    use std::process::Stdio;
    use std::collections::HashSet;

    let t0 = Instant::now();
    let want: HashSet<&str> = args.langs.split(',').map(|s| s.trim()).collect();

    eprintln!("[from-kzip] reading {} …", args.kzip.display());
    let in_filters: &[String] = args.in_;
    let not_in_filters: &[String] = args.not_in;
    // A pure path predicate so the kzip walker can short-circuit
    // before the full proto/JSON decode. Empty filter strings are
    // no-ops (match scry's conservative empty-semantic so an
    // upstream that forwards Option<String> without trimming doesn't
    // silently reject every CU).
    let accept_path = |p: &str| -> bool {
        if !in_filters.is_empty()
            && !in_filters.iter().any(|s| !s.is_empty() && p.contains(s.as_str()))
        { return false; }
        if not_in_filters.iter().any(|s| !s.is_empty() && p.contains(s.as_str())) {
            return false;
        }
        true
    };
    let units = if in_filters.is_empty() && not_in_filters.is_empty() {
        // No path filter — full decode every CU. (Most users running
        // a scoped index will pass --in.)
        kzip::read_units_progress(args.kzip, CliProgress::new("from-kzip"))?
    } else {
        // Cheap peek path: only fully decode CUs whose primary path
        // matches the filter. On AOSP this is the difference between
        // ~3 min (read all 118 k) and ~30 s (peek all, decode the few
        // hundred that match `--in frameworks/base,...`).
        kzip::read_units_filtered(args.kzip, CliProgress::new("from-kzip"), accept_path)?
    };
    eprintln!("[from-kzip] {} units kept after path filter", units.len());

    // Language filter: route_language drops anything we don't have
    // an indexer for (kotlin / rust in v0.0.75); `--langs` further
    // restricts. Path filter is already applied by the walker above
    // when set; we still re-check here so the per-CU code path is
    // uniform whether or not the walker did the peek.
    let mut plan: Vec<(IndexerKind, &kzip::Unit)> = Vec::with_capacity(units.len());
    let mut skipped_lang = 0usize;
    let mut skipped_path = 0usize;
    for u in &units {
        let Some(kind) = route_language(&u.language()) else { skipped_lang += 1; continue };
        if !want.contains(lang_label(kind)) { skipped_lang += 1; continue; }
        let p = u.primary_path().unwrap_or("");
        if !accept_path(p) { skipped_path += 1; continue; }
        plan.push((kind, u));
    }
    eprintln!("[from-kzip] plan: {} CUs to index ({} skipped: lang={}, path={})",
        plan.len(), skipped_lang + skipped_path, skipped_lang, skipped_path);
    if plan.is_empty() { anyhow::bail!("from-kzip: nothing to index after filters"); }

    // Staging dir — one process-local subdir under SCRY_TMP_DIR or
    // /mnt/agent/tmp. Cleaned up at the end.
    let staging = args.staging.map(|p| p.to_path_buf()).unwrap_or_else(|| {
        let base = std::env::var_os("SCRY_TMP_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("/mnt/agent/tmp"));
        base.join(format!("scry2-from-kzip-{}", std::process::id()))
    });
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("mkdir staging {}", staging.display()))?;
    eprintln!("[from-kzip] staging dir: {}", staging.display());

    let workers = if args.workers == 0 {
        std::cmp::max(1, num_cpus_get() / 2)
    } else { args.workers };
    eprintln!("[from-kzip] workers: {workers}");

    // --- Resume scaffolding -----------------------------------------------
    //
    // Partial state lives next to the final output. A killed run leaves
    // `<out>.partial.s2db` (the most recent rolling snapshot) and
    // `<out>.partial.shas` (one CU sha per line, all of which are
    // baked into that snapshot). With `--resume` we load both and
    // skip already-folded CUs.
    let partial_s2db_path = partial_s2db_for(args.out);
    let partial_shas_path = partial_shas_for(args.out);
    let mut starting_builder = IndexBuilder::new();
    let mut done_shas: std::collections::HashSet<String> = std::collections::HashSet::new();
    if args.resume {
        match (partial_s2db_path.exists(), partial_shas_path.exists()) {
            (true, true) => {
                eprintln!("[from-kzip] --resume: loading {} + {}",
                    partial_s2db_path.display(), partial_shas_path.display());
                let ix = scry2::reader::Index::open(&partial_s2db_path)
                    .with_context(|| format!("open partial {}", partial_s2db_path.display()))?;
                starting_builder.populate_from_index(&ix)
                    .with_context(|| "replay partial snapshot")?;
                let shas = std::fs::read_to_string(&partial_shas_path)
                    .with_context(|| format!("read {}", partial_shas_path.display()))?;
                for line in shas.lines() {
                    let s = line.trim();
                    if !s.is_empty() { done_shas.insert(s.to_string()); }
                }
                eprintln!("[from-kzip] --resume: loaded {} prior CUs",
                    done_shas.len());
            }
            (false, false) => {
                eprintln!("[from-kzip] --resume: no partial state found at {}; starting fresh",
                    partial_s2db_path.display());
            }
            // Either-or means an aborted snapshot. Refuse to silently
            // half-resume — the user almost certainly wants to know.
            _ => anyhow::bail!(
                "--resume: partial state is incomplete ({} present={}, {} present={})",
                partial_s2db_path.display(), partial_s2db_path.exists(),
                partial_shas_path.display(), partial_shas_path.exists(),
            ),
        }
    }
    let before_filter = plan.len();
    plan.retain(|(_, u)| !done_shas.contains(&u.sha));
    let skipped_resume = before_filter - plan.len();
    if skipped_resume > 0 {
        eprintln!("[from-kzip] --resume: skipped {skipped_resume} CUs (already snapshotted)");
    }
    if plan.is_empty() {
        eprintln!("[from-kzip] --resume: every CU was already ingested; promoting partial to final");
        std::fs::rename(&partial_s2db_path, args.out)
            .with_context(|| format!("rename {} → {}",
                partial_s2db_path.display(), args.out.display()))?;
        let _ = std::fs::remove_file(&partial_shas_path);
        return Ok(());
    }

    // Per-worker sinks (builder + pending shas, atomically swappable).
    // AOSP CUs carry 600k+ entries (~8 s of ingest wall each); with a
    // shared `Mutex<IndexBuilder>` 36 workers all parked in
    // futex_wait_queue and throughput pegged at ~7 CUs/min. Each
    // worker now owns its sink, so the per-CU ingest holds only its
    // own lock — uncontended in the normal path.
    let workers_n = workers;
    let worker_sinks: Vec<std::sync::Mutex<WorkerSink>> = (0..workers_n)
        .map(|_| std::sync::Mutex::new(WorkerSink::default()))
        .collect();
    // The accumulator carries the snapshot history. On `--resume` it's
    // seeded with the partial replay plus its sha set; otherwise it
    // starts empty. Workers never touch this mutex — only the active
    // snapshotter does, so contention is rare.
    let accumulator_mu = std::sync::Mutex::new(Accumulator {
        builder:        starting_builder,
        committed_shas: done_shas,
    });
    // `FileIdAllocator` is shared by-reference (interior mutex inside
    // `intern`). Workers hit it once per file path during ingest,
    // not once per CU — there's no whole-CU lock to serialize on.
    let file_ids = kythe::FileIdAllocator::default();
    let by_lang_mu: std::sync::Mutex<std::collections::HashMap<&'static str, LangStats>> =
        std::sync::Mutex::new(std::collections::HashMap::new());
    // Atomic progress counter so workers can report a coherent
    // completed-CU count.
    let done = std::sync::atomic::AtomicUsize::new(0);
    let progress_mu = std::sync::Mutex::new(CliProgress::new("from-kzip"));
    // Snapshot serialization. `snap_writer_mu` ensures only one
    // snapshotter writes at a time; `last_snap_done` is the value of
    // `done` at the last triggered snapshot, used by workers to decide
    // (lock-free) whether to attempt a snapshot. Workers never block on
    // these in the normal path — the trigger check is an atomic load,
    // and only the would-be snapshotter even tries `snap_writer_mu`.
    let snap_writer_mu = std::sync::Mutex::new(());
    let last_snap_done = std::sync::atomic::AtomicUsize::new(0);

    // Split the plan into N work shards by index. Static partition
    // is simpler than a work-stealing queue and load-balances well
    // because CU runtime is fairly uniform within a language family.
    let n_workers = workers_n;
    let plan_ref = &plan;
    let partial_s2db_path_ref = &partial_s2db_path;
    let partial_shas_path_ref = &partial_shas_path;
    let accumulator_mu_ref    = &accumulator_mu;
    let worker_sinks_ref      = &worker_sinks;
    let snap_writer_mu_ref    = &snap_writer_mu;
    let last_snap_done_ref    = &last_snap_done;
    let snapshot_every        = args.snapshot_every;
    std::thread::scope(|s| -> Result<()> {
        let mut handles = Vec::with_capacity(n_workers);
        for w_id in 0..n_workers {
            let staging = staging.clone();
            let inject_rules = args.inject_rules;
            let kythe_root = args.kythe_root;
            let jvm_heap = args.jvm_heap.to_string();
            let my_sink     = &worker_sinks_ref[w_id];
            let all_sinks   = worker_sinks_ref;
            let accumulator_mu = accumulator_mu_ref;
            let snap_writer_mu = snap_writer_mu_ref;
            let last_snap_done = last_snap_done_ref;
            let file_ids = &file_ids;
            let by_lang_mu  = &by_lang_mu;
            let done = &done;
            let progress_mu = &progress_mu;
            let plan_len = plan_ref.len();
            let kzip_path = args.kzip;
            let partial_s2db_path = partial_s2db_path_ref;
            let partial_shas_path = partial_shas_path_ref;
            handles.push(s.spawn(move || -> Result<()> {
                let mut extractor = kzip::SubKzipWriter::open(kzip_path)?;
                let mut i = w_id;
                while i < plan_len {
                    let (kind, unit) = plan_ref[i];
                    let label = lang_label(kind);
                    let sub_path = staging.join(format!("{}.kzip", unit.sha));
                    let jvm_tmp = staging.join(format!("{}.jvmtmp", unit.sha));
                    // RAII cleanup: the file/dir is removed when the
                    // guards go out of scope at the end of this loop
                    // body — including on panic. Holding the guards
                    // even when the paths don't yet exist is harmless
                    // (remove_*  on a missing path is a no-op for us).
                    let _sub_guard = CleanupPath { path: sub_path.clone(), is_dir: false };
                    let _jvm_guard = matches!(kind, IndexerKind::JavaSource | IndexerKind::JvmBytecode)
                        .then(|| {
                            let _ = std::fs::create_dir_all(&jvm_tmp);
                            CleanupPath { path: jvm_tmp.clone(), is_dir: true }
                        });
                    let primary = unit.primary_path().unwrap_or("");
                    let matching: Vec<&str> = inject_rules.iter()
                        .filter(|r| primary.starts_with(&r.path_prefix))
                        .map(|r| r.arg.as_str())
                        .collect();
                    let extract_res = if matching.is_empty() {
                        extractor.extract(unit, &sub_path)
                    } else {
                        extractor.extract_with(unit, &sub_path, |cu| {
                            for &a in matching.iter().rev() {
                                if !cu.argument.iter().any(|existing| existing == a) {
                                    cu.argument.insert(0, a.to_string());
                                }
                            }
                        })
                    };
                    if let Err(e) = extract_res {
                        let mut by_lang = by_lang_mu.lock().unwrap();
                        let stats = by_lang.entry(label).or_default();
                        stats.failed += 1;
                        if stats.fail_tails.len() < MAX_FAIL_TAILS {
                            stats.fail_tails.push(format!("sha={} extract: {e:#}", unit.sha));
                        }
                        i += n_workers;
                        continue;
                    }
                    let mut cmd = build_indexer_command(kind, kythe_root, &sub_path,
                                                       &jvm_heap, &jvm_tmp)?;
                    let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
                        Ok(c) => c,
                        Err(e) => {
                            let mut by_lang = by_lang_mu.lock().unwrap();
                            let stats = by_lang.entry(label).or_default();
                            stats.failed += 1;
                            if stats.fail_tails.len() < MAX_FAIL_TAILS {
                                stats.fail_tails.push(format!("sha={} spawn: {e:#}", unit.sha));
                            }
                            i += n_workers;
                            continue;
                        }
                    };
                    let stderr_h = child.stderr.take().unwrap();
                    let stderr_thread = std::thread::spawn(move || drain_tail(stderr_h, STDERR_TAIL_BYTES));
                    let stdout_h = child.stdout.take().unwrap();
                    let cu_t0 = Instant::now();
                    // Lock the shared builder for the duration of
                    // this CU's stream. The indexer subprocess writes
                    // a few hundred MB max for a typical CU; ingesting
                    // it is bounded by stream parse speed (~hundreds
                    // of MB/s), not the indexer subprocess. Other
                    // workers' subprocesses keep running while we hold
                    // the lock.
                    // Ingest into THIS worker's local sink — no
                    // cross-worker contention on the builder side.
                    // The same lock guards `pending_shas`, so a
                    // successful CU's sha is committed to the sink
                    // atomically with its data; the snapshotter then
                    // takes both via one `mem::take` and never sees
                    // a "data without sha" gap (which would re-run
                    // already-applied CUs on resume).
                    //
                    // `file_ids` is shared by-reference: its interior
                    // mutex is taken only per intern (O(hash)), not
                    // for the whole CU, so workers don't serialize.
                    let ingest_res = {
                        let mut sink = my_sink.lock().unwrap();
                        scry2::kythe::ingest_tolerant(stdout_h, &mut sink.builder, file_ids, true)
                    };
                    // On ingest failure the subprocess may still be
                    // writing; killing it now unblocks the pipe and
                    // lets `wait()` return so we can reap the child
                    // (and the stderr drain thread) instead of
                    // leaking the worker.
                    if ingest_res.is_err() { let _ = child.kill(); }
                    let wait_res = child.wait();
                    // join() Err means the drain thread panicked
                    // (corrupted stderr stream, OOM in the ring
                    // buffer, etc.) — record that rather than
                    // silently dropping the tail.
                    let stderr_tail = match stderr_thread.join() {
                        Ok(v) => v,
                        Err(_) => b"<stderr drain thread panicked>".to_vec(),
                    };
                    // `_sub_guard` / `_jvm_guard` clean the paths at
                    // end-of-scope (and on panic) — no explicit
                    // remove_* needed here.

                    // Aggregate stats + record any failure tail.
                    // Every per-CU outcome — ingest error, wait
                    // error, non-zero exit, empty stream — lands
                    // here; no path silently drops a failure.
                    let elapsed = cu_t0.elapsed().as_secs_f64();
                    let exit_code = wait_res.as_ref().ok().and_then(|s| s.code());
                    let exit_ok   = wait_res.as_ref().map(|s| s.success()).unwrap_or(false);
                    let any_entries = ingest_res.as_ref().map(|cu| cu.entries > 0).unwrap_or(false);
                    let mut by_lang = by_lang_mu.lock().unwrap();
                    let stats = by_lang.entry(label).or_default();
                    stats.cus += 1;
                    if let Ok(cu) = &ingest_res {
                        stats.entries  += cu.entries;
                        stats.anchors  += cu.anchors_flushed;
                        stats.xrefs    += cu.xrefs_emitted;
                        stats.inherits += cu.inherits_emitted;
                        stats.aliases  += cu.aliases_emitted;
                        stats.calls    += cu.calls_emitted;
                    }
                    let failed = ingest_res.is_err() || wait_res.is_err() || !exit_ok;
                    if failed {
                        stats.failed += 1;
                        if stats.fail_tails.len() < MAX_FAIL_TAILS {
                            let mut head = format!(
                                "sha={} exit={exit_code:?} wall={elapsed:.1}s",
                                unit.sha,
                            );
                            if let Err(e) = &ingest_res {
                                head.push_str(&format!("\ningest-error: {e:#}"));
                            }
                            if let Err(e) = &wait_res {
                                head.push_str(&format!("\nwait-error: {e:#}"));
                            }
                            let tail = String::from_utf8_lossy(&stderr_tail);
                            let tail = tail.trim();
                            if !tail.is_empty() { head.push('\n'); head.push_str(tail); }
                            stats.fail_tails.push(head);
                        }
                    } else if any_entries {
                        stats.succeeded += 1;
                    } else {
                        stats.empty += 1;
                    }
                    drop(by_lang);
                    // Register the sha *inside* my_sink (same lock as
                    // the builder) — guarantees the on-disk
                    // `.partial.shas` never names a CU whose data
                    // isn't in `.partial.s2db`. Only successful CUs
                    // (had entries, ingested cleanly, exited 0) earn
                    // a sha; failures and empties are safe to re-run.
                    let cu_succeeded = !failed && any_entries;
                    if cu_succeeded {
                        my_sink.lock().unwrap().pending_shas.push(unit.sha.clone());
                    }
                    let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    progress_mu.lock().unwrap().report("index", n, plan_len);

                    // Snapshot trigger — lock-free check. If the
                    // threshold is crossed and we can grab the writer
                    // lock, we become the snapshotter. Other workers
                    // never block on this path: they only contend
                    // when *they* are also trying to write a snapshot,
                    // and `try_lock` makes a losing candidate fall
                    // through immediately.
                    use std::sync::atomic::Ordering;
                    if snapshot_every > 0 && n >= last_snap_done.load(Ordering::Relaxed) + snapshot_every {
                        if let Ok(_writer_guard) = snap_writer_mu.try_lock() {
                            // Re-check under the writer lock so a
                            // racing worker that snuck in first
                            // doesn't double-snapshot.
                            if n >= last_snap_done.load(Ordering::Relaxed) + snapshot_every {
                                last_snap_done.store(n, Ordering::Relaxed);
                                // Drain every worker sink we can
                                // grab via `try_lock`. Sinks mid-CU
                                // (lock held by the ingesting worker)
                                // are SKIPPED — their data + shas
                                // stay together in the sink and land
                                // in the NEXT snapshot. This is the
                                // load-bearing guarantee: a single
                                // 25-min mega-CU never blocks the
                                // snapshot from making progress, so
                                // memory pressure releases on every
                                // tick and the partial files always
                                // advance.
                                //
                                // Correctness: each sink's
                                // (builder, pending_shas) pair is
                                // atomic — skipping the sink keeps
                                // them together. On resume the
                                // mid-CU worker's CUs aren't in the
                                // shas file, so they get re-run.
                                // Idempotent.
                                let mut drained_data = IndexBuilder::new();
                                let mut drained_shas: Vec<String> = Vec::new();
                                let mut drained_n = 0usize;
                                let mut skipped_n = 0usize;
                                for ws in all_sinks {
                                    match ws.try_lock() {
                                        Ok(mut sink) => {
                                            let taken_builder = std::mem::take(&mut sink.builder);
                                            let taken_shas    = std::mem::take(&mut sink.pending_shas);
                                            drop(sink);
                                            drained_data.merge_from(taken_builder);
                                            drained_shas.extend(taken_shas);
                                            drained_n += 1;
                                        }
                                        Err(_) => { skipped_n += 1; }
                                    }
                                }
                                // Fold drained state into the
                                // accumulator + capture a clone for
                                // the snapshot write. The accumulator
                                // lock is only contended by snapshot
                                // writers (and we hold snap_writer_mu
                                // so there's at most one of us at a
                                // time), so it's effectively
                                // uncontended.
                                let (cloned, shas) = {
                                    let mut acc = accumulator_mu.lock().unwrap();
                                    acc.builder.merge_from(drained_data);
                                    for s in drained_shas { acc.committed_shas.insert(s); }
                                    file_ids.push_to(&mut acc.builder);
                                    let cloned = acc.builder.clone();
                                    let mut shas: Vec<String> = acc.committed_shas
                                        .iter().cloned().collect();
                                    shas.sort();
                                    (cloned, shas)
                                };
                                if let Err(e) = write_snapshot(cloned, &shas,
                                    partial_s2db_path, partial_shas_path)
                                {
                                    eprintln!("[from-kzip] snapshot @ {n}/{plan_len} failed (sinks drained={drained_n}/{}, busy={skipped_n}): {e:#}",
                                        drained_n + skipped_n);
                                } else {
                                    eprintln!("[from-kzip] snapshot @ {n}/{plan_len}: {} shas durable (sinks drained={drained_n}/{}, busy={skipped_n})",
                                        shas.len(), drained_n + skipped_n);
                                }
                            }
                            // _writer_guard releases here.
                        }
                    }
                    i += n_workers;
                }
                Ok(())
            }));
        }
        for h in handles {
            h.join().map_err(|_| anyhow::anyhow!("worker thread panicked"))??;
        }
        Ok(())
    })?;

    // Drain any rows the workers ingested since the last snapshot
    // into the accumulator, then take ownership of the accumulator's
    // builder for the final write. After std::thread::scope exits
    // every worker thread is joined, so the per-worker mutexes have
    // no other reference — into_inner() is the right primitive.
    let acc = accumulator_mu.into_inner().unwrap();
    let mut builder = acc.builder;
    for ws in worker_sinks {
        let sink = ws.into_inner().unwrap();
        builder.merge_from(sink.builder);
        // We don't write a final .shas (the final s2db at args.out
        // is the authoritative deliverable; the partial files get
        // removed below), so dropped pending_shas are harmless.
    }
    let by_lang     = by_lang_mu.into_inner().unwrap();

    // Report per-language summary + first failure tails.
    for (label, s) in &by_lang {
        eprintln!(
            "[from-kzip] {label}: CUs={} (ok={} empty={} failed={}) entries={} anchors={} xrefs={} inh={} alias={} calls={}",
            s.cus, s.succeeded, s.empty, s.failed,
            s.entries, s.anchors, s.xrefs, s.inherits, s.aliases, s.calls,
        );
        for (i, tail) in s.fail_tails.iter().enumerate() {
            eprintln!("[from-kzip] {label} failure {}/{}:", i + 1, s.fail_tails.len());
            for line in tail.lines() { eprintln!("    {line}"); }
        }
    }

    file_ids.drain_into(&mut builder);
    eprintln!("[from-kzip] writing — xrefs={} syms={} files={} inhs={} calls={}",
        builder.n_xrefs(), builder.n_syms(), builder.n_files(),
        builder.n_inh(), builder.n_calls());
    let bytes = builder.finish(args.out)?;
    // Final write succeeded — discard the rolling partial files so
    // the next from-kzip invocation against this `--out` doesn't
    // pick up a stale snapshot.
    let _ = std::fs::remove_file(&partial_s2db_path);
    let _ = std::fs::remove_file(&partial_shas_path);
    let _ = std::fs::remove_dir_all(&staging);
    eprintln!("[from-kzip] done in {:.2}s → {} ({:.2} GB)",
        t0.elapsed().as_secs_f64(), args.out.display(), bytes as f64 / 1e9);
    Ok(())
}

/// RAII path cleanup. Tracks a sub-kzip file or jvm tmp dir so a panic
/// inside the per-CU body — `ingest_tolerant` choking on a corrupted
/// stream, a builder mutex poisoned by another worker, an OOM during
/// stderr buffering — still removes the path on unwind. Without it,
/// killing a from-kzip run mid-shard leaks one tmpfile per in-flight
/// worker (8–72 files on the AOSP run).
struct CleanupPath {
    path: std::path::PathBuf,
    is_dir: bool,
}

impl Drop for CleanupPath {
    fn drop(&mut self) {
        if self.is_dir {
            let _ = std::fs::remove_dir_all(&self.path);
        } else {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Drain `r` and return the LAST `n` bytes — used to capture indexer
/// stderr tails for failure diagnosis without buffering 10+ MB of
/// INFO output from a noisy CU.
fn drain_tail<R: std::io::Read>(mut r: R, n: usize) -> Vec<u8> {
    let mut ring = Vec::<u8>::with_capacity(n.min(64 << 10));
    let mut buf = [0u8; 8192];
    while let Ok(k) = r.read(&mut buf) {
        if k == 0 { break; }
        if ring.len() + k <= n {
            ring.extend_from_slice(&buf[..k]);
        } else {
            // Discard the oldest bytes; keep last n.
            let combined_len = ring.len() + k;
            let drop = combined_len - n;
            if drop >= ring.len() {
                let offset = drop - ring.len();
                ring.clear();
                ring.extend_from_slice(&buf[offset..k]);
            } else {
                ring.drain(..drop);
                ring.extend_from_slice(&buf[..k]);
            }
        }
    }
    ring
}

/// Lightweight CPU-count helper. `num_cpus` crate would do the same
/// thing but we want zero extra deps for this single call site.
fn num_cpus_get() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::expand_tilde;
    use std::ffi::OsStr;
    use std::path::PathBuf;

    fn h(s: &str) -> Option<&OsStr> { Some(OsStr::new(s)) }

    use super::parse_inject_rules;

    #[test]
    fn inject_rules_basic() {
        let rules = parse_inject_rules(&[
            "libcore/ojluni/src/main/java/::--patch-module=java.base=libcore/ojluni/src/main/java".into(),
        ]).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].path_prefix, "libcore/ojluni/src/main/java/");
        assert_eq!(rules[0].arg, "--patch-module=java.base=libcore/ojluni/src/main/java");
    }

    #[test]
    fn inject_rules_first_double_colon_splits() {
        // ARG may contain single `:` (e.g. `-J:option`); only the
        // FIRST `::` is the separator.
        let rules = parse_inject_rules(&["foo/::-Djava.opts=key:value".into()]).unwrap();
        assert_eq!(rules[0].path_prefix, "foo/");
        assert_eq!(rules[0].arg, "-Djava.opts=key:value");
    }

    #[test]
    fn inject_rules_reject_malformed() {
        // Missing separator.
        assert!(parse_inject_rules(&["nope".into()]).is_err());
        // Empty PREFIX.
        assert!(parse_inject_rules(&["::-something".into()]).is_err());
        // Empty ARG.
        assert!(parse_inject_rules(&["prefix/::".into()]).is_err());
    }

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

    use super::CleanupPath;

    fn unique_tmp(stem: &str) -> PathBuf {
        let dir = std::env::var("SCRY_TMP_DIR").unwrap_or_else(|_| "/mnt/agent/tmp".into());
        // `ThreadId::as_u64` is nightly-only; the Debug repr is stable
        // and contains an opaque per-thread integer, which is enough
        // to disambiguate parallel test threads.
        PathBuf::from(dir).join(format!(
            "scry2-cleanup-{stem}-{}-{:?}",
            std::process::id(),
            std::thread::current().id(),
        ).replace(['(', ')', ' '], "_"))
    }

    #[test]
    fn cleanup_path_removes_file_on_drop() {
        let p = unique_tmp("file");
        std::fs::write(&p, b"transient").unwrap();
        assert!(p.exists());
        {
            let _g = CleanupPath { path: p.clone(), is_dir: false };
            assert!(p.exists(), "guard does not remove eagerly");
        }
        assert!(!p.exists(), "guard removes on drop");
    }

    #[test]
    fn cleanup_path_removes_dir_on_drop() {
        let p = unique_tmp("dir");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("inner"), b"child").unwrap();
        {
            let _g = CleanupPath { path: p.clone(), is_dir: true };
        }
        assert!(!p.exists(), "guard removes dir (and contents) on drop");
    }

    #[test]
    fn cleanup_path_missing_target_is_noop() {
        // Even if the path was never created (extract failed before
        // the indexer wrote the file), Drop must not panic.
        let p = unique_tmp("never-existed");
        let _g = CleanupPath { path: p.clone(), is_dir: false };
        drop(_g);
        // No assertion needed; not panicking is the success condition.
    }

    #[test]
    fn cleanup_path_drops_even_on_unwind() {
        // The whole point of the guard is panic-safety. Trigger a
        // panic inside a `catch_unwind` while the guard is in scope
        // and verify the path is cleaned afterwards.
        let p = unique_tmp("panic");
        std::fs::write(&p, b"data").unwrap();
        let p2 = p.clone();
        let _ = std::panic::catch_unwind(move || {
            let _g = CleanupPath { path: p2, is_dir: false };
            panic!("simulated worker crash");
        });
        assert!(!p.exists(), "guard fired during unwind");
    }
}
