#include "ocl_compat.cuh"

#include "util.cl"
#include "sha3_256.cl"
#include "x16rs.cl"

__constant__ sph_u64 x16rs_d_H_blake[8] = {
    SPH_C64(0x6A09E667F3BCC908), SPH_C64(0xBB67AE8584CAA73B),
    SPH_C64(0x3C6EF372FE94F82B), SPH_C64(0xA54FF53A5F1D36F1),
    SPH_C64(0x510E527FADE682D1), SPH_C64(0x9B05688C2B3E6C1F),
    SPH_C64(0x1F83D9ABFB41BD6B), SPH_C64(0x5BE0CD19137E2179),
};

inline __device__ int diff_big_hash_dev(const hash_32 *src, const hash_32 *tar)
{
#pragma unroll 32
    for (int i = 0; i < 32; i++) {
        if (src->h1[i] > tar->h1[i]) {
            return 1;
        } else if (src->h1[i] < tar->h1[i]) {
            return 0;
        }
    }
    return 0;
}

extern "C" __global__ void x16rs_cuda_main(
    const block_t *input_stuff_89,
    const unsigned int nonce_start,
    const unsigned int x16rs_repeat,
    const unsigned int unit_size,
    hash_32 *global_hashes,
    unsigned int *global_order,
    hash_32 *best_hashes,
    unsigned int *best_nonces)
{
    const unsigned int local_id = threadIdx.x;
    const unsigned int local_size = blockDim.x;
    const unsigned int group_id = blockIdx.x;
    const unsigned int index = local_id * unit_size;
    hash_32 *local_hashes = global_hashes + (group_id * local_size * unit_size);
    __shared__ unsigned int local_nonces[256];
    unsigned int *local_order = global_order + (group_id * local_size * unit_size);
    __shared__ unsigned int ALIGN histogram[16];
    __shared__ unsigned int ALIGN starting_index[16];
    __shared__ unsigned int ALIGN offset[16];

    X16RS_INIT_SHARED_TABLES(local_id, local_size);

    block_t base_stuff = input_stuff_89[0];

    const unsigned int global_offset = nonce_start + ((blockIdx.x * blockDim.x + threadIdx.x) * unit_size);
#pragma unroll 8
    for (unsigned int i = 0; i < unit_size; i++) {
        const unsigned int nonce = global_offset + i;
        write_nonce_to_bytes(79, base_stuff.h1, nonce);
        sha3_256_hash(base_stuff.h8, local_hashes[index + i].h8);
    }
    __syncthreads();

    X16RS_RUN_REPEAT_LOOP(
        local_id, local_size, unit_size, x16rs_repeat,
        local_hashes, index, local_order,
        histogram, starting_index, offset,
        H_blake,
        T0, T1, T2, T3,
        AES0, AES1, AES2, AES3,
        LT0, LT1, LT2, LT3, LT4, LT5, LT6, LT7,
        mixtab0, mixtab1, mixtab2, mixtab3);

    unsigned int best_hash = 0;
#pragma unroll 8
    for (unsigned int i = 1; i < unit_size; i++) {
        if (diff_big_hash_dev(&local_hashes[best_hash], &local_hashes[index + i]) == 1) {
            best_hash = index + i;
        }
    }
    __syncthreads();

    local_hashes[index] = local_hashes[best_hash];
    local_nonces[local_id] = global_offset + best_hash - index;
    __syncthreads();

    for (unsigned int smax = local_size >> 1; smax > 0; smax >>= 1) {
        if (local_id < smax) {
            unsigned int idx_current = index;
            unsigned int idx_pair = (local_id + smax) * unit_size;
            if (diff_big_hash_dev(&local_hashes[idx_current], &local_hashes[idx_pair]) == 1) {
                local_hashes[idx_current] = local_hashes[idx_pair];
                local_nonces[local_id] = local_nonces[local_id + smax];
            }
        }
        __syncthreads();
    }

    if (local_id == 0) {
        best_nonces[group_id] = local_nonces[0];
    }
    if (local_id < 32) {
        best_hashes[group_id].h1[local_id] = local_hashes[0].h1[local_id];
    }
}

extern "C" __global__ void x16rs_cuda_single(
    const block_t *input_stuff_89,
    const unsigned int x16rs_repeat,
    hash_32 *out_hash)
{
    const unsigned int local_id = threadIdx.x;
    const unsigned int local_size = blockDim.x;
    const unsigned int index = 0;
    hash_32 local_hashes[1];
    unsigned int local_order[1];
    __shared__ unsigned int ALIGN histogram[16];
    __shared__ unsigned int ALIGN starting_index[16];
    __shared__ unsigned int ALIGN offset[16];

    X16RS_INIT_SHARED_TABLES(local_id, local_size);

    if (threadIdx.x == 0) {
        block_t base_stuff = input_stuff_89[0];
        sha3_256_hash(base_stuff.h8, local_hashes[0].h8);
    }
    __syncthreads();

    X16RS_RUN_REPEAT_LOOP(
        local_id, local_size, 1, x16rs_repeat,
        local_hashes, index, local_order,
        histogram, starting_index, offset,
        H_blake,
        T0, T1, T2, T3,
        AES0, AES1, AES2, AES3,
        LT0, LT1, LT2, LT3, LT4, LT5, LT6, LT7,
        mixtab0, mixtab1, mixtab2, mixtab3);

    if (threadIdx.x == 0) {
        *out_hash = local_hashes[0];
    }
}