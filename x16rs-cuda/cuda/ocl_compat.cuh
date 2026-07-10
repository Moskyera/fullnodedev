#pragma once

#include <stdint.h>
#include <vector_types.h>

// OpenCL → CUDA compatibility layer for reusing Hacash OpenCL kernels.

typedef uint8_t uchar;
typedef uint32_t uint;
typedef uint64_t ulong;
typedef int32_t sph_s32;

#define __kernel __global__
#define __local_array __shared__
#define OCL_LOCAL_PTR
#ifndef HAMSI_CONST_PTR
#define HAMSI_CONST_PTR const sph_u32 *
#define HAMSI_LOCAL_PTR sph_u32 *
#endif
#define __constant __constant__
#define __private
#define __generic
#define __inline__ inline __device__

#define CLK_LOCAL_MEM_FENCE 0
#define barrier(x) __syncthreads()

#define get_local_id(dim) threadIdx.x
#define get_group_id(dim) blockIdx.x
#define get_local_size(dim) blockDim.x
#define get_global_id(dim) (blockIdx.x * blockDim.x + threadIdx.x)
#define get_global_size(dim) (gridDim.x * blockDim.x)

#define rotate(x, n) \
    (((x) << ((n) & 63)) | ((x) >> (64 - ((n) & 63))))
#define as_ulong(x) ((ulong)(x))
#define as_uint(x) ((uint)(x))
#define as_uint2(x) (*(const uint2 *)&(x))

#define OCL_AS_ULONG_UINT2_S10(v) \
    ((ulong) (*(const uint2 *)&(v)).y | ((ulong)(*(const uint2 *)&(v)).x << 32))

#define OCL_SWAP4(x) \
    ((uint)((((uint)(x) & 0x000000ffu) << 24) | (((uint)(x) & 0x0000ff00u) << 8) | \
            (((uint)(x) & 0x00ff0000u) >> 8) | (((uint)(x) & 0xff000000u) >> 24)))

#define OCL_SWAP8(x) \
    ((ulong)((((ulong)(x) & 0x00000000000000ffull) << 56) | \
             (((ulong)(x) & 0x000000000000ff00ull) << 40) | \
             (((ulong)(x) & 0x0000000000ff0000ull) << 24) | \
             (((ulong)(x) & 0x00000000ff000000ull) << 8) | \
             (((ulong)(x) & 0x000000ff00000000ull) >> 8) | \
             (((ulong)(x) & 0x0000ff0000000000ull) >> 24) | \
             (((ulong)(x) & 0x00ff000000000000ull) >> 40) | \
             (((ulong)(x) & 0xff00000000000000ull) >> 56)))

#define I64(x) x##ULL
#define le2me_64(x) (x)
#define ROTL64(x, n) rotate((x), (n))

#define X16RS_PRAGMA_UNROLL_8 _Pragma("unroll 8")
#define X16RS_PRAGMA_UNROLL_4 _Pragma("unroll 4")

#define ALIGN8 __align__(8)
#define ALIGN __align__(16)
#define ALIGN32 __align__(32)
#define ALIGN64 __align__(64)

#define __attribute__(x)

inline __device__ uint atomic_inc(uint *addr) {
    return atomicAdd(addr, 1u);
}