include! {"version.rs"}

pub mod efficiency;
pub mod gpu_arch;
pub mod gpu_oom;
pub mod hash_util;
pub mod mining_batch;
pub mod mining_guard;
pub mod mining_runtime;
pub mod mining_stats;
pub mod panel_tuning;
pub mod rpc_http;
#[macro_use]
mod mining_util;

pub mod bench_mainnet_repeat16;
pub mod diaworker;
pub mod opencl_diag;
#[cfg(feature = "ocl")]
pub mod opencl_gpu;
#[cfg(feature = "ocl")]
pub mod opencl_list;
pub mod poworker;
// pub mod svrapi; // server api
pub mod diabider;
pub mod fullnode;
pub mod node_api;
