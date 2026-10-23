#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Measure real WFSim primitives on CPU and NVIDIA CUDA, including transfers.

This is an offline feasibility probe, never a replacement board engine. CPU
results come from the pinned upstream Rust implementation, not a Python clone.
"""
import argparse
import concurrent.futures as futures
import ctypes as c
import hashlib
import json
import math
import pathlib
import random
import statistics
import time

from cuda_driver import Cuda


def pointer(array, element, offset=0):
    return c.cast(c.byref(array, offset * c.sizeof(element)), c.POINTER(element))


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--reference", type=pathlib.Path, required=True)
    p.add_argument("--nvrtc-dir", type=pathlib.Path, required=True)
    p.add_argument("--out", type=pathlib.Path, required=True)
    p.add_argument("--sizes", default="32,256,4096,65536")
    p.add_argument("--lanes", type=int, default=16)
    p.add_argument("--repeats", type=int, default=7)
    args = p.parse_args()
    sizes = [int(n) for n in args.sizes.split(",")]
    if min(sizes + [args.lanes, args.repeats]) < 1:
        p.error("sizes, lanes and repeats must be positive")
    lib = c.CDLL(str(args.reference.resolve()))
    lib.hw_first_hit.argtypes = [c.POINTER(c.c_double), c.c_size_t, c.POINTER(c.c_double), c.c_size_t, c.POINTER(c.c_uint64)]
    lib.hw_first_hit.restype = c.c_int
    lib.hw_rng.argtypes = [c.c_uint64, c.c_uint64, c.c_size_t, c.POINTER(c.c_double)]
    lib.hw_rng.restype = None
    report = dict(scope="primitives only; synthetic inputs; no board acceleration claim",
                  reference_sha256=hashlib.sha256(args.reference.read_bytes()).hexdigest(),
                  seed=46174, lanes=args.lanes, samples=[])
    maximum = max(sizes)
    r = random.Random(report["seed"])
    # Includes misses, degenerate directions, tangencies, ties and exponent
    # scaling. One batch shape is a 19x19 formation from the board rulers.
    bodies = (c.c_double * (361 * 2))(*[v for y in range(19) for x in range(19) for v in ((x - 9) * 3.0, y * 3.0 + 5.0)])
    fixed = [(0., 0., 0., 0.), (0., 0., 0., 1.), (.25, 0., 0., 1.),
             (.25000000000000006, 0., 0., 1.), (0., 0., 0., -1.),
             (0., 0., 1e-200, 1e-200), (0., 0., 1e200, 1e200),
             (0., 0., 5e-324, 5e-324)]
    rays = (c.c_double * (maximum * 4))(*[v for i in range(maximum) for v in (fixed[i] if i < len(fixed) else (r.uniform(-30, 30), r.uniform(-5, 5), r.uniform(-1, 1), r.uniform(-.1, 1)))])
    with Cuda(args.nvrtc_dir) as gpu, futures.ThreadPoolExecutor(max_workers=args.lanes) as pool:
        ptx = gpu.compile(pathlib.Path(__file__).with_name("kernels.cu"))
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.with_suffix(".ptx").write_bytes(ptx)
        report.update(device=gpu.name, architecture=gpu.arch, compiler_options=gpu.options)
        db = gpu.allocate(c.sizeof(bodies))
        dr = gpu.allocate(c.sizeof(rays))
        do = gpu.allocate(maximum * 3 * 8)
        gpu.upload(db, bodies)
        # The pinned metering weights are measured from upstream: base includes
        # the helper call; each actual body iteration charges the same weight.
        degenerate = (c.c_double * 4)(0, 0, 0, 0)
        regular = (c.c_double * 4)(0, 0, 0, 1)
        probe = (c.c_uint64 * 3)()
        def cpu_geom(ray_array, count, out, n_bodies=361):
            if lib.hw_first_hit(bodies, n_bodies, ray_array, count, out):
                raise RuntimeError("reference geometry panicked")
        cpu_geom(degenerate, 1, probe)
        base = probe[2]
        cpu_geom(regular, 1, probe, 0)
        if probe[2] != base:
            raise AssertionError("gas depends on normalization path; update the kernel")
        cpu_geom(regular, 1, probe, 1)
        loop = probe[2] - base
        aligned = (c.c_double * 4)(0, 5, 0, 8)
        if lib.hw_first_hit(aligned, 2, regular, 1, probe):
            raise RuntimeError("gas calibration failed")
        compare = probe[2] - base - 2 * loop
        report["gas"] = dict(base=base, per_body=loop, compare=compare)
        for count in sizes:
            ray_batch = (c.c_double * (count * 4)).from_buffer(rays)
            expected = (c.c_uint64 * (count * 3))()
            got = (c.c_uint64 * (count * 3))()
            cpu_geom(ray_batch, count, expected)
            values = [db, c.c_uint(361), dr, c.c_uint(count), c.c_uint64(base), c.c_uint64(loop), c.c_uint64(compare), do]
            def gpu_geom(transfer):
                if transfer:
                    gpu.upload(dr, ray_batch)
                gpu.launch("first_hits", count, values)
                if transfer:
                    gpu.download(got, do)
                else:
                    gpu.synchronize()
            def cpu_parallel():
                batch = math.ceil(count / args.lanes)
                jobs = [pool.submit(cpu_geom, pointer(ray_batch, c.c_double, start * 4), min(batch, count - start),
                                    pointer(got, c.c_uint64, start * 3)) for start in range(0, count, batch)]
                for job in jobs:
                    job.result()
            gpu_geom(True)
            if bytes(got) != bytes(expected):
                mismatch = next(i for i, (a, b) in enumerate(zip(got, expected)) if a != b)
                raise AssertionError(f"geometry differs: count={count}, index={mismatch}, gpu={got[mismatch]:016x}, cpu={expected[mismatch]:016x}")
            timings = {name: [] for name in ["cpu_one", "cpu_all", "gpu_resident", "gpu_transfer"]}
            for trial in range(args.repeats + 1):
                operations = [("cpu_one", lambda: cpu_geom(ray_batch, count, got)), ("cpu_all", cpu_parallel),
                              ("gpu_resident", lambda: gpu_geom(False)), ("gpu_transfer", lambda: gpu_geom(True))]
                if trial % 2:
                    operations.reverse()
                for name, op in operations:
                    start = time.perf_counter()
                    op()
                    elapsed = time.perf_counter() - start
                    if name != "gpu_resident" and bytes(got) != bytes(expected):
                        raise AssertionError(f"geometry differs after {name}")
                    if trial:
                        timings[name].append(elapsed)
            row = dict(primitive="first_hit", count=count, bodies=361, exact_bits_and_gas=True,
                       seconds={name: statistics.median(v) for name, v in timings.items()}, samples=timings)
            report["samples"].append(row)
            print(json.dumps({k: v for k, v in row.items() if k != "samples"}), flush=True)
        # RNG uses the same integer stream and 53-bit conversion as upstream.
        for seed, offset in [(0, 0), (46174, 11), (2**64-1, 2**63), (1234567, 999999)]:
            expected = (c.c_double * maximum)()
            got = (c.c_double * maximum)()
            lib.hw_rng(seed, offset, maximum, expected)
            gpu.launch("rng_draws", maximum, [c.c_uint64(seed), c.c_uint64(offset), c.c_uint(maximum), do])
            gpu.download(got, do)
            if bytes(got) != bytes(expected):
                raise AssertionError("RNG stream differs")
        report["rng_exact_values"] = 4 * maximum
        args.out.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
