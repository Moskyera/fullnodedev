include! {"version.rs"}

pub mod efficiency;
pub mod gpu_arch;
pub mod gpu_oom;
pub mod hash_util;
pub mod mining_runtime;
pub mod panel_tuning;
pub mod mining_stats;
pub mod mining_batch;
#[macro_use]
mod mining_util;

pub mod diaworker;
pub mod poworker;
pub mod opencl_diag;
#[cfg(feature = "ocl")]
pub mod opencl_gpu;
#[cfg(feature = "ocl")]
pub mod opencl_list;
// pub mod svrapi; // server api
pub mod diabider;
pub mod fullnode;
