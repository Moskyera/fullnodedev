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
        eprintln!("CUDA kernels not compiled (install CUDA Toolkit + rebuild with --features cuda)");
        return;
    }
    let intro = hex::decode(GENESIS_INTRO).unwrap();
    let miner = x16rs_cuda::CudaMiner::new(0, 1, 1).expect("cuda miner");
    let gpu_hash = miner.block_hash_once(1, &intro).expect("cuda hash");
    assert_eq!(hex::encode(gpu_hash), GENESIS_HASH);
}