#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Assemble separate PTX units and link a loadable NVIDIA cubin."""
import argparse
import ctypes as c
import hashlib
import json
import pathlib
import time
from memory_budget import limit

limit()
p = argparse.ArgumentParser(description=__doc__)
p.add_argument("source", type=pathlib.Path)
p.add_argument("out", type=pathlib.Path)
p.add_argument("--library", type=pathlib.Path, required=True)
p.add_argument("--opt", choices=["0", "3"], default="3")
p.add_argument("--architecture", choices=["sm_75", "sm_89", "sm_120"], default="sm_120")
p.add_argument("--max-registers", type=int, help="cap registers per thread to trade spills for resident warps")
args = p.parse_args()
lib = c.CDLL(str(args.library.resolve()))
major, minor = c.c_uint(), c.c_uint()
lib.nvJitLinkVersion.argtypes = [c.POINTER(c.c_uint), c.POINTER(c.c_uint)]
if lib.nvJitLinkVersion(c.byref(major), c.byref(minor)):
    raise RuntimeError("cannot read nvJitLink version")
api_version = f"{major.value}_{minor.value}"
functions = {}
for name, params in [
    ("Create", [c.POINTER(c.c_void_p), c.c_uint, c.POINTER(c.c_char_p)]),
    ("Destroy", [c.POINTER(c.c_void_p)]),
    ("AddData", [c.c_void_p, c.c_int, c.c_void_p, c.c_size_t, c.c_char_p]),
    ("Complete", [c.c_void_p]),
    ("GetLinkedCubinSize", [c.c_void_p, c.POINTER(c.c_size_t)]),
    ("GetLinkedCubin", [c.c_void_p, c.c_void_p]),
    ("GetErrorLogSize", [c.c_void_p, c.POINTER(c.c_size_t)]),
    ("GetErrorLog", [c.c_void_p, c.c_void_p]),
    ("GetInfoLogSize", [c.c_void_p, c.POINTER(c.c_size_t)]),
    ("GetInfoLog", [c.c_void_p, c.c_void_p]),
]:
    fn = getattr(lib, f"__nvJitLink{name}_{api_version}")
    fn.argtypes, fn.restype = params, c.c_int
    functions[name] = fn
handle = c.c_void_p()


def log(kind):
    size = c.c_size_t()
    functions[f"Get{kind}LogSize"](handle, c.byref(size))
    data = c.create_string_buffer(size.value)
    functions[f"Get{kind}Log"](handle, data)
    return data.value.decode(errors="replace")


def check(code):
    if code:
        error = log("Error") if handle else ""
        args.out.with_suffix(".error.log").write_text(error)
        raise RuntimeError(f"nvJitLink {code}: {error}")


options = [f"-arch={args.architecture}".encode(), f"-O{args.opt}".encode(), b"-fma=0", b"-prec-div=1", b"-prec-sqrt=1", b"-ftz=0", b"-split-compile=1", b"-time"]
if args.max_registers:
    options.append(f"-maxrregcount={args.max_registers}".encode())
opts = (c.c_char_p * len(options))(*options)
start = time.perf_counter()
check(functions["Create"](c.byref(handle), len(options), opts))
try:
    paths = sorted(args.source.glob("unit-*.ptx"))
    for index, path in enumerate(paths):
        data = c.create_string_buffer(path.read_bytes())
        check(functions["AddData"](handle, 2, data, len(data), path.name.encode()))
        print(f"Added unit {index + 1}/{len(paths)}", flush=True)
    print("Compiling and linking independent units", flush=True)
    check(functions["Complete"](handle))
    size = c.c_size_t()
    check(functions["GetLinkedCubinSize"](handle, c.byref(size)))
    data = c.create_string_buffer(size.value)
    check(functions["GetLinkedCubin"](handle, data))
    args.out.write_bytes(data.raw)
    args.out.with_suffix(".info.log").write_text(log("Info"))
    record = dict(units=len(paths), seconds=time.perf_counter()-start, cubin_bytes=size.value, linker_version=api_version,
                  sha256=hashlib.sha256(data.raw).hexdigest(), options=[x.decode() for x in options])
    args.out.with_suffix(".build.json").write_text(json.dumps(record, indent=2))
    print(json.dumps(record), flush=True)
finally:
    functions["Destroy"](c.byref(handle))
