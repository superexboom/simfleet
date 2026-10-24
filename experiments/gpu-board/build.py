#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Build a complete NVIDIA board backend in an isolated folder.

Source is an upstream worktree prepared with simfleet's native patches. The
native reference must match that source. Never changes the supplied checkout.
"""
import argparse
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tomllib
from memory_budget import limit


def sha(path): return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--source", type=pathlib.Path, required=True)
    p.add_argument("--native", type=pathlib.Path, required=True)
    p.add_argument("--python-deps", type=pathlib.Path, required=True)
    p.add_argument("--wasm2c", type=pathlib.Path, required=True)
    p.add_argument("--nvrtc-dir", type=pathlib.Path, required=True)
    p.add_argument("--link-library", type=pathlib.Path, required=True)
    p.add_argument("--orders", type=pathlib.Path)
    p.add_argument("--sample-row", type=int, default=9)
    p.add_argument("--out", type=pathlib.Path, required=True)
    p.add_argument("--target-dir",type=pathlib.Path,help="reuse an exclusively owned Cargo build cache; output binaries are copied into out")
    p.add_argument("--offline", action="store_true")
    p.add_argument("--architecture", choices=["75", "89", "120"], default="120")
    p.add_argument("--max-registers", type=int, help="optional register cap for the CUDA linker")
    p.add_argument("--word-gas", action="store_true", help="experimental typed cached-gas access path")
    p.add_argument("--revision", type=int, choices=[1, 2, 3], default=1)
    p.add_argument("--cow-page-bytes",type=int,choices=[1024,4096],default=4096)
    p.add_argument("--sparse-page-map",action="store_true")
    p.add_argument("--block", type=int, choices=[8, 16, 32, 64, 128, 256], default=32)
    p.add_argument("--reuse-from", type=pathlib.Path, help="reuse matching bridge/wire binaries for another GPU architecture")
    p.add_argument("--skip-device-validation", action="store_true", help="build-only package for a GPU not installed here; explicitly unverified")
    p.add_argument("--check", action="store_true", help="validate configuration and generated Cargo manifests")
    args = p.parse_args()
    if (args.revision==3)!=(args.cow_page_bytes==1024):p.error("sfg3 requires 1024-byte pages; sfg1/2 require 4096")
    if args.sparse_page_map and args.revision!=3:p.error("sparse maps require sfg3")
    here = pathlib.Path(__file__).resolve().parent; repo = here.parents[1]
    source, out = args.source.resolve(), args.out.resolve()
    orders = (args.orders or repo / "experiments/prepared-board/orders.json").resolve()
    for file in [source/"webapi/src/simfleet.rs", source/"mise.toml", args.native, args.wasm2c,
                 args.nvrtc_dir/"nvrtc64_120_0.dll", args.link_library, orders]:
        if not file.is_file(): p.error(f"missing required file: {file}")
    if out.exists(): p.error("out must be a new dedicated directory")
    limit(); out.mkdir(parents=True)
    (out/"logs").mkdir(); (out/"generated").mkdir()
    rust = tomllib.loads((source/"mise.toml").read_text())["tools"]["rust"]
    manifests = {}
    for kind, filename in [("wire","wire.rs"),("bridge-binary","bridge.rs")]:
        crate = out/kind; crate.mkdir()
        text = f'''[package]
name="simfleet-gpu-{kind}"
version="0.1.0"
edition="2021"
[lib]
crate-type=["cdylib"]
path={json.dumps((here/filename).as_posix())}
[features]
binary=[]
[dependencies]
wfsim-engine={{path={json.dumps((source/'engine').as_posix())}}}
wfsim-webapi={{path={json.dumps((source/'webapi').as_posix())}}}
serde="1"
serde_json={{version="1",features=["float_roundtrip"]}}
postcard={{version="=1.1.3",default-features=false,features=["alloc"]}}
anyhow="1"
[profile.release]
opt-level=3
lto="thin"
codegen-units=1
[workspace]
'''
        manifests[kind] = crate/"Cargo.toml"; manifests[kind].write_text(text)
        shutil.copyfile(repo/"Cargo.lock",crate/"Cargo.lock")
    target=(args.target_dir or out/"target").resolve()
    env=os.environ.copy();env["CARGO_TARGET_DIR"]=str(target)
    env.pop("CARGO_ENCODED_RUSTFLAGS",None)
    if args.offline:env["CARGO_NET_OFFLINE"]="true"
    def execute(stage, command):
        print(stage,flush=True)
        with (out/"logs"/(stage+".log")).open("w",encoding="utf8") as log:
            subprocess.run([str(x) for x in command],env=env,stdout=log,stderr=subprocess.STDOUT,check=True)
    if args.check:
        for kind,manifest in manifests.items():
            execute("metadata-"+kind,["cargo","+"+rust,"metadata","--no-deps","--offline","--format-version","1","--manifest-path",manifest])
        print("Configuration and Cargo manifests verified");return
    if args.reuse_from:
        reuse=args.reuse_from.resolve(); original=json.loads((reuse/"engine.gpu.json").read_text())
        wasm=reuse/"bridge.wasm"; wire=reuse/"wire.dll"
        for file in (wasm,wire):
            if sha(file)!=original["files"][file.name]: raise ValueError("reused bridge/wire checksum mismatch")
        import ctypes
        native=ctypes.CDLL(str(args.native.resolve()));native.sfe_engine_id.restype=ctypes.c_char_p
        if native.sfe_engine_id().decode()!=original["engine"]: raise ValueError("reused bridge belongs to a different engine")
    else:
        execute("wasm-target",["rustup","target","add","wasm32-unknown-unknown","--toolchain",rust])
        env["RUSTFLAGS"]="-C target-cpu=x86-64-v3 -C target-feature=+crt-static"
        execute("native-wire",["cargo","+"+rust,"build","--release","-j","4","--manifest-path",manifests["wire"]])
        env["RUSTFLAGS"]="-C target-feature=-simd128 -C link-arg=--export=__stack_pointer"
        execute("wasm-bridge",["cargo","+"+rust,"build","--release","-j","4","--target","wasm32-unknown-unknown","--features","binary","--manifest-path",manifests["bridge-binary"]])
        wasm=target/"wasm32-unknown-unknown/release/simfleet_gpu_bridge_binary.wasm"
        wire=target/"release/simfleet_gpu_wire.dll"
    execute("cpu-proof",[sys.executable,here/"prepare.py","--python-deps",args.python_deps.resolve(),"--wasm",wasm,
        "--native",args.native.resolve(),"--wire-library",wire,"--orders",orders,"--row",args.sample_row,"--runs","64","--out",out/"proof"])
    proof=json.loads((out/"proof/snapshot.json").read_text())
    generated=out/"generated"
    execute("wasm2c",[args.wasm2c.resolve(),wasm,"--no-debug-names","-n","sf","-o",generated/"sf.c"])
    execute("retarget",[sys.executable,here/"retarget.py",generated/"sf.c",generated/"sf.cu","--cow","--gas-address",proof["gas_address"],"--cow-page-bytes",args.cow_page_bytes,*(["--word-gas"] if args.word_gas else []),*(["--sparse-page-map"] if args.sparse_page_map else [])])
    execute("cuda",[sys.executable,here/"compile_cuda.py",generated/"sf.cu",generated/"sf.ptx","--nvrtc-dir",args.nvrtc_dir.resolve(),"--architecture","compute_"+args.architecture])
    execute("split",[sys.executable,here/"split_ptx.py",generated/"sf.ptx",out/"units"])
    execute("packed-entry",[sys.executable,here/"packed_kernel.py","--cuda",generated/"sf.cu","--ptx",generated/"sf.ptx","--out",generated/"packed.cu"])
    execute("packed-cuda",[sys.executable,here/"compile_cuda.py",generated/"packed.cu",out/"units/unit-packed.ptx","--nvrtc-dir",args.nvrtc_dir.resolve(),"--architecture","compute_"+args.architecture])
    module=out/"engine.cubin"
    link=[sys.executable,here/"link_ptx.py",out/"units",module,"--library",args.link_library.resolve(),"--architecture","sm_"+args.architecture]
    if args.max_registers:link += ["--max-registers",args.max_registers]
    execute("link",link)
    info={key:proof[key] for key in ["engine","wasm_sha256","wire_format","gas_address"]}
    info.update(schema=1,revision=args.revision,block_threads=args.block,host="windows-x64-v3",module_sha256=sha(module),architecture="compute_"+args.architecture,entrypoints=["sf_setup","sf_board","sf_board_packed"],wire_sha256=sha(wire),device_validated=False)
    info.update(cow_page_bytes=args.cow_page_bytes,page_index_bytes=2 if args.cow_page_bytes==1024 else 4)
    info["page_map_region_bytes"]=65536 if args.sparse_page_map else args.cow_page_bytes
    module.with_suffix(".gpu.json").write_text(json.dumps(info,indent=2)+"\n")
    if not args.skip_device_validation:
        execute("gpu-proof",[sys.executable,here/"packed.py","--module",module,"--snapshot",out/"proof","--native",args.native.resolve(),
            "--wire-library",wire,"--copies","2","--private-kib","512","--block",args.block,"--repeats","1","--out",out/"validation.json"])
        info["device_validated"]=True
    shutil.copyfile(wasm,out/"bridge.wasm");shutil.copyfile(wire,out/"wire.dll")
    info["files"]={name:sha(out/name) for name in ["engine.cubin","bridge.wasm","wire.dll"]}
    module.with_suffix(".gpu.json").write_text(json.dumps(info,indent=2)+"\n")
    (out/"provenance.json").write_text(json.dumps(dict(rust=rust,source=str(source),native_sha256=sha(args.native),
        source_files={str(f.relative_to(repo)):sha(f) for f in here.glob("*") if f.is_file()},gpu=info),indent=2)+"\n")
    print(f"Built GPU backend (device validated: {not args.skip_device_validation}): {out}",flush=True)


if __name__=="__main__":main()
