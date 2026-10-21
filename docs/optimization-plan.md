# Throughput optimization

- [x] Native prepared board execution and opaque results; per-run data remains in DLL, ordered folds and JS numeric checks retained.
- [x] Single-worker optimizer inline path and static panel metadata reuse implemented.
- [x] NUMA/LLC topology with processor groups, physical-first ordering, node selectors, per-process offsets and local-first work dispatch.
- [x] Native tail floor bounded at 20 ms; explicit ABI fallback and startup conformance.
- [x] Initial A/B: Braton +20% at 6 lanes, +22% at 16; cheap Fulmin row +67% at 6 lanes versus ordinary API in same build.
- [x] Optimizer full results match; retained-plan path reduces repeated parsing, bounded to one shape per lane.
- [x] Native cancellation/lifetime and real Windows lane binding verified; 200k short calls and 2000 opaque contexts stay bounded after warmup.
- [x] Full 41-row conformance and optimizer matched; pinned-lane maintenance/save-retry passed 36 rows across 3 replacements.
- [x] Final placement A/B: system 0.6557 s, physical-first cache 0.5216 s at six lanes on this 9700X; use cache on this deployment only.
- [x] Mixed replay completed: all 13 outputs identical; makespan remains bounded by the riven leader and did not improve in this single comparison.
- [x] Standard and AVX2 bundles verified; deployment updated with backup, six-lane budget preserved and measured cache placement selected.
- [x] Deployed smoke: six-lane median 0.511065 s; executable/DLL hashes match, only placement changed in config, journals unchanged; stopped after verification.

## Lead follow-up

- [x] Skip discarded reports on paused lead rounds, including a regression that fails on the original implementation.
- [x] Bound and reset lead key/preference metadata caches; preserve original comparator and all fleet messages.
- [x] Three alternating A/B pairs: 25.684 s to 2.478 s; all 103 steps, score responses, final answers and work match.
- [x] Six request variants and 40 repeated fresh searches match, including changed weapon, changed riven stats, multiple starts and budget cuts.
- [x] Final DLL repeats at 2.472 s; actual-client WASM validation passes. Mixed replay: 34.582 s to 14.661 s, all 13 answers match.
- [x] 0.8.1 deployed with 0.8.0 backup; six-lane smoke 0.521232 s, account/config/kept files unchanged, stopped after validation.
- [x] Standard and native release bundles include validation records; native bundle includes all six patches and their recorded hashes.

Starting point: local 0.7.4 changes, deployment stopped. Preserve all existing
changes and the original `.wfsim-src` checkout. Experimental upstream source
is a separate worktree at 8a00c91ed79f53c2f56dd20d7aca0d6fe1b187c0.

Local source and wrapper changes are authorized; no changes to WFSim's server
or public protocol are needed. Fast paths must keep the reference score,
gas, work, run order, random streams and all result counts. Missing extension
capabilities retain the normal native/WASM path. Xeon/NUMA/X3D performance
cannot be claimed from the available single-node Ryzen 9700X.
