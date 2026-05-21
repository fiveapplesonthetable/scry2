# scry2 — known correctness limits

scry2 is honest about what it can and cannot answer precisely. Each of
the following is a property of the current design, documented so a caller
(human or LLM) knows where the edges are and what to reach for instead.

## Body-anchor call attribution is best-effort

`callers` and `callgraph` attribute each call site to its innermost
enclosing function. The attribution is `innermost_containing` in
`crates/scry2/src/kythe.rs`: at ingest, every `/kythe/edge/defines` (not
`defines/binding`) anchor records a body range `(file, start, end, sym)`;
each ref/call site at `(file, offset)` is then matched to the smallest
body range that contains it via an interval search over the file's sorted
body anchors. Innermost wins, so a call inside a lambda is attributed to
the lambda rather than the surrounding method.

This is a heuristic over Kythe's emitted anchor spans, not a proof.
Pathological nesting or overlapping body ranges (degenerate macro
expansions, generated code with synthetic anchors) can mis-attribute a
call to the wrong enclosing function. When a call site falls inside no
body anchor at all, it produces an xref but no `calls` row, so it is
simply absent from the callgraph rather than mis-attributed.

## scry2 ingests a deliberate subset of Kythe edges

The ingest path (`crates/scry2/src/kythe.rs`) consumes only the edges the
query verbs need:

* `defines/binding` → DECL, `defines` → DEF, `ref` / `ref/writes` /
  `ref/imports` → REF, `ref/call` → CALL (the `xrefs` table)
* `extends` / `extends/{private,protected,public}` / `overrides` /
  `satisfies` → the `inh` table
* `/kythe/edge/named` and C++ `/kythe/code` MarkedSource → name aliases
* `typed` → the `typed` section, `param.N` → the `sig` section
* `childof` → the `childrev` table (`members`)
* `completes` / `completes/uniquely` → the C++ DEFN↔DECL VName bridge

Everything else in the Kythe edge model is **not** ingested — including
`influences`/dataflow edges, `instantiates`/`specializes`, macro
`ref/expands` and macro refs, and `typedef`/alias edges. (Import *sites*
are kept: `ref/imports` folds into the `xrefs` table as a plain REF.) If a
question needs the full edge model (e.g. "what does this template
instantiate", dataflow reachability, macro expansion sites), use Kythe's
own serving tables. See [VS_KYTHE.md](VS_KYTHE.md).

## Symbol identity is a hash; lookups can be ambiguous

A `sym` is `xxHash64` of the symbol's Kythe VName string
(`kythe:<lang>:<corpus>#<root>#<path>#<signature>`). At AOSP scale
(~5M syms over a 2^64 keyspace) the collision probability is ~2.7e-13 —
astronomically rare, but not provably impossible.

Name → sym lookup has two paths with different semantics:

* `sym_for_name` returns **one** sym — the alphabetically-first landing
  of a binary search on the name index. For a name shared by several syms
  (overloads, per-jar copies of a method, language-pair variants) this is
  an arbitrary single landing, which may even be a variant that carries
  no xrefs.
* `syms_for_name` returns **all** syms of the exact name, aggregated over
  the contiguous run of matching name rows (deduped). The `def`, `ref`,
  and `callers` verbs use this so they cover every overload and every
  per-jar copy of a name, not just one.

## Substring search semantics

`--substr` runs a **parallel linear scan** over the names table
(`memchr::memmem`, chunked across cores). It is **case-sensitive by
default**; pass `-i` / `--ignore-case` for case-insensitive matching —
the scan lowercases both the needle and each candidate before the
substring check. Each call is bounded by its per-call cap (`--limit`),
which also caps how many matching symbols flow into the `ref` /
`callers --substr` aggregation, so a broad needle stays bounded.

## `--inject-cu-arg` re-encode detects only argument changes

`from-kzip --inject-cu-arg` mutates only `cu.argument` (it prepends a
javac/clang arg to matching CUs). The kzip re-encode path
(`extract_with` in `crates/scry2/src/kzip.rs`) detects mutation by
snapshotting `cu.argument` before and after the transform: if the
arguments are unchanged it keeps the raw proto bytes verbatim, otherwise
it re-encodes the decoded CU. This is correct **as long as the only field
a transform mutates is `cu.argument`**. A future transform that mutated
other CU fields (`source_file`, `required_input`, `v_name`) would slip
past the snapshot, and its change would be silently dropped because the
raw proto would be kept. If such a transform is added, the detection must
be widened — compare the whole CU, or always re-encode.

## `.s2db` is a trusted build output

`Index::open` validates that every section's `(offset, count, stride)`
fits within the mapped file (with checked arithmetic, so a corrupt header
can't overflow into a spuriously in-range end). It does **not** validate
internal references: blob offsets in sym/name/file/type rows are trusted
to be in-range. `.s2db` is a build output produced by `from-kzip` on the same
host; opening an untrusted or corrupt file can still panic on an
out-of-range slice during a query. scry2 is not hardened against
adversarial index files.
