# scry2 vs Kythe serving tables

scry2 is built on Kythe's indexers and serves a slice of Kythe's graph.
It does not replace Kythe's own serving stack. This page is the decision
guide for which to reach for.

## Use scry2 when

* You want a **single self-contained file**. One mmap'd `.s2db` is the
  whole index — no daemon, no database server, no sidecars to keep in
  sync. Copy the file, query it.
* You need **microsecond warm queries**. Every lookup is a binary search
  over sorted, big-endian-packed rows; warm point lookups are single-digit
  µs, with no allocator, parser, or syscall on the hot path.
* You want **substring search** over symbol names without a separate
  index. `--substr` is a parallel linear scan (`memchr::memmem`) over the
  names table, case-sensitive by default with `-i` for case-insensitive,
  bounded by a per-call cap.
* You want a **compact verb set for navigation**: `def`, `ref`,
  `callers`, `callgraph`, `super`, `sub`, `inheritance`, `type`, `sig`,
  `members`, `names`. This is the surface an LLM or agent needs to walk a
  code graph, exposed over CLI, a stdin/stdout JSON REPL, and a Unix
  socket — all with an identical wire shape.

## Use Kythe's serving tables when

* You need the **full Kythe edge model**. scry2 ingests a deliberate
  subset (see [LIMITS.md](LIMITS.md)); the long tail —
  `influences`/dataflow, `instantiates`/`specializes`, macro
  `ref/expands`, `imports`, typedef/alias edges — lives only in Kythe.
* You need **cross-corpus** resolution or the canonical decor/xref
  protocol that Kythe's UI and tools speak.
* You want the **source of truth**. scry2 stores and serves whatever the
  indexers emit, optimized for fast navigation; Kythe's serving tables
  (`write_tables`) are the authoritative, complete representation.

## In short

scry2 is a fast, narrow navigation layer over Kythe data, packaged as one
mmap file. When the question fits its verb set, scry2 answers in
microseconds. When the question needs the full edge model, cross-corpus
joins, or canonical completeness, query Kythe directly.
