# Prepare a board batch once

Offline experiment against WFSim commit `8a00c91ed79f53c2f56dd20d7aca0d6fe1b187c0`,
the source of served engine `872e90ffaf0d` on 2026-10-21. This patch is an
upstream optimization proposal, not a production engine release.

`board_runs_json` currently calls `simulate_shard_json` separately for each
run. That rebuilds `Entrant`, resolves its mods and arcanes, constructs the
fight parameters and computes report-only Forma cost on every iteration.
The patch prepares the immutable fight parameters once per batch with
`ready(request, false)`, then invokes the original `fight::shard` once per
run with the same seed and run index. Every run is still emitted separately
and folded in the original order. The battle loop and its gas meter are
unchanged. No approximate damage formula or smaller sample is substituted.

The isolated DLL is stamped `experimental-prepared`. It was not copied into
the real deployment and was never used to claim or submit server work.
WFSim's current client contract requires the native engine source to be
unmodified. Adoption needs an upstream engine release or an agreed extension.

## Measurements

Ryzen 7 9700X, Windows x64, Rust 1.99.0, x86-64-v3, mimalloc v2, original
release profile. Both DLLs stay loaded in one Python process. Each row warms
up both paths once, then alternates A/B and B/A for three measured repetitions.
No other benchmark runs concurrently. All values below are medians.

`batch-results.json` covers 15 cases, each with 100 runs starting at index 11.
Every response is **byte-identical**, covering per-run RNG state, accumulators,
gas and report counters, not just the score. Observed time reductions range
from 2.7% on expensive crowds to 37.5% on a cheap single-target case. Results
under about 10% remain sensitive to machine noise.

`full-row-results.json` includes all 1000 runs of each row, 100-run batches,
ordered folds and final scoring through the same JSON paths in both engines.
The complete final JSON responses are byte-identical, including score,
metric, work and every count:

| Case | Original | Prepared batch | Time reduction | Throughput ratio |
|---|---:|---:|---:|---:|
| Braton Prime / demolisher / cycle | 3.5428 s | 3.0723 s | 13.3% | 1.153× |
| Torid / demolisher / cycle | 2.6356 s | 2.1470 s | 18.5% | 1.228× |
| Fulmin Prime / heavy gunner / semi-auto | 0.9454 s | 0.6661 s | 29.5% | 1.419× |
| Kuva Hind / multi-target / semi-auto | 1.5142 s | 1.4176 s | 6.4% | 1.068× |

These are native API timings on one executing thread, including Python's
JSON bridge for the full-row comparison. They are not measurements of the
live six-lane client, average site throughput or a 13400. The heavier crowd
cases spend most of their time inside combat simulation and gain less.

Preparing once also reduces the number of request-time permanent allocations
in `base_panel_assembled` within these batches. It does not repair unrelated
engine allocations or guarantee a flat memory curve.

## Reproduction

Use a separate WFSim checkout at the pinned commit. Build a baseline DLL
through simfleet's `native-engine` wrapper, then apply `prepare-once.patch`:

```powershell
git -C <wfsim-checkout> apply <absolute-path-to-prepare-once.patch>
$env:SFE_ENGINE_ID = 'experimental-prepared'
$env:RUSTFLAGS = '-C target-cpu=x86-64-v3 -C target-feature=+crt-static'
cargo build --release --manifest-path <wrapper-Cargo.toml>
python tools/bench-native-api.py --baseline <baseline.dll> --candidate <experimental.dll> --rows experiments/prepared-board/orders.json --out batch-results.json --count 100 --repeats 3
python tools/bench-native-api.py --baseline <baseline.dll> --candidate <experimental.dll> --rows experiments/prepared-board/full-row-orders.json --out full-row-results.json --count 100 --repeats 3 --full-row
```

The wrapper's `wfsim-webapi` and `wfsim-optimizer` path dependencies must point
to that checkout. Both binaries and their hashes are recorded in the result
JSON. This session's DLLs are under `.bench-20261021/algorithm/`; the source
reference was restored after exporting and round-trip checking the patch.

## Other concrete opportunities

- A typed native boundary could avoid turning every shard into a JSON object,
  converting it back into `Shard` for validation, serializing it to text,
  parsing it again in the client and again in the fold. Preserve JS number
  normalization and one-run fold ordering where the current wire depends on
  them. This needs its own conformance and performance proof.
- `optimizer/src/descent.rs::par_map` creates and joins a new OS thread even
  when the wrapper sets `threads: 1`. A single-worker inline path can remove
  thread creation, scheduler waits and TLS allocator churn. Confirm priority,
  callbacks and thread-local state before adoption; no gain is claimed yet.
- `parse_optimize` creates its prepared-fight map on each call. A quick-fleet
  search makes many calls of the same request shape. Keeping this preparation
  in a search-scoped object can remove repeated parsing, but must be released
  on fresh search, completion and cancellation, and separated by engine/data.
- Finalist validation can run independent complete replays on spare budgeted
  lanes. It reduces the serial tail of one search. Under already-saturated
  multi-task load it redistributes work and is not an automatic throughput gain.
- Changing screening, sample counts, pruning or analytic approximation can
  reduce simulation work more radically, but changes the requested answer and
  gas/work. It requires a coordinated upstream algorithm change and accuracy
  evaluation. Existing quick search already screens at 20%, keeps 20%, and
  continues its earlier runs; these techniques are not missing wholesale.

The engine already advances by events and uses a heap for sufficiently large
DoT queues. A generic recommendation to add an event queue or more threads
would repeat work the engine already does.
