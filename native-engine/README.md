# Native host extensions

`tools/build-native-engine.ps1` builds an isolated worktree of the source that
hashes to the requested reference engine id. It never resets `.wfsim-src` or
uses its dirty worktree files. Optional local patches are in `patches/`; a patch
that no longer applies is skipped, and its associated feature is not enabled.
Each library records the upstream commit, local patches and SHA-256 beside it.

`revision` versions the local implementation separately from WFSim's engine
identity. Revision 1 is published as `<engine>-sfe1[-v3].dll` (or the platform's
library spelling). simfleet 0.8.1 tries this revision before legacy libraries,
so an already cached older DLL cannot mask an optimized release. Existing
assets stay immutable; bump the revision when replacing native behavior for
an existing engine ID. Every published library includes its `.sha256` and
`.build.json` provenance. `-NoPatch` keeps the unrevisioned comparison filename.

- `001-prepared-board.patch` exposes an immutable prepared fight to the local
  wrapper. It changes neither the battle loop nor the metered sources.
- `002-single-worker.patch` runs one optimizer worker inline, preserving order,
  seeds and search width. The wrapper still uses `threads: 1`.
- `003-static-panel-metadata.patch` reuses process-lifetime weapon metadata
  instead of leaking a new copy per call. Kitgun assembly does not change these
  metadata fields. A pool slice is allocated once per weapon.
- `004-frozen-optimize-plan.patch` permits a frozen parsed plan to serve another
  fleet message; the wrapper retains at most one shape per lane and starts a
  new one on `fresh` or a changed request. Search state remains thread-local.
- `005-deferred-lead-reports.patch` skips final answer materialization and
  contender reports while any start is waiting for scores. Every start still
  runs and contributes its pending batch and progress in the original order.
- `006-bounded-lead-metadata.patch` memoizes build keys and preference metadata
  during lead replay. Each map has at most 4,096 entries, is reset on `fresh`,
  and is destroyed when the leader thread retires. The original comparator,
  simulator, candidate order and scoring remain authoritative.

These are local native extensions. The public server endpoints, task inputs,
random streams, run order and gas/work calculations are unchanged. The library
id names the upstream reference whose outputs it must reproduce; the build
manifest separately identifies the applied local modifications. A normal
native API remains available, and `-NoPatch` builds a reference wrapper.

## Opaque board ABI 1

`sfe_board_new({request,ruler})` returns an owned opaque row. Its immutable
preparation is initialized on the first compute lane. `sfe_board_runs(row,
from,count)` claims nonoverlapping ranges, computes one original-engine shard
per run and folds completed contiguous runs in ascending order under a short
mutex. It returns the folded count or a negative failure. Duplicate work
invalidates the row. `sfe_board_score` refuses incomplete or failed rows and
calls the original scoring function. Output buffers go to `sfe_free`.

`sfe_board_drop` is called once after every submitted job ends. The client
holds an `Arc` containing both row and originating library throughout each
job, including after task cancellation or engine replacement. Retired workers
are joined by a background reaper before their libraries are released, so
native thread-local destructors still execute from mapped code. No Rust type
layout, string ownership or allocator crosses this ABI.

`exact.rs` uses a zero-allocation serde traversal to check whether a shard's
numbers survive the browser JSON conversion unchanged. Large integers,
negative zero and nonfinite values take the same number-normalization path as
the ordinary client. Each run remains a separate shard; the order of floating
point additions is never changed by a parallel reduction.

The client validates the extension against the site's WASM on ascending,
reverse and seven-run groups before enabling it. `check-native` compares the
whole `counts` object, score bits, metric and work across real rows and gas
vectors. Optimizer validation compares the full results too. The comparison
path can be forced with `SIMFLEET_NATIVE_FAST=0`.

The state is bounded by a task's requested runs and outstanding ranges, not by
process uptime. Prepared plans and the shard accumulator are dropped with the
row; there is no global result cache. Process recycling remains available for
global allocations owned by upstream DLLs that cannot be reclaimed by thread
retirement alone.
