#ifndef X16RX_UTIL_CL
#define X16RX_UTIL_CL

#ifdef NVIDIA_GPU
  #define X16RS_PRAGMA_UNROLL_8 _Pragma("clang unroll(8)")
  #define X16RS_PRAGMA_UNROLL_4 _Pragma("clang unroll(4)")
#else
  #define X16RS_PRAGMA_UNROLL_8
  #define X16RS_PRAGMA_UNROLL_4
#endif

// Under CUDA these are made no-ops by ocl_compat.cuh (Windows nvcc/MSVC rejects the
// __attribute__((aligned)) spelling). Only (re)define the OpenCL attribute versions when
// not building for CUDA, so OpenCL builds are byte-identical to before.
#ifndef __CUDA__
#define ALIGN8 __attribute__((aligned(8)))
#define ALIGN __attribute__((aligned(16)))
#define ALIGN32 __attribute__((aligned(32)))
#define ALIGN64 __attribute__((aligned(64)))
#endif

// Alignment qualifier for *function parameters*. C++/nvcc forbids an alignment
// specifier on a parameter (an array param decays to a pointer), whereas OpenCL C
// allows it. So ALIGN_PARAM keeps the OpenCL hint on OpenCL builds and is a no-op
// under CUDA. Use ALIGN_PARAM (not ALIGN) on any parameter declaration; keep ALIGN
// on struct/union/local/shared/constant declarations (nvcc accepts those).
#ifdef __CUDA__
  #define ALIGN_PARAM
#else
  #define ALIGN_PARAM ALIGN
#endif

typedef union ALIGN8 {
  unsigned char h1[88];
  ulong h8[11];
} block_t;

typedef union ALIGN8 {
  unsigned char h1[96];
  ulong h8[12];
} block_diamond_t;

#ifdef __ENDIAN_LITTLE__

    #define WRITE_NONCE_BYTE4 bytes[offset+0] = nonce_ptr[3]; \
    bytes[offset+1] = nonce_ptr[2];\
    bytes[offset+2] = nonce_ptr[1];\
    bytes[offset+3] = nonce_ptr[0];

#else

    #define WRITE_NONCE_BYTE4 bytes[offset+0] = nonce_ptr[0];\
    bytes[offset+1] = nonce_ptr[1];\
    bytes[offset+2] = nonce_ptr[2];\
    bytes[offset+3] = nonce_ptr[3];

#endif

__inline__ void write_nonce_to_bytes(const int offset, unsigned char* bytes, unsigned int nonce) {
    // nonce bytes
    unsigned char *nonce_ptr = (unsigned char *)&nonce;
    WRITE_NONCE_BYTE4;
}

__inline__ void write_nonce_u64_to_bytes(const int offset, unsigned char* bytes, ulong nonce) {
    bytes[offset + 0] = (uchar)(nonce >> 56);
    bytes[offset + 1] = (uchar)(nonce >> 48);
    bytes[offset + 2] = (uchar)(nonce >> 40);
    bytes[offset + 3] = (uchar)(nonce >> 32);
    bytes[offset + 4] = (uchar)(nonce >> 24);
    bytes[offset + 5] = (uchar)(nonce >> 16);
    bytes[offset + 6] = (uchar)(nonce >> 8);
    bytes[offset + 7] = (uchar)(nonce);
}

#endif