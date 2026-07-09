# Resolution Performance Notes

Working notes from an optimization effort on the resolution phase, on branch
`claude/rubydex-resolution-mechanism-tq3wvg`. Written as a handoff document: current state of the
branch, what was tried, what worked, what didn't, and where the remaining opportunities are.
**The branch is merged with upstream main (as of #907) and is ready to be turned into a PR.**

## Headline results (production codebases, release builds)

Codebase A — 154k files, 2.1M definitions, 12.2M references, measured against pre-sync main:

| | main (pre-sync) | branch | |
|---|---|---|---|
| Resolution | 48.2s | 8.2s | 5.9x |
| Whole pipeline | 60.7s | 19.6s | 3.1x |

Codebase B — 109k files, 1.23M definitions, 13.2M references, measured against **post-sync main**
(which already includes upstream's own linearization improvements, #906/#907):

| | main (post-sync) | branch | |
|---|---|---|---|
| Resolution | 16.75s | 6.63s | 2.5x |
| Whole pipeline | 23.2s | 13.7s | 1.7x |

In both cases the `--stats` query statistics (declaration/definition/orphan counts) are **identical
to main**, i.e. the speedup does not change resolution results.

Caveats: roughly 90% of the win on codebase A came from **eliminating redundant serial work**, not
from parallelism — parallelism proper (verified by A/B on the same machine state) contributes
~1.5–2s there. Peak RSS grows ~10% (5.03GB → 5.67GB on codebase B) from the parallel machinery's
transient buffers (deferred reference records, retained name depths, classification buffers).

## Branch state / PR notes

```
b0c5880 Merge origin/main into the resolution performance branch
750df6d Document the resolution performance work
c24161b Parallelize classification and sorting in prepare_units
08fc65d Parallelize the per-reference sweep in reference waves
d914829 Defer reference recording from parallel waves to a grouped batch insert
3f885a5 Record resolved names per unique name and profile the apply step
12eb885 Resolve reference batches in waves of increasing name depth
e09e70c Deduplicate reference resolution by name in parallel batches
39cfb18 Resolve constant reference batches in parallel
7282c33 Precompute integer sort keys in prepare_units
ee0da04 Avoid redundant ancestor linearization work in the convergence loop
51b308b Add opt-in resolution profiling via RUBYDEX_RESOLUTION_PROFILE
d3c7bd9 Skip cloning complete ancestor chains during resolution
a31a47c Compute descendants by inverting ancestors at the end of resolution
```

Diff vs main is almost entirely `rust/rubydex/src/resolution.rs` (+1 line AGENTS.md documenting
the profiling env var, a `Copy` derive on `Unit` in graph.rs, and a sort-insensitive descendants
assertion in test/declaration_test.rb). If a single PR is too large to review, a natural split is:

1. Serial fixes: `a31a47c` + `d3c7bd9` + `ee0da04` + `7282c33` (the bulk of the speedup, no threads)
2. Profiling: `51b308b` + the later profile refinements
3. Parallel reference waves + parallel prepare_units: the rest

Validation recipe used for every change (worth repeating in the PR):
`cargo test` (1,066 tests) · the same suite with `PARALLEL_THRESHOLD` temporarily set to 1 so every
batch takes the parallel path · `bundle exec rake ruby_test` (212 tests) · `--stats` output
byte-diffed against main on two corpora (Ruby stdlib and stdlib copied 8x) · A/B on a production
codebase with `RUBYDEX_SEQUENTIAL_REFERENCES=1`.

## Measurement infrastructure (use this first)

- `RUBYDEX_RESOLUTION_PROFILE=1` — prints a per-phase breakdown of `resolve()` to stderr: time and
  unit counts per unit kind, `prepare_units` split (depths/classify/sort), reference resolution
  split (parallel compute / serial apply / grouped record insert), per-pass unit counts, and the
  distribution of references by parent scope kind.
- `RUBYDEX_SEQUENTIAL_REFERENCES=1` — forces the serial reference path. **Run-to-run variance on a
  developer machine was ±20%**, repeatedly large enough to invert conclusions. Never compare
  numbers from different runs of different builds; always A/B with this toggle back-to-back.
- Equivalence check: `--stats` output (minus timing lines) byte-diffed against main.
- Parallel-path coverage: temporarily set `PARALLEL_THRESHOLD` to 1 and run the full test suite,
  so every batch goes through the parallel code path (small graphs otherwise take the serial path).

## What worked (in order of impact)

### 1. Eliminating redundant ancestor linearization — codebase A: 48.2s → 14.7s (`ee0da04`)

Profiling showed `ancestors` units taking 29.9s (62%) — almost all redundant:

- `Unit::Ancestors` was enqueued per definition and per blocked reference, so one pass processed
  1.8M linearization units for 409k namespaces. Fix: `enqueue_ancestors` dedups against a
  queued-set. (Upstream's #906 later removed some of the same churn; after the merge, upstream's
  `SingletonAncestors::Enqueue` mode routes through `enqueue_ancestors`.)
- Partial chains were rebuilt from scratch on *every touch* (every pass, every search through
  them) even when nothing they depend on had changed: ~290k stuck chains fully rebuilt in each of
  three no-progress passes. Fix: a partial chain records exactly which unresolved names block it
  (its `Ancestor::Partial` entries — parents' blockers are inlined into child chains), so it is
  only rebuilt when one of those names has since been resolved, or when the declaration gained a
  definition (`dirty_chains`; new definitions can add mixins/superclasses). **This part is not in
  upstream** and is the single largest win on the branch.

Related enablers, done earlier: descendants are no longer propagated during linearization but
computed once at the end of `resolve()` by inverting the (transitively flattened) ancestor chains
(`a31a47c`); complete chains are searched by reference instead of cloned per lookup (`d3c7bd9`).

### 2. Integer sort keys in prepare_units — sort 5.3s → 1.0s (`7282c33`)

The unit sort compared `(depth-hash-lookup, URI-string, offset)` per comparison. Precomputing
`(depth, lexicographic URI rank, offset)` integer keys at classification time made the sort
~5x faster with unchanged order.

### 3. Parallel + per-name reference resolution (`39cfb18`…`08fc65d`)

Final shape (per convergence pass, per depth wave):

1. *(parallel)* resolve the wave's **unique names** with a read-only kernel
   (`try_resolve_name_readonly`, a mirror of `resolve_constant_internal` that returns
   `NeedsSerial` wherever the serial version would mutate, and `Defer` where it would return
   `Unresolved`).
2. *(serial, once per unique name)* record resolved names; run the mutating serial resolution once
   per `NeedsSerial` name (singleton creation for `Attached` scopes, linearization, promotion) —
   ~190k serial resolutions instead of 5.6M per-reference fallback calls.
3. *(parallel)* sweep the wave's references against the updated name table, emitting
   `(declaration, reference)` pairs, requeues (retryable this cycle), or deferrals (routed to
   `pending_work`, mirroring the serial `Unresolved` handling) into per-chunk vectors, merged in
   chunk order — deterministic regardless of worker count.
4. Reference records are applied at the end of `resolve()` as one grouped batch insert
   (`d914829`): grouping by declaration on worker threads first turns 12M declarations-map
   lookups into one lookup per declaration per chunk. Reference sets are `IdentityHashSet`s, so
   insertion order doesn't affect contents.

### 4. Parallel prepare_units — 2.6s → 1.6s (`c24161b`)

Classification splits into a parallel read-only pass (kind + sort key per unit) and a serial
fan-out pass (dedup needs global state). References are distributed into per-depth buckets, which
sort on worker threads independently — equivalent to the single `(depth, uri_rank, offset)` sort.

## What did NOT work (and why — don't repeat these)

### Single-snapshot-per-pass parallel batches (regression: 9.4s → 12.4s)

First parallel version resolved each pass' whole reference batch against one immutable snapshot.
Dependencies then only propagate at pass boundaries: `A::B` can't resolve until the pass after
`A`. Convergence passes went 5 → 11 and reference executions 12.2M → 18.4M, and each extra pass
re-runs all pending bookkeeping. **Fix:** split batches into waves of increasing name depth
(`12eb885`). A name's parent scope and nesting are strictly shallower than the name, so running
waves in depth order with outcomes applied in between exactly reproduces the serial intra-pass
cascade. Unit counts returned to the serial baseline.

### Pass-scoped caching of partial ancestor chains (unsound)

Caching "partial chain built during this pass" broke
`module_own_ancestors_take_priority_over_object_fallback`: a blocker resolved mid-pass must
invalidate immediately, otherwise a reference searching the stale chain falls back to `Object`'s
ancestors and records a **wrong tentative resolution — `record_resolved_name` never overwrites**,
so it sticks. The correct cache validity condition is blocker-based ("none of the chain's
`Ancestor::Partial` names resolved yet"), which also works across passes.

### Expecting large wins from parallelizing reference resolution

The serial resolver already memoizes per name: the first reference with a given name does the
expensive search, `record_resolved_name` stores it, and every later reference is a cheap hash-hit.
So the parallelizable "search" was only ~1.7M unique names (~0.3s of 3.8s); the rest is per-
reference bookkeeping. The first parallel attempts *lost* to serial until the bookkeeping itself
(step 3/4 above) was parallelized.

## Why parallelism underdelivered (analysis)

Resolution is a chain of lookups into GB-scale `IdentityHashMap`s (`declarations` 1.7M entries,
`names` 1.7M, `constant_references` 12–13M, RSS 5–7GB). IDs are already hashes, so each lookup is
a random DRAM access (~100ns stall, 2–3 misses per logical lookup counting the boxed payload),
and graph traversal is dependent pointer-chasing that ILP can't hide. Cores share the memory
system, so parallel speedup saturates around 2–4x regardless of core count. Additionally, all
writes go through the single `&mut Graph` and are inherently serial (Amdahl).

## Upstream sync note (July 2026, merge `b0c5880`)

Upstream landed overlapping work while the branch was in flight: `Outcome` was refactored
(`Resolved(id)` / `Retry { partial_ancestors }` / unit `Unresolved`, no more linearization-id
payload), duplicate ancestor enqueueing was reduced (#906), `get_or_create_singleton_class` gained
`SingletonAncestors` scheduling modes (#907), and **`Unresolved` references are now pushed to
`pending_work` for the next incremental resolution instead of being retried within the current
cycle**. The read-only kernel mirrors this with `ReadOutcome::Defer` and the sweep routes those
references to `extend_work`. All branch machinery was re-reconciled on top; `--stats` stays
byte-identical to post-sync main, which is still 2.5x slower on codebase B.

Whenever the serial resolution logic changes (`resolve_constant_internal`, `run_resolution`,
`search_ancestors`), the read-only mirrors (`try_resolve_name_readonly`, `read_run_resolution`,
`read_search_ancestors`) **must be updated in lockstep** — the byte-diff check catches drift.

## Remaining opportunities (in rough order of expected value)

1. **Waiter index** (raised in priority by the codebase B profile): ~744k references and ~169k
   ancestors units are retried in each of three no-progress tail passes — `A::B` lookups where `A`
   resolved but `B` doesn't exist stay `Retry` forever within the cycle. A failing unit knows
   exactly what blocked it (the unresolved `NameId`, or the chain's `DeclarationId`); registering
   waiters and waking them on `record_resolved_name` / chain completion would remove all blind
   retries (~1s on codebase B) and give the scheduler explicit dependency edges.
2. **Memory layout** (the biggest lever for absolute speed): arena storage + dense u32 indices
   instead of 64-bit hash-keyed maps (keep hashed IDs only at the external/FFI boundary), unboxing
   declaration payloads, sorting batched lookups by index for prefetcher-friendly access. Targets
   both speed (2–3 misses/lookup → ~1, better cache density) and RSS.
3. **Pipeline overlap of indexing and resolution** (wall-clock: Listing + max(Indexing, Resolution)
   instead of the sum): a single mutator thread alternates "merge arrived LocalGraphs / resolve
   pending work" while parser workers run; final settling resolve + end-of-resolve passes
   (descendants, reference records) after indexing completes. Prerequisites: (a) verify that adding
   a definition invalidates now-shadowed resolved names (same requirement as LSP incremental
   updates), (b) the waiter index, (c) determinism requires the fixed point to be arrival-order
   independent.
4. **`handle_remaining_definitions`** (~0.6–1.2s): same compute/apply split as references — owner
   resolution (lexical walk, receiver resolution, alias chasing) is read-only;
   `create_declaration` applies serially.
5. **`compute_name_depths`** (~0.4s): pure function; parallelizable with per-chunk local memo
   caches.
6. **RSS mitigation** (~600MB regression): apply deferred reference records per wave instead of
   accumulating all of them; drop `name_depths` after the convergence loop.
7. **Sharded graph** (declarations/names split by ID hash, writes parallel per shard): the only
   way past the serial-write Amdahl wall, but invasive (FFI, invalidation, determinism) for ~2–3s
   at current sizes. Revisit after (2).

## Incremental resolution (in progress)

`examples/incremental.rs` benchmarks the LSP-style flow: index + resolve, re-index one edited
file + resolve, then a no-op resolve. On the 8x stdlib corpus the no-op resolve — which should be
~free — cost 30ms; on production codebases it extrapolates to ~1s per edit. Three causes:

1. **Fixed O(graph) costs per resolve** — fixed in `711770e`: `compute_name_depths` now computes
   depths lazily for the pending units' names only, and descendant sets are maintained
   incrementally after the first full build (linearization records rebuilt chains in
   `Graph::pending_descendant_chains`; invalidation already removes stale edges). This removed
   ~0.8s/resolve on codebase B-scale graphs.
2. **Permanently unresolvable units re-attempted every resolve** (~53k units on the 8x corpus,
   ~744k on codebase B): `pending_work` is fully drained and re-attempted by every `resolve()`,
   paying classification, sorting, and resolution for units that cannot make progress. This is now
   the dominant no-op cost. **Design for the fix (not yet implemented) — event-keyed parking:**
   - Park units that end a resolve without progress, keyed by what would unblock them:
     `Unresolved`-class lookups by the missing member's `StringId` (woken when `add_member`
     touches that string), `Retry`-class units by the blocking `NameId` (unresolved parent scope /
     partial-chain blocker, woken by `record_resolved_name` / chain completion).
   - The graph accumulates wake events (append-only lists filled by `add_member` /
     `record_resolved_name`); the convergence loop drains events between passes and moves woken
     units back into the queue, and events persist across resolves so parked units wake in the
     first pass of the next resolve. This replaces both the in-loop blind retries (the tail
     passes) and the cross-resolve re-attempts, and doubles as the dependency edges for any future
     scheduling work.
   - Watch out for: staleness (parked units whose reference/definition was deleted — validate on
     wake, as prepare_units already does), the tentative Object-fallback resolutions (must not
     park a unit that the serial path would still retry), and determinism of wake order (drain
     events in insertion order).
3. **Invalidation fan-out** (not yet investigated): editing one small file re-queued ~15k
   references on the 8x corpus. Partly a corpus artifact (8 copies share FQNs, so one file's
   definitions belong to declarations with definitions in all copies), but worth profiling on a
   real codebase — `name_dependents` and descendant-based re-queueing may over-approximate.

### Invalidation scope (next focus — the typical-case incremental cost)

The huge unresolved backlog above is an artifact of missing gem sources; with a fully-indexed
codebase the incremental cost is dominated by **invalidation fan-out**. Measured on a synthetic,
fully-resolvable corpus (2k files, every reference resolves): re-indexing **one unchanged file**
re-queues ~605 references and ~42 chains — ~40x the file's own content, where the correct answer
is zero. Causes, from reading `remove_document_data` / `invalidate_declaration`:

1. **No update diffing**: a document update is remove-everything + re-add, and the removal cascade
   completes before the identical definitions are re-added. Even a byte-identical save pays the
   full storm.
2. **Single-file declarations take the Remove path**: saving the file removes the declaration's
   only definitions, so the declaration is deleted outright — unresolving *every reference to it
   from every file* — and then recreated identically by the re-add.
3. **Update path resets chains unconditionally** (upstream's own TODO in
   `invalidate_declaration`): any definition churn on a declaration clears its ancestors and
   descendants and re-queues the descendants' chains, even when the mixins/superclass are
   unchanged.

Proposed direction, in increasing order of effort:

- **Document-content short-circuit**: skip the update entirely when the new content hash equals
  the old (editor auto-saves become free).
- **Definition-set reconciliation**: index the new content first, diff the definition sets
  (DefinitionIds are content-addressed by uri/offset/name, so unchanged prefixes of the file keep
  their IDs), and run invalidation only on the symmetric difference. Follow-up: match old/new
  definitions by (name, kind, ordinal) and patch offsets in place, so an edit near the top of a
  file doesn't invalidate every definition below it.
- **Ancestor-relevant change detection**: on the Update path, compare the old and new definitions'
  mixins/superclass and skip the chain + descendants reset when they are equal (this is what
  upstream's TODO asks for; it makes method-body edits not touch the hierarchy at all).
- The operation builder/applier backend (`operation/`) may already be intended as the diffing
  pipeline — check whether update reconciliation belongs there before building it elsewhere.

## Invariants to preserve when continuing

- **Determinism**: parallel compute must be pure (snapshot reads); all writes applied in an order
  independent of worker count (fixed unit order, or chunk-order merges). This preserves the
  documented iteration-order guarantees and keeps the byte-diff validation methodology usable.
- **Resolution is monotonic within one `resolve()`**: complete/cyclic chains never demote,
  resolved names never re-resolve (`record_resolved_name` never overwrites — which is also why
  premature/tentative resolutions are dangerous). Parallel designs must fail toward
  Requeue/Defer/NeedsSerial, never toward guessing.
- **Mirror discipline**: the read-only kernel must map serial outcomes exactly —
  `Retry` → `Requeue`, `Unresolved` → `Defer`, any mutation → `NeedsSerial`.

## Unrelated finding (filed for later)

Mixins written in an alias-reopened class body are not merged into the target's ancestors:
`B = A; class B; include N; end` puts the `class B` definition on the ConstantAlias declaration,
so `A.ancestors` misses `N` (real Ruby includes it). Members and nested constants *do* merge
(owner resolution follows alias chains); only mixins/superclass are lost. A fix would attach the
reopening definition to the alias target's declaration, but needs care around
`record_resolved_name` and reference bookkeeping.
