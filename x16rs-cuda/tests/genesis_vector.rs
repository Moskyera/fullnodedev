//! Genesis block hash cross-check (CPU reference always; GPU when built with `cuda`).

use x16rs::block_hash;

const GENESIS_INTRO: &str = "010000000000005c57b08c0000000000000000000000000000000000000000000000000000000000000000ad557702fc70afaf70a855e7b8a4400159643cb5a7fc8a89ba2bce6f818a9b0100000001098b3445000000000000";
const GENESIS_HASH: &str = "000000077790ba2fcdeaef4a4299d9b667135bac577ce204dee8388f1b97f7e6";

#[test]
fn cpu_genesis_block_hash() {
    let intro = hex::decode(GENESIS_INTRO).unwrap();
    let hash = block_hash(1, &intro);
    assert_eq!(hex::encode(hash), GENESIS_HASH);
}

#[test]
#[cfg(feature = "cuda")]
fn cuda_genesis_block_hash_when_available() {
    if !x16rs_cuda::CudaMiner::is_available() {
        eprintln!(
            "CUDA kernels not compiled (install CUDA Toolkit + rebuild with --features cuda)"
        );
        return;
    }
    let intro = hex::decode(GENESIS_INTRO).unwrap();
    let miner = x16rs_cuda::CudaMiner::new(0, 1, 1).expect("cuda miner");
    let gpu_hash = miner.block_hash_once(1, &intro).expect("cuda hash");
    assert_eq!(hex::encode(gpu_hash), GENESIS_HASH);
}

/// The genesis vector selects only ONE of the 16 x16rs algorithms (repeat = 1). This
/// sweeps the input so every `h4[7] % 16` selection — hence every algorithm and both
/// rotate widths — is exercised, and also validates the full 16-round chain at
/// mainnet repeat = 16. Every GPU hash must equal the CPU reference byte-for-byte.
#[test]
#[cfg(feature = "cuda")]
fn cuda_matches_cpu_across_many_inputs() {
    if !x16rs_cuda::CudaMiner::is_available() {
        eprintln!("CUDA kernels not compiled; skipping cross-check");
        return;
    }
    let base = hex::decode(GENESIS_INTRO).unwrap();
    assert_eq!(base.len(), 89);
    let miner = x16rs_cuda::CudaMiner::new(0, 1, 1).expect("cuda miner");

    // repeat = 1: one algorithm selection per input. 4096 inputs cover all 16 well.
    let mut mismatches = 0usize;
    for nonce in 0u32..4096 {
        let mut intro = base.clone();
        intro[79..83].copy_from_slice(&nonce.to_le_bytes());
        let cpu = block_hash(1, &intro);
        let gpu = miner.block_hash_once(1, &intro).expect("cuda hash");
        if gpu != cpu {
            if mismatches < 8 {
                eprintln!(
                    "repeat1 nonce {}: cpu={} gpu={}",
                    nonce,
                    hex::encode(cpu),
                    hex::encode(gpu)
                );
            }
            mismatches += 1;
        }
    }
    assert_eq!(
        mismatches, 0,
        "{}/4096 GPU hashes disagreed with CPU at repeat=1",
        mismatches
    );

    // repeat = 16 (mainnet height >= 750k): validates the full repeat chain, which
    // applies many algorithms per hash.
    let h16 = 800_000u64;
    assert_eq!(x16rs::block_hash_repeat(h16), 16);
    let mut mismatches16 = 0usize;
    for nonce in 0u32..512 {
        let mut intro = base.clone();
        intro[79..83].copy_from_slice(&nonce.to_le_bytes());
        let cpu = block_hash(h16, &intro);
        let gpu = miner.block_hash_once(h16, &intro).expect("cuda hash");
        if gpu != cpu {
            if mismatches16 < 8 {
                eprintln!(
                    "repeat16 nonce {}: cpu={} gpu={}",
                    nonce,
                    hex::encode(cpu),
                    hex::encode(gpu)
                );
            }
            mismatches16 += 1;
        }
    }
    assert_eq!(
        mismatches16, 0,
        "{}/512 GPU hashes disagreed with CPU at repeat=16",
        mismatches16
    );
}

/// Validates the batch mining kernel (`x16rs_cuda_main`) + the host-side cross-workgroup
/// aggregation end to end: for each configuration the returned (nonce, hash) must be
/// self-consistent (hash == block_hash of that nonce) AND must be the true lexicographic
/// minimum over the whole covered nonce span. Covers single- and multi-workgroup and
/// mainnet repeat=16.
#[test]
#[cfg(feature = "cuda")]
fn cuda_batch_matches_cpu() {
    if !x16rs_cuda::CudaMiner::is_available() {
        eprintln!("CUDA kernels not compiled; skipping batch test");
        return;
    }
    let base = hex::decode(GENESIS_INTRO).unwrap();
    const LOCAL_SIZE: u32 = 256;

    // The kernel writes the nonce big-endian at offset 79 (write_nonce_to_bytes under
    // __ENDIAN_LITTLE__). Replicate that so the CPU reference hashes identical bytes.
    let cpu_hash = |height: u64, nonce: u32| -> [u8; 32] {
        let mut intro = base.clone();
        intro[79..83].copy_from_slice(&nonce.to_be_bytes());
        block_hash(height, &intro)
    };
    let cpu_argmin = |height: u64, start: u32, span: u32| -> (u32, [u8; 32]) {
        let mut best_nonce = start;
        let mut best_hash = cpu_hash(height, start);
        for nonce in start..start.wrapping_add(span) {
            let h = cpu_hash(height, nonce);
            if h < best_hash {
                best_hash = h;
                best_nonce = nonce;
            }
        }
        (best_nonce, best_hash)
    };

    // (height, workgroups, unit_size, nonce_start)
    let cases: [(u64, u32, u32, u32); 3] = [
        (1, 1, 4, 10_000),   // single workgroup: exercises the in-kernel reduction
        (1, 4, 2, 500_000),  // multi-workgroup: exercises host aggregation (min across wg)
        (800_000, 2, 2, 77), // repeat=16 mainnet, multi-workgroup
    ];
    for (height, wg, unit, start) in cases {
        let miner = x16rs_cuda::CudaMiner::new(0, wg, unit).expect("cuda miner");
        let (gpu_nonce, gpu_hash) = miner
            .mine_block_batch(height, &base, start, wg)
            .expect("mine batch");
        assert_eq!(
            cpu_hash(height, gpu_nonce),
            gpu_hash,
            "h={} wg={}: GPU (nonce {}, hash) is not self-consistent",
            height,
            wg,
            gpu_nonce
        );
        let span = wg * LOCAL_SIZE * unit;
        let (cpu_nonce, cpu_min) = cpu_argmin(height, start, span);
        assert_eq!(
            gpu_hash, cpu_min,
            "h={} wg={}: GPU best hash != CPU argmin over {} nonces",
            height, wg, span
        );
        assert_eq!(
            gpu_nonce, cpu_nonce,
            "h={} wg={}: GPU best nonce {} != CPU argmin nonce {}",
            height, wg, gpu_nonce, cpu_nonce
        );
    }
}
