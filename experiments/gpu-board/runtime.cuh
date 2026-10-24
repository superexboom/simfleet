// SPDX-License-Identifier: AGPL-3.0-or-later
// CUDA implementation of the small wasm2c runtime surface used by the bridge.
typedef unsigned char uint8_t;
typedef signed char int8_t;
typedef unsigned short uint16_t;
typedef short int16_t;
typedef unsigned int uint32_t;
typedef int int32_t;
typedef unsigned long long uint64_t;
typedef long long int64_t;
typedef unsigned long long uintptr_t;
typedef uint8_t u8; typedef int8_t s8; typedef uint16_t u16; typedef int16_t s16;
typedef uint32_t u32; typedef int32_t s32; typedef uint64_t u64; typedef int64_t s64;
typedef float f32; typedef double f64;
#ifndef SF_COW_PAGE_SHIFT
#define SF_COW_PAGE_SHIFT 12
#endif
#define SF_COW_PAGE_BYTES (1u << SF_COW_PAGE_SHIFT)
#define SF_COW_PAGE_MASK (SF_COW_PAGE_BYTES - 1)
#ifdef SF_COW_SPARSE_MAP
#define SF_COW_MAP_SHIFT 16
#else
#define SF_COW_MAP_SHIFT SF_COW_PAGE_SHIFT
#endif
#if SF_COW_PAGE_SHIFT == 10
typedef u16 sf_page_index_t;
#elif SF_COW_PAGE_SHIFT == 12
typedef u32 sf_page_index_t;
#else
#error Unsupported copy-on-write page layout
#endif
#define WASM_RT_CORE_TYPES_DEFINED
#define WABT_BIG_ENDIAN 0
#define WASM_RT_USE_SEGUE 0
#define WASM_RT_USE_MMAP 0
#define WASM_RT_MEMCHECK_BOUNDS_CHECK 1
#define WASM_RT_MEMCHECK_GUARD_PAGES 0
#define WASM_RT_STACK_DEPTH_COUNT 0
#define WASM_RT_CHECK_BASE(x)
#define LIKELY(x) (x)
#define UNLIKELY(x) (x)
#define UNLIKELY_WITH_UNPREDICTABLE(x) (x)
#define WASM_RT_UNLIKELY(x) (x)
#define INT32_MIN (-2147483647-1)
#define INT32_MAX 2147483647
#define UINT32_MAX 4294967295U
#define INT64_MIN (-9223372036854775807LL-1)
#define INT64_MAX 9223372036854775807LL
#define UINT64_MAX 18446744073709551615ULL
#define INFINITY __int_as_float(0x7f800000)
#define NAN __int_as_float(0x7fc00000)
#ifndef NULL
#define NULL 0
#endif
#ifndef __has_builtin
#define __has_builtin(x) 0
#endif
#define __builtin_clz __clz
#define __builtin_clzll __clzll
#define __builtin_ctz(x) (__ffs(x)-1)
#define __builtin_ctzll(x) (__ffsll(x)-1)
#define __builtin_popcount __popc
#define __builtin_popcountll __popcll
#define wasm_rt_memcpy memcpy
#define static_assert(x) static_assert(x, "wasm2c assertion")

enum { WASM_RT_TRAP_NONE, WASM_RT_TRAP_OOB, WASM_RT_TRAP_INT_OVERFLOW,
       WASM_RT_TRAP_DIV_BY_ZERO, WASM_RT_TRAP_INVALID_CONVERSION,
       WASM_RT_TRAP_UNREACHABLE, WASM_RT_TRAP_CALL_INDIRECT,
       WASM_RT_TRAP_NULL_REF, WASM_RT_TRAP_UNCAUGHT_EXCEPTION,
       WASM_RT_TRAP_UNALIGNED, WASM_RT_TRAP_EXHAUSTION };
typedef const char* wasm_rt_func_type_t;
typedef void (*wasm_rt_function_ptr_t)(void);
struct wasm_rt_tailcallee_t { void (*fn)(void**, void*, wasm_rt_tailcallee_t*); };
struct wasm_rt_funcref_t {
    wasm_rt_func_type_t func_type;
    wasm_rt_function_ptr_t func;
    wasm_rt_tailcallee_t func_tailcallee;
    void *module_instance;
};
typedef void* wasm_rt_externref_t;
struct wasm_rt_memory_t {
    u8 *data, *data_end;
    u32 page_size;
    u64 pages, max_pages, size;
    bool is64;
#ifdef SF_COW
    u8* private_pages;
    sf_page_index_t* page_map;
    u32 private_capacity, used_pages, lane;
    u64 snapshot_bytes;
#ifdef SF_GAS_ADDRESS
    u64 cached_gas;
#endif
#endif
};
struct wasm_rt_funcref_table_t { wasm_rt_funcref_t *data; u32 max_size, size; };
struct wasm_rt_externref_table_t { wasm_rt_externref_t *data; u32 max_size, size; };
#define wasm_rt_funcref_null_value (wasm_rt_funcref_t{NULL,NULL,{NULL},NULL})
#define wasm_rt_externref_null_value ((wasm_rt_externref_t)NULL)
__device__ int sf_memcmp(const void* a, const void* b, size_t n) {
    const u8 *x = (const u8*)a, *y = (const u8*)b;
    for (size_t i = 0; i < n; ++i) if (x[i] != y[i]) return (int)x[i] - (int)y[i];
    return 0;
}
__device__ void* sf_memmove(void* a, const void* b, size_t n) {
    u8 *x = (u8*)a; const u8 *y = (const u8*)b;
    if (x < y) { for (size_t i = 0; i < n; ++i) x[i] = y[i]; }
    else if (x > y) { while (n) { --n; x[n] = y[n]; } }
    return a;
}
#define memcmp sf_memcmp
#define memmove sf_memmove
__device__ u32 *sf_status;
__device__ void wasm_rt_trap(int code) {
    sf_status[blockIdx.x * blockDim.x + threadIdx.x] = 100 + code;
    // End only this lane; the host rejects every unfinished output.
    asm volatile("exit;");
}
#ifdef SF_COW
// Preserve the bits of cached 64-bit WASM loads/stores without byte-wise
// memcpy lowering. Only the floating-point overload reinterprets the bits.
template<class T> __device__ __forceinline__ T sf_gas_value(u64 bits) { return (T)bits; }
template<> __device__ __forceinline__ f64 sf_gas_value<f64>(u64 bits) { return __longlong_as_double(bits); }
template<class T> __device__ __forceinline__ u64 sf_gas_bits(T value) { return (u64)value; }
template<> __device__ __forceinline__ u64 sf_gas_bits<f64>(f64 value) { return __double_as_longlong(value); }
// Each warp stores the same 8-byte word of its 32 instances consecutively.
// Immutable pages remain shared; only written pages get private storage.
__device__ __forceinline__ u8* sf_private(wasm_rt_memory_t* m, u32 slot, u32 offset) {
    return m->private_pages + (((u64)slot * (SF_COW_PAGE_BYTES / 8) + offset / 8) * 32 + m->lane) * 8 + offset % 8;
}
__device__ __forceinline__ u32 sf_mapped(wasm_rt_memory_t* m, u32 page) {
#ifdef SF_COW_SPARSE_MAP
    u32 directory = m->page_map[(u64)(page >> 6) * 32 + m->lane];
    return directory ? *(sf_page_index_t*)sf_private(m,directory-1,(page & 63)*sizeof(sf_page_index_t)) : 0;
#else
    return m->page_map[(u64)page * 32 + m->lane];
#endif
}
__device__ __noinline__ u32 sf_make_private(wasm_rt_memory_t* m, u32 page) {
#ifdef SF_COW_SPARSE_MAP
    sf_page_index_t& directory=m->page_map[(u64)(page >> 6)*32+m->lane];
    if (!directory) {
        if (m->used_pages>=m->private_capacity || m->used_pages >= (sf_page_index_t)~0u) wasm_rt_trap(20);
        u32 slot=m->used_pages++;
        // The 64-entry leaf lives in the same bounded private arena. Untouched
        // 64-KiB regions require only a zero directory entry, no leaf storage.
        for (u32 off=0;off<64*sizeof(sf_page_index_t);off+=8) *(u64*)sf_private(m,slot,off)=0;
        directory=slot+1;
    }
    sf_page_index_t& mapped=*(sf_page_index_t*)sf_private(m,directory-1,(page & 63)*sizeof(sf_page_index_t));
#else
    sf_page_index_t& mapped = m->page_map[(u64)page * 32 + m->lane];
#endif
    if (mapped) return mapped - 1;
    if (m->used_pages >= m->private_capacity || m->used_pages >= (sf_page_index_t)~0u) wasm_rt_trap(20);
    u32 slot = m->used_pages++;
    u64 base = (u64)page * SF_COW_PAGE_BYTES;
    for (u32 off = 0; off < SF_COW_PAGE_BYTES; off += 8) {
        u64 value = base + off < m->snapshot_bytes ? *(const u64*)(m->data + base + off) : 0;
        *(u64*)sf_private(m, slot, off) = value;
    }
    mapped = slot + 1;
    return slot;
}
__device__ __forceinline__ u8 sf_read_byte(wasm_rt_memory_t* m, u64 address) {
#ifdef SF_GAS_ADDRESS
    if (address >= SF_GAS_ADDRESS && address < SF_GAS_ADDRESS + 8)
        return (u8)(m->cached_gas >> ((address - SF_GAS_ADDRESS) * 8));
#endif
    u32 page = address >> SF_COW_PAGE_SHIFT, offset = address & SF_COW_PAGE_MASK;
    u32 mapped = sf_mapped(m,page);
    return mapped ? *sf_private(m, mapped - 1, offset) : address < m->snapshot_bytes ? m->data[address] : 0;
}
__device__ __forceinline__ void sf_write_byte(wasm_rt_memory_t* m, u64 address, u8 value) {
#ifdef SF_GAS_ADDRESS
    if (address >= SF_GAS_ADDRESS && address < SF_GAS_ADDRESS + 8) {
        u32 shift = (address - SF_GAS_ADDRESS) * 8;
        m->cached_gas = (m->cached_gas & ~(255ULL << shift)) | ((u64)value << shift);
        return;
    }
#endif
    u32 page = address >> SF_COW_PAGE_SHIFT, offset = address & SF_COW_PAGE_MASK;
    u32 slot = sf_mapped(m,page);
    if (!slot) slot = sf_make_private(m, page) + 1;
    *sf_private(m, slot - 1, offset) = value;
}
__device__ __forceinline__ void sf_read(wasm_rt_memory_t* m, u64 address, void* out, u32 n) {
#ifdef SF_GAS_ADDRESS
    if (address >= SF_GAS_ADDRESS && address + n <= SF_GAS_ADDRESS + 8) {
        u64 value = m->cached_gas >> ((address - SF_GAS_ADDRESS) * 8);
        memcpy(out, &value, n); return;
    }
#endif
    u32 page = address >> SF_COW_PAGE_SHIFT, offset = address & SF_COW_PAGE_MASK;
    u32 mapped = sf_mapped(m,page);
    if ((offset & 7) + n <= 8) {
        if (mapped) memcpy(out, sf_private(m, mapped - 1, offset), n);
        else if (address < m->snapshot_bytes) memcpy(out, m->data + address, n);
        else memset(out, 0, n);
    } else {
        u8* dest = (u8*)out;
        for (u32 i = 0; i < n; ++i) dest[i] = sf_read_byte(m, address + i);
    }
}
__device__ __forceinline__ void sf_write(wasm_rt_memory_t* m, u64 address, const void* in, u32 n) {
#ifdef SF_GAS_ADDRESS
    if (address >= SF_GAS_ADDRESS && address + n <= SF_GAS_ADDRESS + 8) {
        if (n == 8) memcpy(&m->cached_gas, in, 8);
        else for (u32 k = 0; k < n; ++k) sf_write_byte(m, address+k, ((const u8*)in)[k]);
        return;
    }
#endif
    u32 page = address >> SF_COW_PAGE_SHIFT, offset = address & SF_COW_PAGE_MASK;
    if ((offset & 7) + n <= 8) {
        u32 slot = sf_mapped(m,page);
        if (!slot) slot = sf_make_private(m, page) + 1;
        memcpy(sf_private(m, slot - 1, offset), in, n);
    } else {
        const u8* src = (const u8*)in;
        for (u32 i = 0; i < n; ++i) sf_write_byte(m, address + i, src[i]);
    }
}
template<class T> __device__ __noinline__ T sf_load(wasm_rt_memory_t* m, u64 address) {
    // WASM permits unaligned accesses. Aligned words can use a single device
    // load instead of the byte copies required by an unqualified memcpy.
    if (!(address & (sizeof(T) - 1))
#ifdef SF_GAS_ADDRESS
        && !(address < SF_GAS_ADDRESS + 8 && address + sizeof(T) > SF_GAS_ADDRESS)
#endif
    ) {
        u32 mapped = sf_mapped(m,address >> SF_COW_PAGE_SHIFT);
        if (mapped) return *(const T*)sf_private(m, mapped - 1, address & SF_COW_PAGE_MASK);
        if (address < m->snapshot_bytes) return *(const T*)(m->data + address);
        return T(0);
    }
    T value; sf_read(m, address, &value, sizeof(T)); return value;
}
template<class T> __device__ __noinline__ void sf_store(wasm_rt_memory_t* m, u64 address, T value) {
    if (!(address & (sizeof(T) - 1))
#ifdef SF_GAS_ADDRESS
        && !(address < SF_GAS_ADDRESS + 8 && address + sizeof(T) > SF_GAS_ADDRESS)
#endif
    ) {
        u32 page = address >> SF_COW_PAGE_SHIFT;
        u32 mapped = sf_mapped(m,page);
        if (!mapped) mapped = sf_make_private(m, page) + 1;
        *(T*)sf_private(m, mapped - 1, address & SF_COW_PAGE_MASK) = value;
        return;
    }
    sf_write(m, address, &value, sizeof(T));
}
__device__ void sf_fill(wasm_rt_memory_t* m, u64 d, u32 val, u64 n) {
    u8 byte = val;
    while (n && (d & 7)) { sf_write_byte(m, d++, byte); --n; }
    u64 word = (u64)byte * 0x0101010101010101ULL;
    while (n >= 8) { sf_store(m, d, word); d += 8; n -= 8; }
    while (n) { sf_write_byte(m, d++, byte); --n; }
}
__device__ void sf_copy(wasm_rt_memory_t* dest, const wasm_rt_memory_t* src_const, u64 d, u64 s, u64 n) {
    wasm_rt_memory_t* src = const_cast<wasm_rt_memory_t*>(src_const);
    u8 byte;
    if (dest == src && d > s && d < s + n) {
        while (n) { --n; sf_read(src, s + n, &byte, 1); sf_write(dest, d + n, &byte, 1); }
    } else {
        while (n >= 8 && !((s | d) & 7)) { sf_store(dest, d, sf_load<u64>(src, s)); s += 8; d += 8; n -= 8; }
        while (n) { sf_write_byte(dest, d++, sf_read_byte(src, s++)); --n; }
    }
}
#endif
__device__ u64 wasm_rt_grow_memory(wasm_rt_memory_t* mem, u64 pages) {
    if (pages > mem->max_pages - mem->pages) return ~0ULL;
    u64 old = mem->pages;
    mem->pages += pages; mem->size = mem->pages * mem->page_size;
    return old;
}
