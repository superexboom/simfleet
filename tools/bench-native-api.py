#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Offline A/B of /api/board/runs on two native DLLs; compare every shard bit.

python tools/bench-native-api.py --baseline old.dll --candidate new.dll \
    --rows orders.json --out results.json --count 100 --repeats 3
No compute-server traffic. The candidate may carry an experimental engine id.
"""
import argparse
import ctypes as c
import hashlib
import json
import pathlib
import statistics
import time


class Native:
    def __init__(self, path):
        self.lib = c.CDLL(str(path.resolve()))
        self.api = self.lib.sfe_api
        self.api.argtypes = [c.c_char_p, c.c_size_t, c.c_char_p, c.c_size_t, c.POINTER(c.c_size_t)]
        self.api.restype = c.c_void_p
        self.free = self.lib.sfe_free
        self.free.argtypes = [c.c_void_p, c.c_size_t]
        self.lib.sfe_engine_id.restype = c.c_char_p
        self.engine = self.lib.sfe_engine_id().decode()

    def call(self, path, body):
        path = path.encode()
        size = c.c_size_t()
        p = self.api(path, len(path), body, len(body), c.byref(size))
        if not p:
            raise RuntimeError("native engine panicked")
        try:
            return c.string_at(p, size.value)
        finally:
            self.free(p, size.value)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ("baseline", "candidate", "rows", "out"):
        p.add_argument("--" + name, type=pathlib.Path, required=True)
    p.add_argument("--count", type=int, default=100)
    p.add_argument("--repeats", type=int, default=3)
    p.add_argument("--full-row", action="store_true", help="time all runs, their ordered folds and the final score")
    args = p.parse_args()
    libs = {name: Native(getattr(args, name)) for name in ("baseline", "candidate")}
    data = {"engines": {name: lib.engine for name, lib in libs.items()}, "count": args.count, "full_row": args.full_row,
            "repeats": args.repeats, "rows": [],
            "sha256": {name: hashlib.sha256(getattr(args, name).read_bytes()).hexdigest() for name in libs}}
    orders = json.loads(args.rows.read_text(encoding="utf-8"))
    for number, order in enumerate(orders):
        order_body = json.dumps(order).encode()
        orders_out = [lib.call("/api/board/order", order_body) for lib in libs.values()]
        assert orders_out[0] == orders_out[1], "order differs"
        o = json.loads(orders_out[0])
        label = f"{order['record']['weapon']}/{order['ruler']}/{order['mode']}"
        if not o.get("ok"):
            raise RuntimeError((label, o))
        req = o["request"]
        def full_row(lib):
            acc = None
            for start in range(0, req["runs"], args.count):
                raw = lib.call("/api/board/runs", json.dumps({"request": req, "from": start, "count": min(args.count, req["runs"]-start)}).encode())
                runs = json.loads(raw)
                assert runs.get("ok"), runs
                fold = json.loads(lib.call("/api/board/fold", json.dumps({"request": req, "acc": acc, "pieces": runs["shards"]}).encode()))
                assert fold.get("ok"), fold
                acc = fold["acc"]
            return lib.call("/api/board/score", json.dumps({"request": req, "ruler": order["ruler"], "acc": acc}).encode())
        timings = {name: [] for name in libs}
        for iteration in range(args.repeats + 1):
            body = json.dumps({"request": req, "from": 11, "count": min(args.count, req["runs"]-11)}).encode()
            answers = {}
            for name in (("baseline", "candidate") if iteration % 2 == 0 else ("candidate", "baseline")):
                started = time.perf_counter()
                answers[name] = full_row(libs[name]) if args.full_row else libs[name].call("/api/board/runs", body)
                seconds = time.perf_counter() - started
                if iteration:
                    timings[name].append(seconds)
            # Exact serialized JSON comparison also covers RNG state, all
            # report accumulators, counters, series and gas for every run.
            assert answers["baseline"] == answers["candidate"], f"shards differ: {label}"
            assert json.loads(answers["candidate"]).get("ok"), label
        med = {name: statistics.median(values) for name, values in timings.items()}
        row = dict(label=label, times=timings, median_seconds=med,
                   speedup=med["baseline"]/med["candidate"], same=True,
                   output_bytes=len(answers["baseline"]))
        data["rows"].append(row)
        args.out.write_text(json.dumps(data, indent=2), encoding="utf-8")
        print(json.dumps(row), flush=True)


if __name__ == "__main__":
    main()
