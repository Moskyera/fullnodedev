use x16rs_cuda::{CudaMiner, CudaResult};

pub struct CudaMiningResources {
    pub miner: CudaMiner,
    pub workgroups: u32,
    pub unit_size: u32,
}

pub fn initialize_cuda(
    device_index: i32,
    workgroups: u32,
    unit_size: u32,
) -> Vec<Arc<CudaMiningResources>> {
    if !CudaMiner::is_available() {
        eprintln!(
            "[CUDA] x16rs-cuda built without kernels — rebuild with: cargo build -p poworker --features cuda"
        );
        return Vec::new();
    }

    match CudaMiner::list_devices() {
        Ok(devices) => {
            for d in &devices {
                println!(
                    "[CUDA] Device #{}: {} (SM {}.{}, MP={})",
                    d.index, d.name, d.compute_major, d.compute_minor, d.multiprocessor_count
                );
            }
        }
        Err(e) => {
            eprintln!("[CUDA] enumerate devices failed: {e}");
            return Vec::new();
        }
    }

    match CudaMiner::new(device_index, workgroups, unit_size) {
        Ok(miner) => {
            println!(
                "[CUDA] Initialized device #{device_index} work_groups={workgroups} unit_size={unit_size}"
            );
            vec![Arc::new(CudaMiningResources {
                miner,
                workgroups,
                unit_size,
            })]
        }
        Err(e) => {
            eprintln!("[CUDA] init failed: {e}");
            Vec::new()
        }
    }
}

pub fn do_group_block_mining_cuda(
    cuda: &CudaMiningResources,
    height: u64,
    block_intro: Vec<u8>,
    nonce_start: u32,
    workgroups: u32,
) -> CudaResult<(u32, [u8; 32])> {
    cuda.miner
        .mine_block_batch(height, &block_intro, nonce_start, workgroups)
}