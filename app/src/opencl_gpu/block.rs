use crate::gpu_oom::GpuBatchError;
use crate::hash_util::hash_more_power;
use crate::opencl_gpu::{
    OpenCLResources, SHARE_LIST_CAPACITY, enqueue_mining_kernel, read_block_gpu_results,
    read_share_gpu_results, write_share_inputs_to_gpu, write_stuff_to_gpu,
};

/// Block intro serialization is a fixed 89-byte layout (matches CUDA STUFF_BYTES).
const BLOCK_INTRO_BYTES: usize = 89;

/// Everything one OpenCL block batch produced.
pub struct OpenclBatchOutput {
    /// The batch's single strongest nonce/hash, from the work-group tree
    /// reduction. Unchanged, and still the only thing solo mining reads.
    pub best: (u32, [u8; 32]),
    /// Every nonce whose hash beat the share target, up to the buffer capacity.
    /// Always empty when the caller passed no share target.
    pub shares: Vec<(u32, [u8; 32])>,
    /// How many hits the kernel counted in TOTAL, including the ones that did
    /// not fit in `shares`. Greater than `shares.len()` means the batch is
    /// undersampling and the miner is earning less than it mined.
    pub share_hits: u64,
}

/// One block batch, best result only.
///
/// Kept exactly as it was for the benchmark and auto-tune callers, which want
/// one hash per batch and nothing else. Pool mining goes through
/// [`do_group_block_mining_opencl_shares`].
pub fn do_group_block_mining_opencl(
    opencl: &OpenCLResources,
    height: u64,
    block_intro: Vec<u8>,
    nonce_start: u32,
    num_work_groups: u32,
    local_work_size: u32,
    unit_size: u32,
) -> std::result::Result<(u32, [u8; 32]), GpuBatchError> {
    do_group_block_mining_opencl_shares(
        opencl,
        height,
        block_intro,
        nonce_start,
        num_work_groups,
        local_work_size,
        unit_size,
        None,
    )
    .map(|out| out.best)
}

/// One block batch, plus the list of every nonce that beat `share_target`.
///
/// `share_target` is `None` for solo mining. That is the whole guarantee that
/// solo behaviour is untouched: the kernel is launched with share_capacity=0,
/// which skips the appending block, and the host writes and reads not one extra
/// byte over the bus.
#[allow(clippy::too_many_arguments)]
pub fn do_group_block_mining_opencl_shares(
    opencl: &OpenCLResources,
    height: u64,
    block_intro: Vec<u8>,
    nonce_start: u32,
    num_work_groups: u32,
    local_work_size: u32,
    unit_size: u32,
    share_target: Option<&[u8; 32]>,
) -> std::result::Result<OpenclBatchOutput, GpuBatchError> {
    if block_intro.len() != BLOCK_INTRO_BYTES {
        return Err(GpuBatchError::from_message(&format!(
            "block intro must be {BLOCK_INTRO_BYTES} bytes, got {}",
            block_intro.len()
        )));
    }
    let mut most_nonce = 0u32;
    let mut most_hash = [255u8; 32];
    let repeat = x16rs::block_hash_repeat(height) as u32;

    let write_event = write_stuff_to_gpu(opencl, &block_intro, None)
        .map_err(|e| GpuBatchError::from_message(&e))?;

    let (share_capacity, ready_event) = match share_target {
        Some(target) => {
            let event = write_share_inputs_to_gpu(opencl, target, Some(&write_event))
                .map_err(|e| GpuBatchError::from_message(&e))?;
            (SHARE_LIST_CAPACITY as u32, event)
        }
        None => (0u32, write_event),
    };

    let kernel_event = enqueue_mining_kernel(
        opencl,
        nonce_start,
        repeat,
        unit_size,
        num_work_groups,
        local_work_size,
        share_capacity,
        Some(&ready_event),
    )?;

    let mut hashes = vec![0u8; opencl.buffer_best_hashes.len()];
    let mut nonces = vec![0u32; opencl.buffer_best_nonces.len()];
    read_block_gpu_results(opencl, &kernel_event, &mut hashes, &mut nonces)
        .map_err(|e| GpuBatchError::from_message(&e))?;

    for i in 0..num_work_groups as usize {
        let hash_bytes = &hashes[i * 32..(i * 32) + 32];
        if hash_more_power(hash_bytes, &most_hash) {
            most_hash.copy_from_slice(hash_bytes);
            most_nonce = nonces[i];
        }
    }

    let mut shares = Vec::new();
    let mut share_hits = 0u64;
    if share_capacity > 0 {
        let mut share_nonces: Vec<u32> = Vec::new();
        let mut share_hashes: Vec<u8> = Vec::new();
        share_hits =
            read_share_gpu_results(opencl, &kernel_event, &mut share_nonces, &mut share_hashes)
                .map_err(|e| GpuBatchError::from_message(&e))?;
        shares.reserve(share_nonces.len());
        for (i, nonce) in share_nonces.iter().enumerate() {
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&share_hashes[i * 32..(i * 32) + 32]);
            shares.push((*nonce, hash));
        }
    }

    if opencl.needs_queue_finish {
        opencl
            .queue
            .finish()
            .map_err(|e| GpuBatchError::from_message(&format!("queue finish: {}", e)))?;
    }

    Ok(OpenclBatchOutput {
        best: (most_nonce, most_hash),
        shares,
        share_hits,
    })
}

#[cfg(test)]
mod gpu_tests {
    use super::*;
    use crate::opencl_gpu::initialize_opencl;

    /// Mainnet repeat count is 16 from height 750000 on, so the kernel under
    /// test here runs the same algorithm mix a real block does.
    const REPEAT16_HEIGHT: u64 = 800_000;
    const WORK_GROUPS: u32 = 2;
    const LOCAL_SIZE: u32 = 256;
    const UNIT_SIZE: u32 = 4;
    const BATCH_NONCES: u32 = WORK_GROUPS * LOCAL_SIZE * UNIT_SIZE;
    const NONCE_START: u32 = 4_000;

    fn opencl_dir() -> String {
        std::env::var("HACASH_OPENCL_DIR").unwrap_or_else(|_| {
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("app crate must have a workspace parent")
                .join("x16rs")
                .join("opencl")
                .to_string_lossy()
                .into_owned()
        })
    }

    fn cpu_hash(intro: &[u8], nonce: u32) -> [u8; 32] {
        let mut stuff = intro.to_vec();
        stuff[79..83].copy_from_slice(&nonce.to_be_bytes());
        x16rs::block_hash(REPEAT16_HEIGHT, &stuff)
    }

    #[test]
    #[ignore = "requires a physical OpenCL GPU; set HACASH_RUN_OPENCL_INTEGRATION=1"]
    fn the_share_list_matches_the_cpu_and_leaves_the_best_result_untouched() {
        assert_eq!(
            std::env::var_os("HACASH_RUN_OPENCL_INTEGRATION").as_deref(),
            Some(std::ffi::OsStr::new("1")),
            "set HACASH_RUN_OPENCL_INTEGRATION=1 before running this ignored GPU test"
        );
        let platform: u32 = std::env::var("HACASH_OPENCL_PLATFORM_ID")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let devices = std::env::var("HACASH_OPENCL_DEVICE_IDS").unwrap_or_else(|_| "0".to_string());
        let mut resources = initialize_opencl(
            false,
            &opencl_dir(),
            &platform,
            &devices,
            &WORK_GROUPS,
            &LOCAL_SIZE,
            &UNIT_SIZE,
            None,
            false,
        );
        assert!(!resources.is_empty(), "no usable OpenCL device");
        let opencl = resources.remove(0);

        // The kernel's sha3_256_hash folds the 89-byte message's padding into a
        // constant word that hard-codes byte 88 as 0x00 (a real block intro ends
        // in the two zero bytes of its transaction count), so a test intro has
        // to honour that or the card and the CPU hash different messages.
        let mut intro: Vec<u8> = (0..BLOCK_INTRO_BYTES as u8).collect();
        intro[88] = 0;

        // What the CPU says about this exact window, which is the authority.
        let mut cpu: Vec<(u32, [u8; 32])> = (NONCE_START..NONCE_START + BATCH_NONCES)
            .map(|nonce| (nonce, cpu_hash(&intro, nonce)))
            .collect();
        let cpu_best = *cpu
            .iter()
            .min_by(|a, b| a.1.cmp(&b.1))
            .expect("non empty window");

        // 1. SOLO: no share target, so the kernel skips the whole added block.
        //    The single best result has to be the CPU's, byte for byte.
        let solo = do_group_block_mining_opencl_shares(
            &opencl,
            REPEAT16_HEIGHT,
            intro.clone(),
            NONCE_START,
            WORK_GROUPS,
            LOCAL_SIZE,
            UNIT_SIZE,
            None,
        )
        .expect("solo batch");
        assert_eq!(
            solo.best.1,
            cpu_hash(&intro, solo.best.0),
            "the hash the card reports for its own nonce must be the CPU's"
        );
        assert_eq!(solo.best, cpu_best, "solo best must match the CPU exactly");
        assert!(solo.shares.is_empty(), "solo must never build a share list");
        assert_eq!(solo.share_hits, 0);

        // 2. POOL, easiest possible target: every nonce is payable, so the
        //    counter sees the whole window and the list reports the overflow.
        let pool = do_group_block_mining_opencl_shares(
            &opencl,
            REPEAT16_HEIGHT,
            intro.clone(),
            NONCE_START,
            WORK_GROUPS,
            LOCAL_SIZE,
            UNIT_SIZE,
            Some(&[0xffu8; 32]),
        )
        .expect("pool batch");
        assert_eq!(
            pool.best, cpu_best,
            "adding the share list must not perturb the reduction"
        );
        assert_eq!(
            pool.share_hits, BATCH_NONCES as u64,
            "the counter must see every hit, not only the stored ones"
        );
        assert_eq!(
            pool.shares.len(),
            SHARE_LIST_CAPACITY.min(BATCH_NONCES as usize)
        );
        let mut seen: Vec<u32> = pool.shares.iter().map(|(nonce, _)| *nonce).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            pool.shares.len(),
            "no nonce may be listed twice"
        );
        for (nonce, hash) in &pool.shares {
            assert!(
                (NONCE_START..NONCE_START + BATCH_NONCES).contains(nonce),
                "share nonce {nonce} is outside the batch window"
            );
            assert_eq!(
                *hash,
                cpu_hash(&intro, *nonce),
                "share hash must match the CPU"
            );
        }

        // 3. POOL, a target only three nonces beat: exactly those three, and
        //    nothing else, may come back.
        cpu.sort_by(|a, b| a.1.cmp(&b.1));
        let strict_target = cpu[2].1;
        let expected: Vec<u32> = {
            let mut want: Vec<u32> = cpu[..3].iter().map(|(nonce, _)| *nonce).collect();
            want.sort_unstable();
            want
        };
        let strict = do_group_block_mining_opencl_shares(
            &opencl,
            REPEAT16_HEIGHT,
            intro.clone(),
            NONCE_START,
            WORK_GROUPS,
            LOCAL_SIZE,
            UNIT_SIZE,
            Some(&strict_target),
        )
        .expect("strict batch");
        assert_eq!(strict.best, cpu_best);
        assert_eq!(strict.share_hits, 3);
        let mut got: Vec<u32> = strict.shares.iter().map(|(nonce, _)| *nonce).collect();
        got.sort_unstable();
        assert_eq!(
            got, expected,
            "the kernel must list exactly the payable nonces"
        );
    }
}
