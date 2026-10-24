#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Differential CUDA memory test: unaligned, overlapping, COW and gas accesses."""
import argparse
import ctypes as c
import json
import pathlib
import random
import sys
from memory_budget import limit

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "hardware"))
from cuda_driver import Cuda


class Op(c.Structure):
    _fields_ = [("kind", c.c_uint), ("dst", c.c_uint), ("src", c.c_uint), ("length", c.c_uint), ("value", c.c_uint64)]


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--nvrtc-dir", type=pathlib.Path, required=True)
    p.add_argument("--out", type=pathlib.Path, required=True)
    p.add_argument("--cow-page-bytes", type=int, choices=[1024, 4096], default=4096)
    p.add_argument("--sparse-page-map",action="store_true")
    args = p.parse_args(); limit()
    cases, steps, size = 64, 1024, 131072
    page_bytes = args.cow_page_bytes
    page_count = size // page_bytes
    map_entries=size//65536 if args.sparse_page_map else page_count
    arena_pages=page_count+size//65536 if args.sparse_page_map else page_count
    if args.sparse_page_map and page_bytes!=1024:p.error("sparse maps require 1024-byte pages")
    index_bytes = 2 if page_bytes == 1024 else 4
    rng = random.Random(789123)
    image = bytes(rng.randrange(256) for _ in range(65536))
    operations = (Op * (cases*steps))(); answers = []
    for lane in range(cases):
        memory = bytearray(image + bytes(65536))
        for index in range(steps):
            kind = index % 3
            n = rng.choice([1,2,4,8]) if kind == 0 else rng.randrange(129)
            at = rng.choice([rng.randrange(size-256), 57, 60, 63, 64, 65, 68, 71, page_bytes-3, page_bytes-1, 65530])
            src = max(0, at + rng.randrange(-16,17))
            value = rng.getrandbits(64)
            operations[lane*steps+index] = Op(kind, at, src, n, value)
            if kind == 0: memory[at:at+n] = value.to_bytes(8,"little")[:n]
            elif kind == 1: memory[at:at+n] = bytes([value & 255])*n
            else: memory[at:at+n] = memory[src:src+n]
        answers.append(bytes(memory))
    source = f"#define SF_COW 1\n#define SF_GAS_ADDRESS 64ULL\n#define SF_COW_PAGE_SHIFT {page_bytes.bit_length()-1}\n" + ("#define SF_COW_SPARSE_MAP 1\n" if args.sparse_page_map else "") + pathlib.Path(__file__).with_name("runtime.cuh").read_text() + r'''
struct Op {u32 kind,dst,src,length;u64 value;};
extern "C" __global__ void init(u32* status) {if (!blockIdx.x && !threadIdx.x) sf_status=status;}
extern "C" __global__ void check(const u8* image, const Op* ops, u8* private_pages,
    sf_page_index_t* page_map, u8* out, u32* status, u32 capacity) {
    u32 i=blockIdx.x*blockDim.x+threadIdx.x;
    if(i>=64) return;
    status[i]=1;
    // Cached i64/f64 accesses must preserve every bit, including NaN payloads.
    u64 gas_bits = i & 1 ? 0x7ff8000000001234ULL : 0xfedcba9876543210ULL;
    if (sf_gas_value<u64>(gas_bits) != gas_bits || sf_gas_bits(sf_gas_value<f64>(gas_bits)) != gas_bits) {
        status[i]=3; return;
    }
    wasm_rt_memory_t mem={(u8*)image,(u8*)image+65536,65536,1,2,65536,false,
        private_pages+(u64)(i/32)*capacity*SF_COW_PAGE_BYTES*32,
        page_map+(u64)(i/32)*(131072/(1u<<SF_COW_MAP_SHIFT))*32,capacity,0,i%32,65536,*(const u64*)(image+64)};
    if(wasm_rt_grow_memory(&mem,1)!=1 || wasm_rt_grow_memory(&mem,1)!=~0ULL) {status[i]=2;return;}
    for(u32 k=0;k<1024;++k){
        Op x=ops[i*1024+k];
        if(x.kind==0) {
            if(x.length==8) sf_store(&mem,x.dst,x.value);
            else if(x.length==4) sf_store(&mem,x.dst,(u32)x.value);
            else if(x.length==2) sf_store(&mem,x.dst,(u16)x.value);
            else sf_store(&mem,x.dst,(u8)x.value);
        } else if(x.kind==1) sf_fill(&mem,x.dst,x.value,x.length);
        else sf_copy(&mem,&mem,x.dst,x.src,x.length);
    }
    for(u32 k=0;k<131072;++k) out[(u64)i*131072+k]=sf_read_byte(&mem,k);
    status[i]=0;
}
'''
    args.out.parent.mkdir(parents=True,exist_ok=True)
    cu = args.out.with_suffix(".cu"); cu.write_text(source,newline="\n")
    with Cuda(args.nvrtc_dir) as gpu:
        gpu.compile(cu)
        di=gpu.allocate(len(image)); gpu.upload(di,(c.c_ubyte*len(image)).from_buffer_copy(image))
        dop=gpu.allocate(c.sizeof(operations)); gpu.upload(dop,operations)
        private=gpu.allocate(cases*arena_pages*page_bytes); maps=gpu.allocate(cases*map_entries*index_bytes)
        out=gpu.allocate(cases*size); status=gpu.allocate(cases*4)
        gpu.driver.cuMemsetD8_v2.argtypes=[c.c_uint64,c.c_ubyte,c.c_size_t]
        gpu.launch("init",1,[status])
        checks=[]
        for block,capacity in [(8,arena_pages),(32,arena_pages),(32,4)]:
            gpu.check(gpu.driver.cuMemsetD8_v2(maps,0,cases*map_entries*index_bytes))
            gpu.launch("check",cases,[di,dop,private,maps,out,status,c.c_uint(capacity)],block_size=block)
            actual_status=(c.c_uint*cases)();gpu.download(actual_status,status)
            if capacity==4:
                assert all(x==120 for x in actual_status),list(actual_status)
                checks.append(dict(block=block,capacity=capacity,overflow_rejected=True));continue
            assert not any(actual_status),list(actual_status)
            result=(c.c_ubyte*(cases*size))();gpu.download(result,out)
            view=memoryview(result).cast("B")
            for lane,expected in enumerate(answers):
                assert bytes(view[lane*size:(lane+1)*size])==expected,(block,lane)
            checks.append(dict(block=block,capacity=capacity,all_bytes_equal=True))
    record=dict(cases=cases,operations_per_case=steps,cow_page_bytes=page_bytes,page_index_bytes=index_bytes,sparse_page_map=args.sparse_page_map,checks=checks)
    args.out.write_text(json.dumps(record,indent=2)+"\n");print(json.dumps(record))


if __name__=="__main__":main()
