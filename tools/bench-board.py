#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Offline, alternating native board benchmarks with exact answer checks.

Uses the production opaque board ABI, persistent worker threads and complete
ordered scores. Includes preparation, simulation, reduction and scoring time.
No network requests or volunteer leases. Output includes inputs and DLL hashes.
"""
import argparse
import concurrent.futures as futures
import ctypes as c
import hashlib
import json
import pathlib
import statistics
import time


class Native:
    def __init__(self, path):
        self.path = path.resolve()
        self.lib = c.CDLL(str(self.path))
        for name, args, result in [
            ("sfe_api", [c.c_char_p, c.c_size_t, c.c_char_p, c.c_size_t, c.POINTER(c.c_size_t)], c.c_void_p),
            ("sfe_free", [c.c_void_p, c.c_size_t], None),
            ("sfe_board_new", [c.c_char_p, c.c_size_t], c.c_void_p),
            ("sfe_board_runs", [c.c_void_p, c.c_uint32, c.c_uint32], c.c_int64),
            ("sfe_board_score", [c.c_void_p, c.POINTER(c.c_size_t)], c.c_void_p),
            ("sfe_board_drop", [c.c_void_p], None),
            ("sfe_engine_id", [], c.c_char_p),
        ]:
            fn = getattr(self.lib, name)
            fn.argtypes, fn.restype = args, result
        self.engine = self.lib.sfe_engine_id().decode()

    def result(self, ptr, size):
        if not ptr:
            raise RuntimeError("native call failed")
        try:
            value = c.string_at(ptr, size.value)
        finally:
            self.lib.sfe_free(ptr, size.value)
        if not json.loads(value).get("ok"):
            raise RuntimeError(value.decode())
        return value

    def call(self, route, value):
        route, body, size = route.encode(), json.dumps(value).encode(), c.c_size_t()
        return self.result(self.lib.sfe_api(route, len(route), body, len(body), c.byref(size)), size)

    def row(self, request, ruler, pool, batch):
        body = json.dumps(dict(request=request, ruler=ruler)).encode()
        ctx = self.lib.sfe_board_new(body, len(body))
        if not ctx:
            raise RuntimeError("board creation failed")
        jobs = []
        try:
            for start in range(0, request["runs"], batch):
                jobs.append(pool.submit(self.lib.sfe_board_runs, ctx, start, min(batch, request["runs"] - start)))
            # Every submitted call must return before releasing the row, including
            # when another range fails. This is also the DLL lifetime boundary.
            futures.wait(jobs)
            for job in jobs:
                if job.result() < 0:
                    raise RuntimeError("board range failed")
            size = c.c_size_t()
            return self.result(self.lib.sfe_board_score(ctx, c.byref(size)), size)
        finally:
            if jobs:
                futures.wait(jobs)
            self.lib.sfe_board_drop(ctx)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--library", action="append", required=True, metavar="NAME=PATH")
    parser.add_argument("--orders", type=pathlib.Path, required=True)
    parser.add_argument("--out", type=pathlib.Path, required=True)
    parser.add_argument("--lanes", default="1,8,16")
    parser.add_argument("--runs", type=int, help="override runs for short profiling; omitted for original rows")
    parser.add_argument("--batch", type=int, default=8)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--rounds", type=int, default=1, help="repeat the full corpus per timed sample")
    parser.add_argument("--expect", type=pathlib.Path, help="compare with saved answers from this harness")
    args = parser.parse_args()
    if min(args.batch, args.repeats, args.rounds, args.runs if args.runs is not None else 1) < 1:
        parser.error("counts must be positive")
    lanes = [int(n) for n in args.lanes.split(",")]
    if min(lanes) < 1:
        parser.error("lanes must be positive")
    libs = {}
    for specification in args.library:
        name, path = specification.split("=", 1)
        if name in libs:
            parser.error("library names must be unique")
        libs[name] = Native(pathlib.Path(path))
    if len({lib.engine for lib in libs.values()}) != 1:
        parser.error("engine identities differ")
    orders = json.loads(args.orders.read_text(encoding="utf-8"))
    if not isinstance(orders, list):
        orders = [orders]
    corpus, answers = [], {}
    saved = json.loads(args.expect.read_text(encoding="utf-8")) if args.expect else None
    if saved and (saved["engine"] != next(iter(libs.values())).engine or
                  saved["orders_sha256"] != hashlib.sha256(args.orders.read_bytes()).hexdigest()):
        parser.error("saved answers belong to a different engine or order corpus")
    for index, order in enumerate(orders):
        prepared = [lib.call("/api/board/order", order) for lib in libs.values()]
        if any(item != prepared[0] for item in prepared):
            raise AssertionError(f"order preparation differs: {index}")
        req = json.loads(prepared[0])["request"]
        if args.runs:
            req["runs"] = args.runs
        label = f"{index}:{order['record']['weapon']}/{order['ruler']}/{order['mode']}"
        corpus.append((label, req, order["ruler"]))
    report = dict(
        engine=next(iter(libs.values())).engine,
        libraries={name: dict(path=str(lib.path), sha256=hashlib.sha256(lib.path.read_bytes()).hexdigest()) for name, lib in libs.items()},
        orders_sha256=hashlib.sha256(args.orders.read_bytes()).hexdigest(),
        corpus=[dict(label=label, runs=req["runs"]) for label, req, _ in corpus],
        batch=args.batch, rounds=args.rounds, samples=[], summary=[],
    )
    args.out.parent.mkdir(parents=True, exist_ok=True)
    for count in lanes:
        with futures.ThreadPoolExecutor(max_workers=count) as pool:
            for trial in range(args.repeats + 1):
                names = list(libs)
                if trial % 2:
                    names.reverse()
                for name in names:
                    elapsed, work = 0.0, 0
                    rows = []
                    for _ in range(args.rounds):
                        for label, req, ruler in corpus:
                            start = time.perf_counter()
                            answer = libs[name].row(req, ruler, pool, args.batch)
                            seconds = time.perf_counter() - start
                            expected = answers.setdefault(label, answer)
                            if expected != answer:
                                args.out.with_suffix(".mismatch.json").write_text(json.dumps(dict(label=label, expected=json.loads(expected), actual=json.loads(answer)), indent=2))
                                raise AssertionError(f"full answer differs: {label}, {name}, lanes={count}")
                            result = json.loads(answer)
                            if saved and json.dumps(result, sort_keys=True) != json.dumps(saved["answers"].get(label), sort_keys=True):
                                raise AssertionError(f"saved answer differs: {label}, {name}")
                            elapsed += seconds
                            work += result.get("work", 0)
                            rows.append(dict(label=label, seconds=seconds))
                    if trial:
                        sample = dict(library=name, lanes=count, trial=trial, seconds=elapsed, work=work, work_per_second=work / elapsed, rows=rows)
                        report["samples"].append(sample)
                        args.out.write_text(json.dumps(report, indent=2), encoding="utf-8")
                        print(json.dumps({key: value for key, value in sample.items() if key != "rows"}), flush=True)
        for name in libs:
            times = [s["seconds"] for s in report["samples"] if s["library"] == name and s["lanes"] == count]
            report["summary"].append(dict(library=name, lanes=count, median_seconds=statistics.median(times), min_seconds=min(times), max_seconds=max(times)))
    report["answers"] = {label: json.loads(value) for label, value in answers.items()}
    args.out.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report["summary"], indent=2))


if __name__ == "__main__":
    main()
