#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
import argparse
import hashlib
import json
import pathlib
import sys
import time
from memory_budget import limit

limit()

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "hardware"))
from cuda_driver import Cuda

p = argparse.ArgumentParser()
p.add_argument("source", type=pathlib.Path)
p.add_argument("out", type=pathlib.Path)
p.add_argument("--nvrtc-dir", type=pathlib.Path, required=True)
p.add_argument("--architecture", choices=["compute_75", "compute_89", "compute_120"])
args = p.parse_args()
start = time.perf_counter()
print("Compiling the complete CUDA board module", flush=True)
with Cuda(args.nvrtc_dir) as gpu:
    ptx = gpu.compile(args.source, extra_options=("--device-as-default-execution-space", "--disable-warnings"), load=False, architecture=args.architecture)
    args.out.write_bytes(ptx)
    record = dict(seconds=time.perf_counter()-start, ptx_bytes=len(ptx), options=gpu.options,
                  source_sha256=hashlib.sha256(args.source.read_bytes()).hexdigest(), ptx_sha256=hashlib.sha256(ptx).hexdigest())
    args.out.with_suffix(".build.json").write_text(json.dumps(record, indent=2))
    print(json.dumps(record), flush=True)
