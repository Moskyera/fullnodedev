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
// OpenCL `__global` address-space qualifier -> nothing in CUDA. (Distinct token from
// CUDA's `__global__`, which is a whole-token identifier and is NOT affected.) Only
// reached by currently-dead DEC64BE/DEC32LE paths; defined for robustness.
#define __global
#define __inline__ inline __device__

#define CLK_LOCAL_MEM_FENCE 0
#define barrier(x) __syncthreads()

#define get_local_id(dim) threadIdx.x
#define get_group_id(dim) blockIdx.x
#define get_local_size(dim) blockDim.x
#define get_global_id(dim) (blockIdx.x * blockDim.x + threadIdx.x)
#define get_global_size(dim) (gridDim.x * blockDim.x)

// Width-correct rotate. The OpenCL `rotate` builtin rotates within the operand's own
// bit width. A single 64-bit macro silently MISCOMPILES 32-bit rotates: shifting a
// uint right by (64 - n) with n in 0..31 shifts by 33..63 -> undefined behaviour ->
// nvcc's PTX yields 0, so the rotate degenerates into a plain left shift. That
// corrupts cubehash/luffa/simd/hamsi (via SPH_ROTL32) and the AES tables
// (rotate(AES0[i], 8U) -> shavite/echo). Provide width-specific overloads; the type
// of the first argument selects the width. Both count params are `uint` so the first
// argument alone disambiguates (no overload ambiguity when a 32-bit value is rotated
// by a `ulong` literal count, e.g. `rotate(uint_val, 8UL)`); the count is re-masked
// internally, preserving OpenCL's modulo-width semantics.
static __device__ __forceinline__ uint rotate(uint x, uint n) {
    n &= 31u;
    return (x << n) | (x >> ((32u - n) & 31u));
}
static __device__ __forceinline__ ulong rotate(ulong x, uint n) {
    n &= 63u;
    return (x << n) | (x >> ((64u - n) & 63u));
}
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

// Do NOT redefine __attribute__ wholesale. On modern nvcc (12.x, Linux) CUDA's
// own __global__/__device__ qualifiers expand THROUGH __attribute__, so stripping
// it silently turns every kernel into a plain __host__ function (then __syncthreads
// fails to compile). Only neutralize the OpenCL-specific attributes nvcc does not
// understand; __attribute__((aligned(N))) (util.cl's ALIGN macros) is accepted by
// nvcc as-is.
#define work_group_size_hint(x, y, z)
#define reqd_work_group_size(x, y, z)

inline __device__ uint atomic_inc(uint *addr) {
    return atomicAdd(addr, 1u);
}