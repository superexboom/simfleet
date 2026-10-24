#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Execute whole upstream board runs on CUDA, then compare every shard."""
import argparse
import ctypes as c
import hashlib
import importlib.util
import json
import pathlib
import sys
import time
import statistics
from concurrent.futures import ThreadPoolExecutor
from memory_budget import limit
from artifacts import validate_module

limit()

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "hardware"))
from cuda_driver import Cuda


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--ptx", type=pathlib.Path, required=True)
    p.add_argument("--snapshot", type=pathlib.Path, required=True)
    p.add_argument("--native", type=pathlib.Path, required=True)
    p.add_argument("--nvrtc-dir", type=pathlib.Path)
    p.add_argument("--count", type=int, default=4)
    p.add_argument("--from-index", type=int, default=0)
    p.add_argument("--stack-kib", type=int, default=16)
    p.add_argument("--jit-level", type=int, choices=range(5), default=0)
    p.add_argument("--cow", action="store_true")
    p.add_argument("--private-kib", type=int, default=512)
    p.add_argument("--repeats", type=int, default=1)
    p.add_argument("--warmup", action="store_true")
    p.add_argument("--block", type=int, choices=[8, 16, 32, 64, 128, 256], default=32)
    p.add_argument("--wire-library", type=pathlib.Path)
    p.add_argument("--cpu-rows", type=int, default=0, help="measure additional complete CPU rows while GPU batches run")
    p.add_argument("--cpu-lanes", type=int, default=16)
    p.add_argument("--out", type=pathlib.Path, required=True)
    args = p.parse_args()
    if args.count < 1 or args.from_index < 0 or args.from_index + args.count > 1000000 or args.repeats < 1:
        p.error("invalid run range")
    if args.private_kib < 4 or args.private_kib % 4:
        p.error("private KiB must be a positive multiple of 4")
    meta = json.loads((args.snapshot / "snapshot.json").read_text())
    module_info = validate_module(args.ptx, [meta], "sf_board", args.wire_library)
    wire = None
    if meta.get("wire_format"):
        if not args.wire_library: p.error("binary snapshot requires --wire-library")
        from wire import Wire
        wire = Wire(args.wire_library)
    if args.cpu_rows and (not wire or not args.warmup or args.from_index or args.count != meta["request"]["runs"]):
        p.error("mixed measurement requires binary complete rows and --warmup")
    spec = importlib.util.spec_from_file_location("bench_board", pathlib.Path(__file__).resolve().parents[2] / "tools/bench-board.py")
    mod = importlib.util.module_from_spec(spec); spec.loader.exec_module(mod)
    native = mod.Native(args.native)
    if native.engine != meta["engine"]: raise AssertionError("native reference engine does not match snapshot")
    snapshot = (args.snapshot / "memory.bin").read_bytes()
    assert hashlib.sha256(snapshot).hexdigest() == meta["memory_sha256"]
    output_stride = 32768
    report = dict(count=args.count, start=args.from_index, engine=meta["engine"], memory_bytes=len(snapshot),
                  module_sha256=hashlib.sha256(args.ptx.read_bytes()).hexdigest(), cow=args.cow, block=args.block, samples=[])
    with Cuda(args.nvrtc_dir) as gpu:
        if gpu.arch != module_info["architecture"]: raise ValueError("GPU architecture mismatch")
        gpu.driver.cuCtxSetLimit.argtypes = [c.c_int, c.c_size_t]
        gpu.driver.cuCtxSetLimit.restype = c.c_int
        gpu.check(gpu.driver.cuCtxSetLimit(0, args.stack_kib * 1024))
        ptx = c.create_string_buffer(args.ptx.read_bytes())
        started = time.perf_counter()
        print("Loading complete CUDA module", flush=True)
        gpu.driver.cuModuleLoadDataEx.argtypes = [c.POINTER(c.c_void_p), c.c_void_p, c.c_uint, c.POINTER(c.c_int), c.POINTER(c.c_void_p)]
        gpu.driver.cuModuleLoadDataEx.restype = c.c_int
        jit_options = (c.c_int * 1)(7)
        jit_values = (c.c_void_p * 1)(args.jit_level)
        gpu.check(gpu.driver.cuModuleLoadDataEx(c.byref(gpu.module), ptx, 1, jit_options, jit_values))
        report["module_load_seconds"] = time.perf_counter() - started
        print("Loaded; allocating snapshot instances", flush=True)
        image = (c.c_ubyte * len(snapshot)).from_buffer_copy(snapshot)
        dimage = gpu.allocate(len(snapshot))
        rounded_count = (args.count + 31) // 32 * 32
        capacity = len(snapshot) + 8 * 1024**2
        page_bytes=module_info.get("cow_page_bytes",4096)
        index_bytes=module_info.get("page_index_bytes",4)
        map_region_bytes=module_info.get("page_map_region_bytes",page_bytes)
        if (page_bytes,index_bytes,map_region_bytes) not in [(4096,4,4096),(1024,2,1024),(1024,2,65536)]:raise ValueError("unsupported COW layout")
        virtual_pages = capacity // map_region_bytes
        private_pages = args.private_kib * 1024 // page_bytes
        memories = gpu.allocate(args.private_kib * 1024 * rounded_count if args.cow else len(snapshot) * args.count)
        page_map = gpu.allocate(virtual_pages * rounded_count * index_bytes) if args.cow else None
        pages_used = gpu.allocate(args.count * 4) if args.cow else None
        outputs = gpu.allocate(output_stride * args.count)
        lengths = gpu.allocate(args.count * 4)
        statuses = gpu.allocate(args.count * 4)
        tables = None if args.cow else gpu.allocate(args.count * 354 * 32)
        gpu.upload(dimage, image)
        gpu.launch("sf_setup", 1, [statuses])
        status = (c.c_uint * args.count)()
        sizes = (c.c_uint * args.count)()
        result = (c.c_ubyte * (output_stride * args.count))()
        cpu_pool = ThreadPoolExecutor(max_workers=args.cpu_lanes)
        control = ThreadPoolExecutor(max_workers=1)
        def cpu_rows(count):
            answer = None
            for _ in range(count):
                row = native.row(meta["request"], meta["order"]["ruler"], cpu_pool, 8)
                if answer is not None and row != answer: raise AssertionError("CPU repeat changed answer")
                answer = row
            return answer
        cpu_future = None
        mixed_started = None
        parameters = [memories, c.c_uint64(len(snapshot)), c.c_uint64(len(snapshot)),
                   c.c_uint(meta["stack_pointer"]), c.c_uint(meta["handle"]), c.c_uint(args.from_index), c.c_uint(args.count),
                   outputs, c.c_uint(output_stride), lengths, tables, statuses]
        if args.cow:
            parameters = [dimage, memories, page_map, c.c_uint64(capacity), c.c_uint64(len(snapshot)), c.c_uint(private_pages),
                          c.c_uint(virtual_pages), c.c_uint(meta["stack_pointer"]), c.c_uint(meta["handle"]),
                          c.c_uint(args.from_index), c.c_uint(args.count), outputs, c.c_uint(output_stride), lengths, pages_used, statuses]
            gpu.driver.cuMemsetD8_v2.argtypes = [c.c_uint64, c.c_ubyte, c.c_size_t]
            gpu.driver.cuMemsetD8_v2.restype = c.c_int
        for trial in range(args.repeats + int(args.warmup)):
            if args.cpu_rows and trial == 1:
                cpu_rows(1)
                mixed_started = time.perf_counter()
                cpu_future = control.submit(cpu_rows, args.cpu_rows)
            started = time.perf_counter()
            if args.cow:
                gpu.check(gpu.driver.cuMemsetD8_v2(page_map, 0, virtual_pages * rounded_count * index_bytes))
            else:
                gpu.launch("sf_clone", min(len(snapshot) * args.count, 65536),
                           [dimage, memories, c.c_uint64(len(snapshot)), c.c_uint(args.count)])
            gpu.synchronize()
            cloned = time.perf_counter() - started
            print(f"Computing complete board runs, pass {trial + 1}", flush=True)
            started = time.perf_counter()
            cpu_started = time.thread_time()
            gpu.launch("sf_board", args.count, parameters, block_size=args.block)
            gpu.synchronize()
            computed = time.perf_counter() - started
            host_cpu = time.thread_time() - cpu_started
            fold_time = 0.0
            if wire and args.from_index == 0 and args.count == meta["request"]["runs"]:
                gpu.download(status, statuses)
                gpu.download(sizes, lengths)
                if any(status):
                    report['statuses']=list(status)
                    args.out.parent.mkdir(parents=True,exist_ok=True)
                    args.out.write_text(json.dumps(report,indent=2))
                    raise RuntimeError(f"GPU row incomplete: {[(i,code) for i,code in enumerate(status) if code][:16]}")
                gpu.download(result, outputs)
                before_fold = time.perf_counter()
                score = wire.score(meta["request"], meta["order"]["ruler"], result, sizes, output_stride)
                fold_time = time.perf_counter() - before_fold
                if "exact_score" in report and report["exact_score"] != score: raise AssertionError("GPU repeat changed score")
                report["exact_score"] = score
            if trial or not args.warmup:
                report["samples"].append(dict(reset_seconds=cloned, compute_seconds=computed, host_cpu_seconds=host_cpu, fold_and_score_seconds=fold_time))
        if cpu_future:
            cpu_answer = json.loads(cpu_future.result())
            if cpu_answer != report["exact_score"]: raise AssertionError("mixed CPU/GPU scores differ")
            mixed_seconds = time.perf_counter() - mixed_started
            started = time.perf_counter()
            cpu_rows(args.cpu_rows + args.repeats)
            baseline_seconds = time.perf_counter() - started
            report["mixed"] = dict(cpu_rows=args.cpu_rows, gpu_rows=args.repeats, cpu_lanes=args.cpu_lanes,
                                   mixed_seconds=mixed_seconds, cpu_only_seconds=baseline_seconds,
                                   speedup=baseline_seconds/mixed_seconds)
        control.shutdown(wait=True)
        cpu_pool.shutdown(wait=True)
        report["compute_seconds"] = statistics.median(s["compute_seconds"] for s in report["samples"])
        gpu.download(status, statuses)
        gpu.download(sizes, lengths)
        if args.cow and not any(status):
            used = (c.c_uint * args.count)()
            gpu.download(used, pages_used)
            report["private_pages"] = dict(limit=private_pages, min=min(used), max=max(used),page_bytes=page_bytes)
        report["statuses"] = list(status)
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(json.dumps(report, indent=2))
        if any(status):
            failed = [(i, code) for i, code in enumerate(status) if code]
            raise RuntimeError(f"GPU lane failures: {len(failed)}; first: {failed[:16]}")
        gpu.download(result, outputs)
        if wire and args.from_index == 0 and args.count == meta["request"]["runs"]:
            started = time.perf_counter()
            report["exact_score"] = wire.score(meta["request"], meta["order"]["ruler"], result, sizes, output_stride)
            report["fold_and_score_seconds"] = time.perf_counter() - started
        shards = [(wire.decode if wire else json.loads)(bytes(result[i * output_stride:i * output_stride + sizes[i]])) for i in range(args.count)]
    expected = json.loads(native.call("/api/board/runs", dict(request=meta["request"], **{"from": args.from_index, "count": args.count})))["shards"]
    for index, (actual, reference) in enumerate(zip(shards, expected, strict=True)):
        if json.dumps(actual, sort_keys=True) != json.dumps(reference, sort_keys=True):
            args.out.with_suffix(".mismatch.json").write_text(json.dumps(dict(index=index, actual=actual, reference=reference), indent=2))
            raise AssertionError(f"GPU shard differs at run {args.from_index + index}")
    report["exact_shards"] = args.count
    if args.from_index == 0 and args.count == meta["request"]["runs"]:
        if wire:
            score = report["exact_score"]
        else:
            acc = None
            started = time.perf_counter()
            for offset in range(0, args.count, 64):
                acc = json.loads(native.call("/api/board/fold", dict(request=meta["request"], acc=acc, pieces=shards[offset:offset+64])))["acc"]
            score = json.loads(native.call("/api/board/score", dict(request=meta["request"], ruler=meta["order"]["ruler"], acc=acc)))
            report["fold_and_score_seconds"] = time.perf_counter() - started
        with ThreadPoolExecutor(max_workers=args.cpu_lanes) as pool:
            native.row(meta["request"], meta["order"]["ruler"], pool, 8)
            started = time.perf_counter()
            native_score = json.loads(native.row(meta["request"], meta["order"]["ruler"], pool, 8))
            report["cpu_full_row_seconds"] = time.perf_counter() - started
        if json.dumps(score, sort_keys=True) != json.dumps(native_score, sort_keys=True):
            raise AssertionError("whole GPU row score differs from native")
        report["exact_score"] = score
    report["shards"] = shards
    args.out.write_text(json.dumps(report, indent=2))
    print(json.dumps({k: v for k, v in report.items() if k not in ("shards", "statuses")}), flush=True)


if __name__ == "__main__": main()
