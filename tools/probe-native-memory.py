#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Windows native API allocation probe; never contacts the task server.

python tools/probe-native-memory.py <engine.dll> --loops 200000 --mode runs --lanes 6
python tools/probe-native-memory.py <official.dll> --pack <pack.data.json> --mode reload --loops 20

Each invocation owns its process. Reload waits for every API thread to exit
before unloading, just as a drained engine must. Private commit and working
set are both reported; the Python host's own warmup is included.
"""
import argparse
import ctypes as c
import json
import pathlib
import time
from concurrent.futures import ThreadPoolExecutor


class Counters(c.Structure):
    _fields_ = [("cb", c.c_ulong), ("faults", c.c_ulong)] + [(x, c.c_size_t) for x in
        ("peak_ws", "ws", "peak_pool", "pool", "peak_nonpaged", "nonpaged", "page", "peak_page", "private")]


def memory():
    m = Counters()
    m.cb = c.sizeof(m)
    kernel = c.WinDLL("kernel32", use_last_error=True)
    kernel.GetCurrentProcess.restype = c.c_void_p
    get = c.WinDLL("psapi", use_last_error=True).GetProcessMemoryInfo
    get.argtypes = [c.c_void_p, c.POINTER(Counters), c.c_ulong]
    if not get(kernel.GetCurrentProcess(), c.byref(m), m.cb):
        raise c.WinError(c.get_last_error())
    return {"private_mib": round(m.private / 2**20, 2), "working_set_mib": round(m.ws / 2**20, 2)}


def main():
    p = argparse.ArgumentParser()
    p.add_argument("library", type=pathlib.Path)
    p.add_argument("--pack", type=pathlib.Path)
    p.add_argument("--loops", type=int, default=10000)
    p.add_argument("--mode", choices=["order", "runs", "reload", "contexts"], default="order")
    p.add_argument("--lanes", type=int, default=1)
    a = p.parse_args()
    order = {"record": {"weapon": "braton_prime", "mods": ["serration", "split_chamber", "point_strike", "vital_sense", "infected_clip", "stormbringer", "hellfire", "speed_trigger"], "evolutions": ["braton_prime_evo1_incarnon_form", "braton_prime_munitions_grit", "braton_prime_voids_guidance", "braton_prime_critical_parallel"], "arcanes": ["primary_deadhead"]}, "ruler": "demolisher", "mode": "base"}
    start = time.monotonic()
    def report(i):
        print(json.dumps({"iteration": i, "seconds": round(time.monotonic()-start, 3), **memory()}), flush=True)
    def load():
        lib = c.CDLL(str(a.library.resolve()))
        own = hasattr(lib, "sfe_api")
        api = getattr(lib, "sfe_api" if own else "wfsim_api")
        free = getattr(lib, "sfe_free" if own else "wfsim_free")
        api.argtypes = [c.c_char_p, c.c_size_t, c.c_char_p, c.c_size_t, c.POINTER(c.c_size_t)]
        api.restype = c.c_void_p
        free.argtypes = [c.c_void_p, c.c_size_t]
        def take(ptr, n):
            if not ptr:
                raise RuntimeError("native call failed")
            try:
                return c.string_at(ptr, n.value)
            finally:
                free(ptr, n.value)
        if a.pack and not own:
            install = lib.wfsim_install_data
            install.argtypes = [c.c_char_p, c.c_size_t, c.POINTER(c.c_size_t)]
            install.restype = c.c_void_p
            b = a.pack.read_bytes()
            n = c.c_size_t()
            why = take(install(b, len(b), c.byref(n)), n)
            if why:
                raise RuntimeError(why)
        def call(path, body):
            n = c.c_size_t()
            result = take(api(path, len(path), body, len(body), c.byref(n)), n)
            out = json.loads(result)
            if out.get("ok") is False or "error" in out:
                raise RuntimeError(out)
            return out
        return lib, call
    report(0)
    if a.mode == "reload":
        import _ctypes
        for i in range(a.loops):
            lib, call = load()
            with ThreadPoolExecutor(max_workers=1) as pool:
                pool.submit(call, b"/api/board/order", json.dumps(order).encode()).result()
            del call
            _ctypes.FreeLibrary(lib._handle)
            del lib
            report(i+1)
        return
    lib, call = load()
    with ThreadPoolExecutor(max_workers=a.lanes) as pool:
        req = pool.submit(call, b"/api/board/order", json.dumps(order).encode()).result()["request"]
        if a.mode == "contexts":
            new=lib.sfe_board_new;new.argtypes=[c.c_char_p,c.c_size_t];new.restype=c.c_void_p
            runs=lib.sfe_board_runs;runs.argtypes=[c.c_void_p,c.c_uint32,c.c_uint32];runs.restype=c.c_int64
            drop=lib.sfe_board_drop;drop.argtypes=[c.c_void_p]
            req["duration"]=0.01;req["runs"]=14
            body=json.dumps({"request":req,"ruler":"demolisher"}).encode()
            for i in range(a.loops):
                ptr=new(body,len(body))
                if not ptr: raise RuntimeError('new context')
                futures=[pool.submit(runs,ptr,start,7) for start in (7,0)]
                try:
                    for f in futures:
                        if f.result()<0: raise RuntimeError('context run')
                finally:
                    # Even on error, all users must finish before context drop.
                    for f in futures:f.result()
                    drop(ptr)
                if i%100==0 or i+1==a.loops:report(i+1)
            return
        path, body = b"/api/board/order", json.dumps(order).encode()
        if a.mode == "runs":
            req["duration"] = 0.01
            path, body = b"/api/board/runs", json.dumps({"request": req, "from": 0, "count": 1}).encode()
        for i in range(0, a.loops, 100):
            futures = [pool.submit(call, path, body) for _ in range(min(100, a.loops-i))]
            for f in futures:
                f.result()
            if i % 1000 == 0 or i+100 >= a.loops:
                report(min(i+100, a.loops))
    report(a.loops)


if __name__ == "__main__":
    main()
