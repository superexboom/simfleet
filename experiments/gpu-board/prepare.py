#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Prepare one board row on the CPU and snapshot the original WASM memory."""
import argparse
import hashlib
import importlib.util
import json
import pathlib
import sys
import time
import struct


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--python-deps", type=pathlib.Path, required=True)
    p.add_argument("--wasm", type=pathlib.Path, required=True)
    p.add_argument("--native", type=pathlib.Path, required=True)
    p.add_argument("--orders", type=pathlib.Path, required=True)
    p.add_argument("--row", type=int, default=0)
    p.add_argument("--out", type=pathlib.Path, required=True)
    p.add_argument("--duration", type=float)
    p.add_argument("--references", type=int, default=4)
    p.add_argument("--runs", type=int, help="offline benchmark run count; does not request server work")
    p.add_argument("--wire-library", type=pathlib.Path)
    p.add_argument("--request-json",type=pathlib.Path,help="offline diagnostic request override; never a server result")
    args = p.parse_args()
    sys.path.insert(0, str(args.python_deps.resolve()))
    import wasmtime as w
    spec = importlib.util.spec_from_file_location("bench_board", pathlib.Path(__file__).resolve().parents[2] / "tools/bench-board.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    native = mod.Native(args.native)
    orders = json.loads(args.orders.read_text())
    order = orders[args.row] if isinstance(orders, list) else orders
    request = json.loads(native.call("/api/board/order", order))["request"]
    if args.request_json: request=json.loads(args.request_json.read_text(encoding='utf8'))
    if args.duration is not None:
        request["duration"] = args.duration
    if args.runs is not None:
        if not 1 <= args.runs <= 1000000:
            p.error("runs must be between 1 and 1000000")
        request["runs"] = args.runs
    engine = w.Engine()
    module = w.Module.from_file(engine, str(args.wasm))
    if module.imports:
        raise RuntimeError([(i.module, i.name) for i in module.imports])
    store = w.Store(engine)
    exports = w.Instance(store, module, []).exports(store)
    wire_format = exports["sf_wire_format"](store) if "sf_wire_format" in exports else 0
    wire = None
    if wire_format:
        if not args.wire_library: p.error("binary bridge requires --wire-library")
        from wire import Wire
        wire = Wire(args.wire_library)
    memory = exports["memory"]
    data = json.dumps(request).encode()
    ptr = exports["sf_alloc"](store, len(data))
    memory.write(store, data, ptr)
    started = time.perf_counter()
    handle = exports["sf_prepare"](store, ptr, len(data))
    prepare_seconds = time.perf_counter() - started
    exports["sf_free"](store, ptr, len(data))
    references = []
    for index in range(args.references):
        result = exports["sf_run"](store, handle, index)
        pos, size = result & 0xffffffff, result >> 32
        raw = bytes(memory.read(store, pos, pos + size))
        shard = wire.decode(raw) if wire else json.loads(raw)
        reference = json.loads(native.call("/api/board/runs", dict(request=request, **{"from": index, "count": 1})))["shards"][0]
        if json.dumps(shard, sort_keys=True) != json.dumps(reference, sort_keys=True):
            raise AssertionError(f"bridge differs from published native engine at run {index}")
        references.append(shard)
        exports["sf_free"](store, pos, size)
    gas_address = None
    if "sf_gas_probe" in exports:
        before_value = exports["sf_gas_probe"](store, 0) & ((1 << 64) - 1)
        before = bytes(memory.read(store))
        after_value = exports["sf_gas_probe"](store, 1000003) & ((1 << 64) - 1)
        after = bytes(memory.read(store))
        old, new = struct.pack("<Q", before_value), struct.pack("<Q", after_value)
        matches, start = [], 0
        while (found := after.find(new, start)) >= 0:
            if found % 8 == 0 and before[found:found+8] == old: matches.append(found)
            start = found + 1
        if len(matches) != 1: raise AssertionError(f"ambiguous gas counter: {matches}")
        gas_address = matches[0]
        assert exports["sf_gas_probe"](store, -1000003) == before_value
    image = bytes(memory.read(store))
    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "memory.bin").write_bytes(image)
    meta = dict(engine=native.engine, wasm_sha256=hashlib.sha256(args.wasm.read_bytes()).hexdigest(),
                memory_sha256=hashlib.sha256(image).hexdigest(), memory_bytes=len(image),
                stack_pointer=exports["__stack_pointer"].value(store), handle=handle,
                order=order, request=request, references=references, prepare_seconds=prepare_seconds,
                wire_format=wire_format, gas_address=gas_address)
    (args.out / "snapshot.json").write_text(json.dumps(meta, indent=2) + "\n")
    print(json.dumps({k: v for k, v in meta.items() if k not in ("order", "request", "references")}))


if __name__ == "__main__":
    main()
