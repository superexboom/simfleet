#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Batch independent, unmodified board rows on the GPU with optional CPU work."""
import argparse
import ctypes as c
from concurrent.futures import ThreadPoolExecutor
import hashlib
import importlib.util
import json
import pathlib
import statistics
import sys
import time
from memory_budget import limit
from wire import Wire
from artifacts import validate_module

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "hardware"))
from cuda_driver import Cuda


class Job(c.Structure):
    _fields_ = [("snapshot", c.c_uint64), ("bytes", c.c_uint64), ("handle", c.c_uint),
                ("stack", c.c_uint), ("runs", c.c_uint), ("reserved", c.c_uint)]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--module", type=pathlib.Path, required=True)
    p.add_argument("--snapshot", type=pathlib.Path, action="append", required=True)
    p.add_argument("--copies", type=int, default=1, help="offline benchmark copies, never extra server jobs")
    p.add_argument("--native", type=pathlib.Path, required=True)
    p.add_argument("--wire-library", type=pathlib.Path, required=True)
    p.add_argument("--nvrtc-dir", type=pathlib.Path)
    p.add_argument("--private-kib", type=int, default=192)
    p.add_argument("--block", type=int, choices=[1, 8, 16, 32, 64, 128, 256], default=32)
    p.add_argument("--repeats", type=int, default=3)
    p.add_argument("--cpu-rows", type=int, default=0)
    p.add_argument("--cpu-lanes", type=int, default=16)
    p.add_argument("--baseline-first", action="store_true")
    p.add_argument("--out", type=pathlib.Path, required=True)
    args = p.parse_args()
    if min(args.copies, args.private_kib, args.repeats, args.cpu_lanes) < 1 or args.private_kib % 4 or args.cpu_rows < 0:
        p.error("invalid limits")
    limit()
    entries = []
    for path in args.snapshot:
        meta = json.loads((path / "snapshot.json").read_text())
        blob = (path / "memory.bin").read_bytes()
        if meta.get("wire_format") != 1 or hashlib.sha256(blob).hexdigest() != meta["memory_sha256"]:
            raise ValueError("invalid binary snapshot")
        entries.append((meta, blob))
    if len({(m["engine"], m["wasm_sha256"], m["gas_address"]) for m, _ in entries}) != 1:
        raise ValueError("incompatible snapshots")
    module_info = validate_module(args.module, [m for m, _ in entries], "sf_board_packed", args.wire_library)
    rows = [index for _ in range(args.copies) for index in range(len(entries))]
    count = sum(entries[i][0]["request"]["runs"] for i in rows)
    if count > 65536: p.error("batch exceeds 65,536-run bound")
    spec = importlib.util.spec_from_file_location("bench_board", pathlib.Path(__file__).resolve().parents[2] / "tools/bench-board.py")
    mod = importlib.util.module_from_spec(spec); spec.loader.exec_module(mod)
    native, wire = mod.Native(args.native), Wire(args.wire_library)
    if native.engine != entries[0][0]["engine"]: raise ValueError("engine mismatch")
    record = dict(engine=native.engine, rows=len(rows), runs=count, block=args.block,
                  module_sha256=hashlib.sha256(args.module.read_bytes()).hexdigest(), samples=[])
    offsets, owners, indices = [], [], []
    for row_id, entry_id in enumerate(rows):
        n = entries[entry_id][0]["request"]["runs"]
        offsets.append(len(owners)); owners.extend([entry_id] * n); indices.extend(range(n))
    with Cuda(args.nvrtc_dir) as gpu, ThreadPoolExecutor(max_workers=args.cpu_lanes) as pool, ThreadPoolExecutor(max_workers=1) as control:
        if gpu.arch != module_info["architecture"]: raise ValueError("GPU architecture mismatch")
        gpu.driver.cuCtxSetLimit.argtypes = [c.c_int, c.c_size_t]
        gpu.check(gpu.driver.cuCtxSetLimit(0, 16384))
        image = c.create_string_buffer(args.module.read_bytes())
        gpu.check(gpu.driver.cuModuleLoadData(c.byref(gpu.module), image))
        jobs = (Job * len(entries))()
        for i, (meta, blob) in enumerate(entries):
            pointer = gpu.allocate(len(blob))
            source = (c.c_ubyte * len(blob)).from_buffer_copy(blob); gpu.upload(pointer, source)
            jobs[i] = Job(pointer.value, len(blob), meta["handle"], meta["stack_pointer"], meta["request"]["runs"], 0)
        djobs = gpu.allocate(c.sizeof(jobs)); gpu.upload(djobs, jobs)
        own = (c.c_uint * count)(*owners); ind = (c.c_uint * count)(*indices)
        downers = gpu.allocate(c.sizeof(own)); dindices = gpu.allocate(c.sizeof(ind))
        gpu.upload(downers, own); gpu.upload(dindices, ind)
        rounded = (count + 31) // 32 * 32
        capacity = max(len(blob) for _, blob in entries) + 8 * 1024**2
        virtual_pages = capacity // 4096
        private = gpu.allocate(rounded * args.private_kib * 1024)
        page_map = gpu.allocate(rounded * virtual_pages * 4)
        stride = 8192
        outputs = gpu.allocate(count * stride)
        lengths = gpu.allocate(count * 4); statuses = gpu.allocate(count * 4); pages = gpu.allocate(count * 4)
        result = (c.c_ubyte * (count * stride))()
        sizes = (c.c_uint * count)(); status = (c.c_uint * count)(); used = (c.c_uint * count)()
        gpu.driver.cuMemsetD8_v2.argtypes = [c.c_uint64, c.c_ubyte, c.c_size_t]
        gpu.launch("sf_setup", 1, [statuses])
        parameters = [djobs, downers, dindices, private, page_map, c.c_uint64(capacity),
                      c.c_uint(args.private_kib//4), c.c_uint(virtual_pages), c.c_uint(count),
                      outputs, c.c_uint(stride), lengths, pages, statuses]
        expected_scores = []
        for meta, _ in entries:
            expected_scores.append(json.loads(native.row(meta["request"], meta["order"]["ruler"], pool, 8)))
        def cpu_rows(n):
            for i in range(n):
                index = i % len(entries); meta = entries[index][0]
                score = json.loads(native.row(meta["request"], meta["order"]["ruler"], pool, 8))
                if score != expected_scores[index]: raise AssertionError("CPU answer changed")
        cpu_future = None; mixed_start = baseline_seconds = None
        for trial in range(args.repeats + 1):
            if args.cpu_rows and trial == 1:
                if args.baseline_first:
                    started = time.perf_counter(); cpu_rows(args.cpu_rows + len(rows)*args.repeats)
                    baseline_seconds = time.perf_counter() - started
                mixed_start = time.perf_counter(); cpu_future = control.submit(cpu_rows, args.cpu_rows)
            started = time.perf_counter(); cpu_started = time.thread_time()
            gpu.check(gpu.driver.cuMemsetD8_v2(page_map, 0, rounded*virtual_pages*4))
            gpu.launch("sf_board_packed", count, parameters, block_size=args.block)
            gpu.synchronize()
            compute = time.perf_counter()-started
            gpu.download(status, statuses); gpu.download(sizes, lengths)
            if any(status): raise RuntimeError(f"GPU failures: {[(i,x) for i,x in enumerate(status) if x][:16]}")
            gpu.download(result, outputs); gpu.download(used, pages)
            merge_started = time.perf_counter()
            scores = []
            for row_id, entry_id in enumerate(rows):
                meta = entries[entry_id][0]; n = meta["request"]["runs"]; offset = offsets[row_id]
                data = (c.c_ubyte * (stride*n)).from_buffer(result, stride*offset)
                lens = (c.c_uint * n).from_buffer(sizes, 4*offset)
                score = wire.score(meta["request"], meta["order"]["ruler"], data, lens, stride)
                if score != expected_scores[entry_id]: raise AssertionError(f"GPU score mismatch in row {row_id}")
                scores.append(score)
            merged = time.perf_counter()-merge_started
            sample = dict(compute_seconds=compute, merge_seconds=merged, total_seconds=time.perf_counter()-started,
                          host_cpu_seconds=time.thread_time()-cpu_started, max_private_pages=max(used))
            if trial: record["samples"].append(sample)
            print(json.dumps(dict(trial=trial, **sample)), flush=True)
        if cpu_future:
            cpu_future.result(); elapsed = time.perf_counter()-mixed_start
            if baseline_seconds is None:
                started = time.perf_counter(); cpu_rows(args.cpu_rows+len(rows)*args.repeats)
                baseline_seconds = time.perf_counter()-started
            record["mixed"] = dict(cpu_rows=args.cpu_rows, gpu_rows=len(rows)*args.repeats, seconds=elapsed,
                                   cpu_only_seconds=baseline_seconds, speedup=baseline_seconds/elapsed)
        # Verify full shards, not only score, for every distinct task shape.
        view = memoryview(result).cast("B")
        for entry_id, (meta, _) in enumerate(entries):
            row_id = rows.index(entry_id); first = offsets[row_id]; n = meta["request"]["runs"]
            expected = json.loads(native.call("/api/board/runs", dict(request=meta["request"], **{"from":0,"count":n})))["shards"]
            for index, reference in enumerate(expected):
                at = first+index; raw = bytes(view[at*stride:at*stride+sizes[at]])
                if json.dumps(wire.decode(raw), sort_keys=True) != json.dumps(reference, sort_keys=True):
                    raise AssertionError(f"shard mismatch {entry_id}/{index}")
            for other, which in enumerate(rows):
                if which != entry_id or other == row_id: continue
                for index in range(n):
                    a, b = first+index, offsets[other]+index
                    if sizes[a] != sizes[b] or view[a*stride:a*stride+sizes[a]] != view[b*stride:b*stride+sizes[b]]:
                        raise AssertionError("repeated task shard changed")
        record.update(exact_shards=count, exact_scores=len(rows), scores=scores,
                      median_seconds=statistics.median(x["total_seconds"] for x in record["samples"]))
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(record,indent=2)+"\n")
    print(json.dumps({k:v for k,v in record.items() if k not in ("scores","samples")}),flush=True)


if __name__ == "__main__": main()
