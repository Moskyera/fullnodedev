//! OpenCL GPU context, buffers, per-device OOM recovery, and kernel dispatch.

pub mod block;
mod compile;
mod handle;
mod init;
mod resources;

pub use handle::{OpenclGpuHandle, OpenclGpuSnapshot, opencl_snapshot_from_resource};
pub use init::initialize_opencl;
pub use resources::{
    OpenCLResources, enqueue_diamond_kernel, enqueue_mining_kernel, read_block_gpu_results,
    read_diamond_gpu_results, write_stuff_to_gpu,
};
