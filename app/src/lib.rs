include! {"version.rs"}

// First, and `macro_use`, because `wlogln!` and `wlogerr!` replace `println!`
// and `eprintln!` throughout this crate: they must be in scope for every module
// declared below.
#[macro_use]
pub mod worker_log;

pub mod efficiency;
pub mod gpu_arch;
pub mod gpu_oom;
/// Windows only: the AMD driver's own library is the only GPU temperature
/// source that exists on a consumer Windows install.
#[cfg(windows)]
pub mod gpu_temp_adl;
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
/// Byte-equivalence gate + fixed-work baseline for the OpenCL x16rs kernel.
/// Never called by the mining path; it is the measuring instrument.
pub mod x16rs_gate;
/// Auto-tune measured at the mainnet x16rs repeat, on a corpus frozen for the
/// whole session, scored on the watts the card reports.
pub mod autotune16;
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
