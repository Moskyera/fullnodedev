#include "util.cl"
#include "x16rs.cl"
#include "sha3_256.cl"

inline int diff_big_hash(const hash_32 *src, const hash_32 *tar)
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

__attribute__((work_group_size_hint(256, 1, 1)))
__kernel void x16rs_main(
    __constant const block_t* input_stuff_89,
    const unsigned int nonce_start,
    const unsigned int x16rs_repeat,
    const unsigned int unit_size,
    __global hash_32* global_hashes,
    __global unsigned int* global_order,
    __global hash_32* best_hashes,
    __global unsigned int* best_nonces
) {
    const unsigned int local_id = get_local_id(0);
    const unsigned int local_size = get_local_size(0);
    const unsigned int group_id = get_group_id(0);
    const unsigned int index = local_id * unit_size;
    hash_32* local_hashes = global_hashes + (group_id * local_size * unit_size);
    __local unsigned int local_nonces[256];
    __global unsigned int* local_order = global_order + (group_id * local_size * unit_size);
    __local unsigned int ALIGN histogram[16];
    __local unsigned int ALIGN starting_index[16];
    __local unsigned int ALIGN offset[16];

    // Setup x16 local shared vars
    X16RS_INIT_SHARED_TABLES(local_id, local_size);

    block_t base_stuff = input_stuff_89[0];
    
    const unsigned int global_offset = nonce_start + (get_global_id(0) * unit_size);
    X16RS_PRAGMA_UNROLL_8
    for (unsigned int i = 0; i < unit_size; i++) {
        // Insert Nonce
#ifdef NVIDIA_GPU
        const unsigned int nonce = global_offset + i;
#else
        volatile const unsigned int nonce = global_offset + i;
#endif
        write_nonce_to_bytes(79, base_stuff.h1, nonce);
        // Hash Block
        sha3_256_hash(base_stuff.h8, local_hashes[index + i].h8);
    }          
    barrier(CLK_LOCAL_MEM_FENCE | CLK_GLOBAL_MEM_FENCE);

#ifdef AMD_GFX_GFX1201
    X16RS_RUN_REPEAT_LOOP(
        local_id, local_size, unit_size, x16rs_repeat,
        local_hashes, index, local_order,
        histogram, starting_index, offset,
        H_blake,
        T0_G, T0_G, T0_G, T0_G,
        AES0_C, AES1_C, AES2_C, AES3_C,
        plain_T0, plain_T1, plain_T2, plain_T3, plain_T4, plain_T5, plain_T6, plain_T7,
        mixtab0_c, mixtab1_c, mixtab2_c, mixtab3_c
    );
#else
    X16RS_RUN_REPEAT_LOOP(
        local_id, local_size, unit_size, x16rs_repeat,
        local_hashes, index, local_order,
        histogram, starting_index, offset,
        H_blake,
        T0, T1, T2, T3,
        AES0, AES1, AES2, AES3,
        LT0, LT1, LT2, LT3, LT4, LT5, LT6, LT7,
        mixtab0, mixtab1, mixtab2, mixtab3
    );
#endif
    
    unsigned int best_hash = index;
    X16RS_PRAGMA_UNROLL_8
    for (unsigned int i = 1; i < unit_size; i++) {
        if (diff_big_hash(&local_hashes[best_hash], &local_hashes[index + i]) == 1) {
            best_hash = index + i;
        }
    }
    barrier(CLK_LOCAL_MEM_FENCE | CLK_GLOBAL_MEM_FENCE);

    local_hashes[index] = local_hashes[best_hash];
    local_nonces[local_id] = global_offset + best_hash - index;
    
    barrier(CLK_LOCAL_MEM_FENCE | CLK_GLOBAL_MEM_FENCE);

    // Now perform the reduction across threads
    for (unsigned int smax = local_size >> 1; smax > 0; smax >>= 1) {
        if (local_id < smax) {
            unsigned int idx_current = index;
            unsigned int idx_pair = (local_id + smax) * unit_size;
            if (diff_big_hash(&local_hashes[idx_current], &local_hashes[idx_pair]) == 1) {
                local_hashes[idx_current] = local_hashes[idx_pair];
                local_nonces[local_id] = local_nonces[local_id + smax];
            }
        }
        barrier(CLK_LOCAL_MEM_FENCE | CLK_GLOBAL_MEM_FENCE);
    }

    if(local_id == 0) {
        best_nonces[group_id] = local_nonces[0];
    }
    if(local_id < 32) {
        best_hashes[group_id].h1[local_id] = local_hashes[0].h1[local_id];
    }
}

#ifdef AMD_GFX_GFX1201
/* One-work-item protocol vector used by the host before mining starts. */
__kernel void x16rs_test_groestl2(
    __global const hash_32* input_hashes,
    __global hash_32* output_hashes
) {
    if (get_global_id(0) != 0) {
        return;
    }
    hash_32 value = input_hashes[0];
    hash_x16rs_func_2(&value, T0_G, T0_G, T0_G, T0_G);
    output_hashes[0] = value;
}
#endif
