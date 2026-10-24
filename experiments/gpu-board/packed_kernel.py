#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Build a small batch entry point around the already compiled battle functions."""
import argparse
import pathlib
import re

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("--cuda", type=pathlib.Path, required=True)
p.add_argument("--ptx", type=pathlib.Path, required=True)
p.add_argument("--out", type=pathlib.Path, required=True)
args = p.parse_args()
cuda, ptx = args.cuda.read_text(), args.ptx.read_text()
instance = re.search(r"typedef struct w2c_sf \{.*?\} w2c_sf;", cuda, re.S)[0]
gas = re.search(r"#define SF_GAS_ADDRESS (\d+)ULL", cuda)[1]
page_shift = re.search(r"#define SF_COW_PAGE_SHIFT (\d+)", cuda)
page_shift = page_shift[1] if page_shift else "12"
if page_shift not in ("10", "12"): raise ValueError("unsupported COW layout")
sparse = "#define SF_COW_SPARSE_MAP 1\n" if "#define SF_COW_SPARSE_MAP 1" in cuda else ""
table = re.search(r"\.global \.align 8 \.b8 (\w*sf_shared_table)\[(\d+)\]", ptx)
entry = ptx[ptx.index(".visible .entry sf_board("):]
functions = sorted(set(re.findall(r"[A-Za-z_][A-Za-z0-9_]*sf_run[A-Za-z0-9_]*", entry)))
if len(functions) != 1: raise ValueError(functions)
runtime = pathlib.Path(__file__).with_name("runtime.cuh").read_text().replace("__device__ u32 *sf_status;", "extern __device__ u32 *sf_status;")
kernel = f'''
extern "C" __device__ u64 {functions[0]}(w2c_sf*, u32, u32);
extern __device__ wasm_rt_funcref_t {table[1]}[{int(table[2]) // 32}];
struct SFJob {{ u8* snapshot; u64 bytes; u32 handle, stack, runs, reserved; }};
extern "C" __global__ void sf_board_packed(const SFJob* jobs, const u32* owners, const u32* indices,
    u8* private_pages, sf_page_index_t* page_map, u64 capacity, u32 private_capacity,
    u32 virtual_pages, u32 count, u8* outputs, u32 stride,
    u32* lengths, u32* pages_used, u32* statuses) {{
    u32 i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= count) return;
    statuses[i] = 1;
    const SFJob& job = jobs[owners[i]];
    if (indices[i] >= job.runs) {{ statuses[i] = 3; return; }}
    w2c_sf instance;
    instance.w2c_0x5F_stack_pointer = job.stack;
    instance.w2c_memory = {{job.snapshot, job.snapshot + job.bytes, 65536,
        job.bytes/65536, capacity/65536, job.bytes, false,
        private_pages + (u64)(i/32)*private_capacity*SF_COW_PAGE_BYTES*32,
        page_map + (u64)(i/32)*virtual_pages*32, private_capacity, 0, i%32, job.bytes}};
    instance.w2c_memory.cached_gas = *(const u64*)(job.snapshot + SF_GAS_ADDRESS);
    instance.w2c_T0 = {{{table[1]}, {int(table[2]) // 32}, {int(table[2]) // 32}}};
    u64 answer = {functions[0]}(&instance, job.handle, indices[i]);
    u32 ptr = (u32)answer, len = (u32)(answer >> 32);
    if (len > stride || (u64)ptr + len > instance.w2c_memory.size) {{ statuses[i] = 2; return; }}
    for (u32 k = 0; k < len; ++k) outputs[(u64)i*stride+k] = sf_read_byte(&instance.w2c_memory, (u64)ptr+k);
    pages_used[i] = instance.w2c_memory.used_pages;
    lengths[i] = len; statuses[i] = 0;
}}
'''
args.out.write_text(f"#define SF_COW 1\n#define SF_COW_PAGE_SHIFT {page_shift}\n#define SF_GAS_ADDRESS {gas}ULL\n" + sparse + runtime + "\n" + instance + "\n" + kernel, newline="\n")
