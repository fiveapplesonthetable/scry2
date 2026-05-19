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
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Build-side verbs don't go through Reply.
    match cli.cmd {
        Cmd::Index { entries, out }  => return cmd_index(&entries, &out),
        Cmd::NormalizeKzip { in_, out } => return cmd_normalize_kzip(&in_, &out),
        Cmd::FromKzip { kzip, kythe_root, out, langs, jvm_heap,
                        in_, not_in, staging, workers } =>
            return cmd_from_kzip(FromKzipArgs {
                kzip: &kzip, kythe_root: &kythe_root, out: &out,
                langs: &langs, jvm_heap: &jvm_heap,
                in_: &in_, not_in: &not_in,
                staging: staging.as_deref(), workers,
            }),
        Cmd::Serve { socket } => {
            let sock = socket.unwrap_or_else(|| server::default_socket_for(&cli.index));
            return server::serve(&cli.index, &sock);
        }
        Cmd::Repl => return server::repl(&cli.index),
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
    kzip:       &'a std::path::Path,
    kythe_root: &'a std::path::Path,
    out:        &'a std::path::Path,
    langs:      &'a str,
    jvm_heap:   &'a str,
    in_:        &'a [String],
    not_in:     &'a [String],
    staging:    Option<&'a std::path::Path>,
    workers:    usize,
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

fn cmd_from_kzip(args: FromKzipArgs<'_>) -> Result<()> {
    use std::process::Stdio;
    use std::collections::HashSet;

    let t0 = Instant::now();
    let want: HashSet<&str> = args.langs.split(',').map(|s| s.trim()).collect();

    eprintln!("[from-kzip] reading {} …", args.kzip.display());
    let units = kzip::read_units_progress(args.kzip, CliProgress::new("from-kzip"))?;
    eprintln!("[from-kzip] {} units total", units.len());

    // CU-level filter: (a) want this language, (b) path matches --in/--not-in.
    let in_filters:  &[String] = args.in_;
    let not_in_filters: &[String] = args.not_in;
    let primary_ok = |u: &kzip::Unit| -> bool {
        if in_filters.is_empty() && not_in_filters.is_empty() { return true; }
        let p = u.primary_path().unwrap_or("");
        if !in_filters.is_empty()
            && !in_filters.iter().any(|s| !s.is_empty() && p.contains(s.as_str()))
        { return false; }
        if not_in_filters.iter().any(|s| !s.is_empty() && p.contains(s.as_str())) {
            return false;
        }
        true
    };
    let mut plan: Vec<(IndexerKind, &kzip::Unit)> = Vec::with_capacity(units.len());
    let mut skipped_lang = 0usize;
    let mut skipped_path = 0usize;
    for u in &units {
        let Some(kind) = route_language(&u.language()) else { skipped_lang += 1; continue };
        if !want.contains(lang_label(kind)) { skipped_lang += 1; continue; }
        if !primary_ok(u) { skipped_path += 1; continue; }
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

    let mut builder = IndexBuilder::new();
    let mut file_ids = kythe::FileIdAllocator::default();
    let mut by_lang: std::collections::HashMap<&'static str, LangStats> =
        std::collections::HashMap::new();

    // Sequential per-CU dispatch with one reusable SubKzipWriter
    // (opens the source kzip once and reads each CU's blobs on demand).
    // Parallelism is deferred — sequential at ~1-3s/CU is already
    // bounded by the indexer subprocess; the gain from N workers
    // shows up after we add a thread pool that shares one
    // SubKzipWriter and one IndexBuilder behind a Mutex (v0.2).
    let _ = workers; // reserved for v0.2 parallelism
    let mut extractor = kzip::SubKzipWriter::open(args.kzip)?;
    let mut progress = CliProgress::new("from-kzip");
    for (i, (kind, unit)) in plan.iter().enumerate() {
        let label = lang_label(*kind);
        let stats = by_lang.entry(label).or_default();
        let sub_path = staging.join(format!("{}.kzip", unit.sha));
        // Reusable JVM temp dir per CU — java_indexer needs writable
        // dir to unpack JDK system modules from --system <jdk_image>.
        let jvm_tmp = staging.join(format!("{}.jvmtmp", unit.sha));
        if matches!(kind, IndexerKind::JavaSource | IndexerKind::JvmBytecode) {
            std::fs::create_dir_all(&jvm_tmp)?;
        }
        // Build sub-kzip.
        if let Err(e) = extractor.extract(unit, &sub_path) {
            stats.failed += 1;
            stats.fail_tails.push(format!("sha={} extract: {e:#}", unit.sha));
            continue;
        }
        // Spawn indexer.
        let mut cmd = build_indexer_command(*kind, args.kythe_root, &sub_path,
                                            args.jvm_heap, &jvm_tmp)?;
        let mut child = match cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(c) => c,
            Err(e) => {
                stats.failed += 1;
                stats.fail_tails.push(format!("sha={} spawn: {e:#}", unit.sha));
                let _ = std::fs::remove_file(&sub_path);
                let _ = std::fs::remove_dir_all(&jvm_tmp);
                continue;
            }
        };
        // Drain stderr in a thread to avoid blocking the indexer on
        // its stderr pipe when we're sinking stdout into ingest.
        let stderr_h = child.stderr.take().unwrap();
        let stderr_thread = std::thread::spawn(move || drain_tail(stderr_h, STDERR_TAIL_BYTES));
        let stdout_h = child.stdout.take().unwrap();

        let cu_t0 = Instant::now();
        let cu_stats = scry2::kythe::ingest_tolerant(stdout_h, &mut builder, &mut file_ids, true)?;
        let status = child.wait()?;
        let stderr_tail = stderr_thread.join().unwrap_or_default();
        let _ = std::fs::remove_file(&sub_path);
        let _ = std::fs::remove_dir_all(&jvm_tmp);

        stats.cus += 1;
        stats.entries  += cu_stats.entries;
        stats.anchors  += cu_stats.anchors_flushed;
        stats.xrefs    += cu_stats.xrefs_emitted;
        stats.inherits += cu_stats.inherits_emitted;
        stats.aliases  += cu_stats.aliases_emitted;
        stats.calls    += cu_stats.calls_emitted;
        let ok = status.success();
        let any = cu_stats.entries > 0;
        match (ok, any) {
            (true,  true)  => stats.succeeded += 1,
            (true,  false) => stats.empty     += 1,
            (false, _)     => {
                stats.failed += 1;
                if stats.fail_tails.len() < MAX_FAIL_TAILS {
                    let tail = String::from_utf8_lossy(&stderr_tail).into_owned();
                    stats.fail_tails.push(format!(
                        "sha={} exit={:?} wall={:.1}s\n{}",
                        unit.sha, status.code(),
                        cu_t0.elapsed().as_secs_f64(),
                        tail.trim(),
                    ));
                }
            }
        }
        progress.report("index", i + 1, plan.len());
    }

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
    let _ = std::fs::remove_dir_all(&staging);
    eprintln!("[from-kzip] done in {:.2}s → {} ({:.2} GB)",
        t0.elapsed().as_secs_f64(), args.out.display(), bytes as f64 / 1e9);
    Ok(())
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
