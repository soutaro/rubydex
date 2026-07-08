# Resolution Performance Notes

Working notes from an optimization effort on the resolution phase (branch
`claude/rubydex-resolution-mechanism-tq3wvg`). Written as a handoff document: what was tried, what
worked, what didn't, and where the remaining opportunities are.

All "production codebase" numbers below are from a real monorepo: **154k files, 2.1M definitions,
1.7M declarations, 12.2M constant references** (release build, `RUBYDEX_RESOLUTION_PROFILE=1`).

## Headline result

| | Before | After | |
|---|---|---|---|
| Resolution | 48.2s | 8.2s | 5.9x |
| Whole pipeline | 60.7s | 19.6s | 3.1x |

Roughly **90% of the win came from eliminating redundant serial work**, not from parallelism.
Parallelism proper (verified by A/B on the same machine state) contributes ~1.5–2s. See
"Why parallelism underdelivered" below.

## Measurement infrastructure (use this first)

- `RUBYDEX_RESOLUTION_PROFILE=1` — prints a per-phase breakdown of `resolve()` to stderr: time and
  unit counts per unit kind, `prepare_units` split (depths/classify/sort), reference resolution
  split (parallel compute / serial apply / grouped record insert), per-pass unit counts, and the
  distribution of references by parent scope kind.
- `RUBYDEX_SEQUENTIAL_REFERENCES=1` — forces the serial reference path. **Run-to-run variance on a
  developer machine was ±20%**, repeatedly large enough to invert conclusions. Never compare
  numbers from different runs of different builds; always A/B with this toggle back-to-back.
- Equivalence check: `--stats` output (minus timing lines) was verified **byte-identical** against
  the previous commit after every change, on two corpora (Ruby stdlib `.rb` files, and stdlib
  copied 8x ≈ 5.8k files). This caught real bugs.
- Parallel-path coverage: temporarily set `PARALLEL_THRESHOLD` to 1 and run the full test suite,
  so every batch goes through the parallel code path (small graphs otherwise take the serial path).

## What worked (in order of impact)

### 1. Eliminating redundant ancestor linearization — 48.2s → 14.7s (`ee0da04`)

Profiling showed `ancestors` units taking 29.9s (62%) — almost all redundant:

- `Unit::Ancestors` was enqueued per definition and per blocked reference, so one pass processed
  1.8M linearization units for 409k namespaces. Fix: `enqueue_ancestors` dedups against a
  queued-set.
- Partial chains were rebuilt from scratch on *every touch* (every pass, every search through
  them) even when nothing they depend on had changed: ~290k stuck chains fully rebuilt in each of
  three no-progress passes. Fix: a partial chain records exactly which unresolved names block it
  (its `Ancestor::Partial` entries — parents' blockers are inlined into child chains), so it is
  only rebuilt when one of those names has since been resolved, or when the declaration gained a
  definition (`dirty_chains`; new definitions can add mixins/superclasses).

Related enablers, done earlier: descendants are no longer propagated during linearization but
computed once at the end of `resolve()` by inverting the (transitively flattened) ancestor chains
(`a31a47c`); complete chains are searched by reference instead of cloned per lookup (`d3c7bd9`).

### 2. Integer sort keys in prepare_units — sort 5.3s → 1.0s (`7282c33`)

The unit sort compared `(depth-hash-lookup, URI-string, offset)` per comparison. Precomputing
`(depth, lexicographic URI rank, offset)` integer keys at classification time made the sort
~5x faster with unchanged order.

### 3. Parallel + per-name reference resolution — refs 3.5s → 2.8s net (`39cfb18`…`08fc65d`)

Final shape (per convergence pass, per depth wave):

1. *(parallel)* resolve the wave's **unique names** with a read-only kernel
   (`try_resolve_name_readonly`, a mirror of `resolve_constant_internal` that returns
   `NeedsSerial` wherever the serial version would mutate).
2. *(serial, once per unique name)* record resolved names; run the mutating serial resolution once
   per `NeedsSerial` name (singleton creation for `Attached` scopes, linearization, promotion) —
   190k serial resolutions instead of 5.6M per-reference fallback calls.
3. *(parallel)* sweep the wave's references against the updated name table, emitting
   `(declaration, reference)` pairs or requeues into per-chunk vectors, merged in chunk order
   (deterministic regardless of worker count).
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
`names` 1.7M, `constant_references` 12.2M, RSS ~6.6GB). IDs are already hashes, so each lookup is
a random DRAM access (~100ns stall, 2–3 misses per logical lookup counting the boxed payload),
and graph traversal is dependent pointer-chasing that ILP can't hide. Cores share the memory
system, so parallel speedup saturates around 2–4x regardless of core count. Additionally, all
writes go through the single `&mut Graph` and are inherently serial (Amdahl).

## Remaining opportunities (not attempted, in rough order of expected value)

1. **Memory layout** (likely the biggest lever now): arena storage + dense u32 indices instead of
   64-bit hash-keyed maps (keep hashed IDs only at the external/FFI boundary), unboxing
   declaration payloads, sorting batched lookups by index for prefetcher-friendly access. Targets
   both speed (2–3 misses/lookup → ~1, better cache density) and the 6.6GB RSS.
2. **Pipeline overlap of indexing and resolution** (~19.6s → ~11s wall): a single mutator thread
   alternates "merge arrived LocalGraphs / resolve pending work" while parser workers run; final
   settling resolve + end-of-resolve passes (descendants, reference records) after indexing
   completes. Prerequisites: (a) verify that adding a definition invalidates now-shadowed resolved
   names (same requirement as LSP incremental updates — this becomes a huge stress test of that
   machinery), (b) a waiter index (below) to avoid re-running blocked units every round,
   (c) determinism requires the fixed point to be arrival-order independent.
3. **Waiter index**: blocked units still retry every pass (~54k refs + ~50k ancestors x 3 tail
   passes). A failing unit knows exactly what blocked it (the unresolved `NameId`, or the chain's
   `DeclarationId`); registering waiters and waking them on `record_resolved_name` /
   chain-completion would remove all blind retries and give the scheduler explicit dependency
   edges.
4. **`handle_remaining_definitions` (1.2s)**: same compute/apply split as references — owner
   resolution (lexical walk, receiver resolution, alias chasing) is read-only; `create_declaration`
   applies serially.
5. **`compute_name_depths` (0.4s)**: pure function; parallelizable with per-chunk local memo
   caches (some duplicated work, but parallel).
6. **Sharded graph** (declarations/names split by ID hash, writes parallel per shard): the only
   way past the serial-write Amdahl wall, but invasive (FFI, invalidation, determinism) for ~2–3s
   at current sizes. Revisit after (1).

## Invariants to preserve when continuing

- **Determinism**: parallel compute must be pure (snapshot reads); all writes applied in an order
  independent of worker count (fixed unit order, or chunk-order merges). This preserves the
  documented iteration-order guarantees and keeps the byte-diff validation methodology usable.
- **Resolution is monotonic within one `resolve()`**: complete/cyclic chains never demote,
  resolved names never re-resolve (`record_resolved_name` never overwrites — which is also why
  premature/tentative resolutions are dangerous). Parallel designs must fail toward
  Requeue/NeedsSerial, never toward guessing.
- Validation recipe per change: full Rust suite (1,100 tests) + suite with `PARALLEL_THRESHOLD=1`
  + Ruby suite (208 tests) + `--stats` byte-diff on the stdlib and stdlib-x8 corpora + A/B with
  `RUBYDEX_SEQUENTIAL_REFERENCES` on the target codebase.

## Upstream sync note (July 2026)

Main was synced with upstream while this branch was in flight. Upstream landed overlapping work:
`Outcome` was refactored (`Resolved(id)` / `Retry { partial_ancestors }` / unit `Unresolved`, no
more linearization-id payload), duplicate ancestor enqueueing was reduced (#906),
`get_or_create_singleton_class` gained `SingletonAncestors` scheduling modes (#907), and
**`Unresolved` references are now pushed to `pending_work` for the next incremental resolution
instead of being retried within the current cycle** — the read-only kernel mirrors this with
`ReadOutcome::Defer` and the sweep routes those references to `extend_work`. The blocker-based
partial-chain reuse, `enqueue_ancestors` dedup (`Enqueue` mode routes through it), and all parallel
machinery were re-reconciled on top; `--stats` is byte-identical to origin/main on both corpora and
the merged branch resolves the 8x stdlib corpus 1.9x faster than origin/main on 4 cores.

## Unrelated finding (filed for later)

Mixins written in an alias-reopened class body are not merged into the target's ancestors:
`B = A; class B; include N; end` puts the `class B` definition on the ConstantAlias declaration,
so `A.ancestors` misses `N` (real Ruby includes it). Members and nested constants *do* merge
(owner resolution follows alias chains); only mixins/superclass are lost. A fix would attach the
reopening definition to the alias target's declaration, but needs care around
`record_resolved_name` and reference bookkeeping.
