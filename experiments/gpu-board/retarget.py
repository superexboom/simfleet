#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Retarget pinned wasm2c output to CUDA without changing simulation bodies.

The CPU initializes WASM data and prepares the row; CUDA gets that memory
snapshot. This removes host initialization code, not game rules or gas checks.
"""
import argparse
import hashlib
import json
import pathlib
import re


def remove_function(source, name):
    pattern = re.compile(r"^[\w *]+\b" + re.escape(name) + r"\([^;]*?\)\s*\{", re.M)
    found = pattern.search(source)
    if not found:
        raise ValueError("missing function " + name)
    start = found.end()
    depth, end = 1, start
    while depth:
        if source[end] == "{": depth += 1
        if source[end] == "}": depth -= 1
        end += 1
    return source[:found.start()] + source[end:]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("source", type=pathlib.Path)
    p.add_argument("out", type=pathlib.Path)
    p.add_argument("--cow", action="store_true", help="share immutable pages and transpose private pages across each warp")
    p.add_argument("--gas-address", type=int, help="counter address proven by the CPU bridge probe")
    args = p.parse_args()
    if args.gas_address is not None and (not args.cow or args.gas_address % 8):
        p.error("gas cache requires COW and an aligned address")
    source = args.source.read_text()
    original = source
    # The data segment is already present in the shared CPU snapshot.
    source, segments = re.subn(r"static const u8 data_segment_data_\w+\[\] = \{.*?\n\};", "", source, flags=re.S)
    for name in ["init_globals", "init_memories", "init_data_instances", "init_tables", "init_elem_instances",
                 "wasm2c_sf_instantiate", "wasm2c_sf_free", "wasm2c_sf_get_func_type"]:
        source = remove_function(source, name)
    source = source[source.index("#define IS_SINGLE_UNSHARED_DEFAULT32_MEMORY"):]
    # All generated functions execute on the device. Keep large Wasm functions
    # out of line so compiler inlining cannot explode the complete engine.
    source = re.sub(r"(?m)^(static )?(u32|u64|f32|f64|void) ((?:wrap_)?w2c_sf_\w+)\(",
                    r"__device__ __noinline__ \1\2 \3(", source)
    source = re.sub(r"\bstatic inline\b", "static __device__ __forceinline__", source)
    source = re.sub(r"(?m)^static (float|double|bool) (\w+)\(", r"static __device__ \1 \2(", source)
    source = source.replace("W2C_COLD_FUNC static bool", "static __device__ bool")
    # Type IDs and function table entries are immutable device globals.
    source = source.replace("#define FUNC_TYPE_T(x) static const", "#define FUNC_TYPE_T(x) static __device__ const")
    source = source.replace("static const wasm_elem_segment_expr_t", "static __device__ const wasm_elem_segment_expr_t")
    source = source.replace("(wasm_rt_funcref_t){", "wasm_rt_funcref_t{")
    source = source.replace("wasm_rt_memory_t* w2c_sf_memory", "__device__ wasm_rt_memory_t* w2c_sf_memory")
    source = source.replace("u32* w2c_sf_0x5F_stack_pointer", "__device__ u32* w2c_sf_0x5F_stack_pointer")
    header = args.source.with_suffix(".h").read_text()
    instance = re.search(r"typedef struct w2c_sf \{.*?\} w2c_sf;", header, re.S).group()
    table = re.search(r"funcref_table_init\(&instance->w2c_T0, (\w+), (\d+), (\d+)u, 0, (\d+), instance\)", original)
    if not table:
        raise ValueError("unsupported table initialization")
    segment, entries, offset, count = table.groups()
    size = int(entries) + int(offset)
    init = re.search(r"static __device__ const wasm_elem_segment_expr_t " + segment + r"\[\] = \{(.*?)\n\};", source, re.S)
    initializers = [line.strip().removesuffix(",") for line in init[1].splitlines() if line.strip()]
    if len(initializers) != int(entries):
        raise ValueError("unexpected table initializer")
    source = source[:init.start()] + f"static __device__ wasm_elem_segment_expr_t {segment}[{entries}];" + source[init.end():]
    table_setup = "\n".join(f"{segment}[{i}] = wasm_elem_segment_expr_t{value};" for i, value in enumerate(initializers))
    kernel = f'''
extern "C" __global__ void sf_setup(u32* statuses) {{
    if (blockIdx.x || threadIdx.x) return;
    sf_status = statuses;
    {table_setup}
}}
extern "C" __global__ void sf_clone(const u8* snapshot, u8* memory, u64 bytes, u32 count) {{
    u64 i = blockIdx.x * (u64)blockDim.x + threadIdx.x;
    u64 total = bytes * count;
    for (; i < total; i += gridDim.x * (u64)blockDim.x) memory[i] = snapshot[i % bytes];
}}
extern "C" __global__ void sf_board(u8* memories, u64 stride, u64 size,
                                    u32 stack, u32 handle, u32 from, u32 count,
                                    u8* outputs, u32 output_stride, u32* lengths,
                                    wasm_rt_funcref_t* tables, u32* statuses) {{
    u32 i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= count) return;
    statuses[i] = 1;
    w2c_sf instance;
    instance.w2c_0x5F_stack_pointer = stack;
    instance.w2c_memory = {{ memories + i * stride, memories + (i+1) * stride, 65536, size/65536, stride/65536, size, false }};
    instance.w2c_T0 = {{ tables + i * {size}, {size}, {size} }};
    instance.w2c_T0.data[0] = wasm_rt_funcref_null_value;
    funcref_table_init(&instance.w2c_T0, {segment}, {entries}, {offset}, 0, {count}, &instance);
    u64 answer = w2c_sf_sf_run(&instance, handle, from + i);
    u32 ptr = (u32)answer, len = (u32)(answer >> 32);
    if (len > output_stride || (u64)ptr + len > instance.w2c_memory.size) {{ statuses[i] = 2; return; }}
    memcpy(outputs + (u64)i * output_stride, instance.w2c_memory.data + ptr, len);
    lengths[i] = len;
    statuses[i] = 0;
}}
'''
    if args.cow:
        source, reads = re.subn(r"wasm_rt_memcpy\(&result, MEM_ADDR_MEMOP\(mem, addr, sizeof\(t1\)\),\s*\\\s*sizeof\(t1\)\);",
                               "result = sf_load<t1>(mem, addr);", source)
        source, writes = re.subn(r"wasm_rt_memcpy\(MEM_ADDR_MEMOP\(mem, addr, sizeof\(t1\)\), &wrapped,\s*\\\s*sizeof\(t1\)\);",
                                "sf_store<t1>(mem, addr, wrapped);", source)
        if reads != 1 or writes != 1:
            raise ValueError(f"unsupported memory helper shape {reads}/{writes}")
        source = source.replace("memset(MEM_ADDR(mem, d, n), val, n);", "sf_fill(mem, d, val, n);")
        source = source.replace("memmove(MEM_ADDR(dest, dest_addr, n), MEM_ADDR(src, src_addr, n), n);", "sf_copy(dest, src, dest_addr, src_addr, n);")
        # A snapshot's tables are immutable. Rebind indirect calls to this
        # instance while sharing code pointers and signature IDs across lanes.
        source = re.sub(r"instance->w2c_T0\.data\[[^\]]+\]\.module_instance", "instance", source)
        source += f"\nstatic __device__ wasm_rt_funcref_t sf_shared_table[{size}];\n"
        kernel = f'''
extern "C" __global__ void sf_setup(u32* statuses) {{
    if (blockIdx.x || threadIdx.x) return;
    sf_status = statuses;
    {table_setup}
    wasm_rt_funcref_table_t table = {{sf_shared_table, {size}, {size}}};
    sf_shared_table[0] = wasm_rt_funcref_null_value;
    funcref_table_init(&table, {segment}, {entries}, {offset}, 0, {count}, NULL);
}}
extern "C" __global__ void sf_board(u8* snapshot, u8* private_pages, u32* page_map,
                                    u64 capacity, u64 snapshot_bytes, u32 private_capacity,
                                    u32 virtual_pages, u32 stack, u32 handle,
                                    u32 from, u32 count, u8* outputs, u32 output_stride,
                                    u32* lengths, u32* pages_used, u32* statuses) {{
    u32 i = blockIdx.x * blockDim.x + threadIdx.x;
    if (i >= count) return;
    statuses[i] = 1;
    w2c_sf instance;
    instance.w2c_0x5F_stack_pointer = stack;
    instance.w2c_memory = {{snapshot, snapshot + snapshot_bytes, 65536,
        snapshot_bytes/65536, capacity/65536, snapshot_bytes, false,
        private_pages + (u64)(i/32)*private_capacity*4096*32,
        page_map + (u64)(i/32)*virtual_pages*32, private_capacity, 0, i%32, snapshot_bytes}};
    instance.w2c_T0 = {{sf_shared_table, {size}, {size}}};
    u64 answer = w2c_sf_sf_run(&instance, handle, from + i);
    u32 ptr = (u32)answer, len = (u32)(answer >> 32);
    if (len > output_stride || (u64)ptr + len > instance.w2c_memory.size) {{ statuses[i] = 2; return; }}
    for (u32 k = 0; k < len; ++k) outputs[(u64)i*output_stride+k] = sf_read_byte(&instance.w2c_memory, (u64)ptr+k);
    pages_used[i] = instance.w2c_memory.used_pages;
    lengths[i] = len;
    statuses[i] = 0;
}}
'''
    runtime = pathlib.Path(__file__).with_name("runtime.cuh").read_text()
    defines = "#define SF_COW 1\n" if args.cow else ""
    if args.gas_address is not None:
        defines += f"#define SF_GAS_ADDRESS {args.gas_address}ULL\n"
        source = source.replace("result = sf_load<t1>(mem, addr);", "if (sizeof(t1) == 8 && addr == SF_GAS_ADDRESS) memcpy(&result, &mem->cached_gas, 8); else result = sf_load<t1>(mem, addr);")
        source = source.replace("sf_store<t1>(mem, addr, wrapped);", "if (sizeof(t1) == 8 && addr == SF_GAS_ADDRESS) memcpy(&mem->cached_gas, &wrapped, 8); else sf_store<t1>(mem, addr, wrapped);")
        kernel = kernel.replace("u64 answer = w2c_sf_sf_run", "instance.w2c_memory.cached_gas = *(const u64*)(snapshot + SF_GAS_ADDRESS);\n    u64 answer = w2c_sf_sf_run")
    result = defines + runtime + "\n" + instance + "\n" + source + "\n" + kernel
    args.out.write_text(result, newline="\n")
    meta = dict(source_sha256=hashlib.sha256(args.source.read_bytes()).hexdigest(),
                cuda_sha256=hashlib.sha256(result.encode()).hexdigest(), table_entries=size,
                removed_data_segments=segments, source_bytes=len(original), cuda_bytes=len(result), cow=args.cow, gas_address=args.gas_address)
    args.out.with_suffix(".json").write_text(json.dumps(meta, indent=2))
    print(json.dumps(meta))


if __name__ == "__main__": main()
