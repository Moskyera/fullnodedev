include! {"version.rs"}

pub mod efficiency;
pub mod gpu_arch;

pub mod diaworker;
pub mod poworker;
#[cfg(feature = "ocl")]
pub mod opencl_list;
// pub mod svrapi; // server api
pub mod diabider;
pub mod fullnode;
