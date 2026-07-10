//! CUDA block miner for Hacash x16rs PoW.
//!
//! Enable with `--features cuda` and NVIDIA CUDA Toolkit installed (`CUDA_PATH`).

use std::ffi::c_void;

pub const STUFF_BYTES: usize = 89;
pub const HASH_BYTES: usize = 32;
pub const DEFAULT_LOCAL_SIZE: u32 = 256;

#[derive(Debug, Clone)]
pub struct CudaDeviceInfo {
    pub index: i32,
    pub name: String,
    pub compute_major: i32,
    pub compute_minor: i32,
    pub multiprocessor_count: i32,
}

#[derive(Debug)]
pub struct CudaMiner {
    device: i32,
    stuff_buf: *mut c_void,
    best_hashes_buf: *mut c_void,
    best_nonces_buf: *mut c_void,
    global_hashes_buf: *mut c_void,
    global_order_buf: *mut c_void,
    workgroups: u32,
    local_size: u32,
    unit_size: u32,
}

// Device pointers are owned exclusively; each launch calls cudaSetDevice first.
unsafe impl Send for CudaMiner {}
unsafe impl Sync for CudaMiner {}

#[derive(Debug)]
pub enum CudaError {
    NotCompiled,
    Driver(String),
    InvalidArgs(String),
}

impl std::fmt::Display for CudaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CudaError::NotCompiled => write!(f, "x16rs-cuda built without CUDA kernels"),
            CudaError::Driver(msg) => write!(f, "CUDA: {msg}"),
            CudaError::InvalidArgs(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for CudaError {}

pub type CudaResult<T> = Result<T, CudaError>;

impl CudaMiner {
    pub fn is_available() -> bool {
        cuda_available()
    }

    pub fn list_devices() -> CudaResult<Vec<CudaDeviceInfo>> {
        if !cuda_available() {
            return Err(CudaError::NotCompiled);
        }
        cuda_list_devices()
    }

    pub fn new(device_index: i32, workgroups: u32, unit_size: u32) -> CudaResult<Self> {
        if !cuda_available() {
            return Err(CudaError::NotCompiled);
        }
        if workgroups == 0 || unit_size == 0 {
            return Err(CudaError::InvalidArgs(
                "workgroups and unit_size must be > 0".into(),
            ));
        }
        cuda_init_miner(device_index, workgroups, unit_size)
    }

    pub fn device_index(&self) -> i32 {
        self.device
    }

    pub fn workgroups(&self) -> u32 {
        self.workgroups
    }

    pub fn unit_size(&self) -> u32 {
        self.unit_size
    }

    pub fn batch_nonce_space(&self) -> u32 {
        self.workgroups
            .saturating_mul(self.local_size)
            .saturating_mul(self.unit_size)
    }

    /// Mine a batch; returns best nonce + hash (lexicographic max) for the batch.
    pub fn mine_block_batch(
        &self,
        height: u64,
        block_intro: &[u8],
        nonce_start: u32,
        workgroups: u32,
    ) -> CudaResult<(u32, [u8; HASH_BYTES])> {
        if block_intro.len() != STUFF_BYTES {
            return Err(CudaError::InvalidArgs(format!(
                "block_intro must be {} bytes, got {}",
                STUFF_BYTES,
                block_intro.len()
            )));
        }
        let repeat = x16rs::block_hash_repeat(height) as u32;
        cuda_mine_batch(
            self,
            block_intro,
            nonce_start,
            repeat,
            workgroups.min(self.workgroups),
        )
    }

    /// Single-hash helper for tests (genesis vector).
    pub fn block_hash_once(&self, height: u64, block_intro: &[u8]) -> CudaResult<[u8; HASH_BYTES]> {
        if block_intro.len() != STUFF_BYTES {
            return Err(CudaError::InvalidArgs(format!(
                "block_intro must be {} bytes",
                STUFF_BYTES
            )));
        }
        let repeat = x16rs::block_hash_repeat(height) as u32;
        cuda_block_hash_single(self, block_intro, repeat)
    }
}

impl Drop for CudaMiner {
    fn drop(&mut self) {
        if cuda_available() {
            let _ = cuda_free_miner(self);
        }
    }
}

fn cuda_available() -> bool {
    cfg!(cuda_available)
}

#[cfg(cuda_available)]
mod driver {
    use super::*;
    use std::ffi::CStr;
    use std::ptr;

    type CudaError_t = i32;
    const CUDA_SUCCESS: CudaError_t = 0;

    #[link(name = "cudart")]
    unsafe extern "C" {
        fn cudaGetDeviceCount(count: *mut i32) -> CudaError_t;
        fn cudaSetDevice(device: i32) -> CudaError_t;
        fn cudaGetDeviceProperties(prop: *mut CudaDeviceProp, device: i32) -> CudaError_t;
        fn cudaMalloc(ptr: *mut *mut c_void, size: usize) -> CudaError_t;
        fn cudaFree(ptr: *mut c_void) -> CudaError_t;
        fn cudaMemcpy(
            dst: *mut c_void,
            src: *const c_void,
            count: usize,
            kind: i32,
        ) -> CudaError_t;
        fn cudaDeviceSynchronize() -> CudaError_t;
        fn cudaGetErrorString(err: CudaError_t) -> *const i8;
    }

    const CUDA_MEMCPY_HOST_TO_DEVICE: i32 = 1;
    const CUDA_MEMCPY_DEVICE_TO_HOST: i32 = 2;

    #[repr(C)]
    struct CudaDeviceProp {
        name: [i8; 256],
        _pad: [u8; 1024],
        major: i32,
        minor: i32,
        multiProcessorCount: i32,
    }

    unsafe extern "C" {
        fn x16rs_cuda_main(
            input_stuff_89: *const c_void,
            nonce_start: u32,
            x16rs_repeat: u32,
            unit_size: u32,
            global_hashes: *mut c_void,
            global_order: *mut c_void,
            best_hashes: *mut c_void,
            best_nonces: *mut c_void,
        );

        fn x16rs_cuda_single(
            input_stuff_89: *const c_void,
            x16rs_repeat: u32,
            out_hash: *mut c_void,
        );
    }

    fn check(err: CudaError_t) -> CudaResult<()> {
        if err == CUDA_SUCCESS {
            Ok(())
        } else {
            unsafe {
                let cstr = CStr::from_ptr(cudaGetErrorString(err));
                Err(CudaError::Driver(cstr.to_string_lossy().into_owned()))
            }
        }
    }

    pub fn cuda_list_devices() -> CudaResult<Vec<CudaDeviceInfo>> {
        let mut count = 0i32;
        check(unsafe { cudaGetDeviceCount(&mut count) })?;
        let mut out = Vec::new();
        for idx in 0..count {
            let mut prop = CudaDeviceProp {
                name: [0; 256],
                _pad: [0; 1024],
                major: 0,
                minor: 0,
                multiProcessorCount: 0,
            };
            check(unsafe { cudaGetDeviceProperties(&mut prop, idx) })?;
            let name = unsafe { CStr::from_ptr(prop.name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            out.push(CudaDeviceInfo {
                index: idx,
                name,
                compute_major: prop.major,
                compute_minor: prop.minor,
                multiprocessor_count: prop.multiProcessorCount,
            });
        }
        Ok(out)
    }

    pub fn cuda_init_miner(
        device_index: i32,
        workgroups: u32,
        unit_size: u32,
    ) -> CudaResult<CudaMiner> {
        check(unsafe { cudaSetDevice(device_index) })?;
        let local_size = DEFAULT_LOCAL_SIZE;
        let wg = workgroups;
        let mut stuff_buf = ptr::null_mut();
        let mut best_hashes_buf = ptr::null_mut();
        let mut best_nonces_buf = ptr::null_mut();
        let mut global_hashes_buf = ptr::null_mut();
        let mut global_order_buf = ptr::null_mut();

        check(unsafe { cudaMalloc(&mut stuff_buf, STUFF_BYTES) })?;
        check(unsafe {
            cudaMalloc(&mut best_hashes_buf, (wg as usize) * HASH_BYTES)
        })?;
        check(unsafe { cudaMalloc(&mut best_nonces_buf, (wg as usize) * 4) })?;
        let global_slots = (wg as usize) * (local_size as usize) * (unit_size as usize);
        check(unsafe { cudaMalloc(&mut global_hashes_buf, global_slots * HASH_BYTES) })?;
        check(unsafe { cudaMalloc(&mut global_order_buf, global_slots * 4) })?;

        Ok(CudaMiner {
            device: device_index,
            stuff_buf,
            best_hashes_buf,
            best_nonces_buf,
            global_hashes_buf,
            global_order_buf,
            workgroups: wg,
            local_size,
            unit_size,
        })
    }

    pub fn cuda_free_miner(miner: &CudaMiner) -> CudaResult<()> {
        check(unsafe { cudaSetDevice(miner.device) })?;
        unsafe {
            if !miner.stuff_buf.is_null() {
                cudaFree(miner.stuff_buf);
            }
            if !miner.best_hashes_buf.is_null() {
                cudaFree(miner.best_hashes_buf);
            }
            if !miner.best_nonces_buf.is_null() {
                cudaFree(miner.best_nonces_buf);
            }
            if !miner.global_hashes_buf.is_null() {
                cudaFree(miner.global_hashes_buf);
            }
            if !miner.global_order_buf.is_null() {
                cudaFree(miner.global_order_buf);
            }
        }
        Ok(())
    }

    unsafe fn launch_kernel(
        func: *const c_void,
        grid: (u32, u32, u32),
        block: (u32, u32, u32),
        args: &[*mut c_void],
    ) -> CudaResult<()> {
        #[link(name = "cudart")]
        unsafe extern "C" {
            fn cudaLaunchKernel(
                func: *const c_void,
                grid_dim_x: u32,
                grid_dim_y: u32,
                grid_dim_z: u32,
                block_dim_x: u32,
                block_dim_y: u32,
                block_dim_z: u32,
                shared_mem_bytes: usize,
                stream: *mut c_void,
                args: *mut *mut c_void,
                extra: *mut c_void,
            ) -> CudaError_t;
        }

        let mut arg_ptrs = args.to_vec();
        check(unsafe {
            cudaLaunchKernel(
                func,
                grid.0,
                grid.1,
                grid.2,
                block.0,
                block.1,
                block.2,
                0,
                ptr::null_mut(),
                arg_ptrs.as_mut_ptr(),
                ptr::null_mut(),
            )
        })
    }

    pub fn cuda_mine_batch(
        miner: &CudaMiner,
        block_intro: &[u8],
        nonce_start: u32,
        repeat: u32,
        workgroups: u32,
    ) -> CudaResult<(u32, [u8; HASH_BYTES])> {
        check(unsafe { cudaSetDevice(miner.device) })?;
        check(unsafe {
            cudaMemcpy(
                miner.stuff_buf,
                block_intro.as_ptr() as *const c_void,
                STUFF_BYTES,
                CUDA_MEMCPY_HOST_TO_DEVICE,
            )
        })?;

        let mut stuff_ptr = miner.stuff_buf;
        let mut nonce_val = nonce_start;
        let mut repeat_val = repeat;
        let mut unit_val = miner.unit_size;
        let mut hashes_ptr = miner.global_hashes_buf;
        let mut order_ptr = miner.global_order_buf;
        let mut best_hashes_ptr = miner.best_hashes_buf;
        let mut best_nonces_ptr = miner.best_nonces_buf;

        unsafe {
            launch_kernel(
                x16rs_cuda_main as *const c_void,
                (workgroups, 1, 1),
                (miner.local_size, 1, 1),
                &[
                    &mut stuff_ptr as *mut _ as *mut c_void,
                    &mut nonce_val as *mut _ as *mut c_void,
                    &mut repeat_val as *mut _ as *mut c_void,
                    &mut unit_val as *mut _ as *mut c_void,
                    &mut hashes_ptr as *mut _ as *mut c_void,
                    &mut order_ptr as *mut _ as *mut c_void,
                    &mut best_hashes_ptr as *mut _ as *mut c_void,
                    &mut best_nonces_ptr as *mut _ as *mut c_void,
                ],
            )?;
            check(cudaDeviceSynchronize())?;
        }

        let mut hashes = vec![0u8; (workgroups as usize) * HASH_BYTES];
        let mut nonces = vec![0u32; workgroups as usize];
        check(unsafe {
            cudaMemcpy(
                hashes.as_mut_ptr() as *mut c_void,
                miner.best_hashes_buf,
                hashes.len(),
                CUDA_MEMCPY_DEVICE_TO_HOST,
            )
        })?;
        check(unsafe {
            cudaMemcpy(
                nonces.as_mut_ptr() as *mut c_void,
                miner.best_nonces_buf,
                nonces.len() * 4,
                CUDA_MEMCPY_DEVICE_TO_HOST,
            )
        })?;

        let mut best_nonce = 0u32;
        let mut best_hash = [0u8; HASH_BYTES];
        for i in 0..workgroups as usize {
            let hash = &hashes[i * HASH_BYTES..(i + 1) * HASH_BYTES];
            if i == 0 || lex_gt(hash, &best_hash) {
                best_hash.copy_from_slice(hash);
                best_nonce = nonces[i];
            }
        }
        Ok((best_nonce, best_hash))
    }

    pub fn cuda_block_hash_single(
        miner: &CudaMiner,
        block_intro: &[u8],
        repeat: u32,
    ) -> CudaResult<[u8; HASH_BYTES]> {
        check(unsafe { cudaSetDevice(miner.device) })?;
        check(unsafe {
            cudaMemcpy(
                miner.stuff_buf,
                block_intro.as_ptr() as *const c_void,
                STUFF_BYTES,
                CUDA_MEMCPY_HOST_TO_DEVICE,
            )
        })?;
        let mut out = [0u8; HASH_BYTES];
        let mut stuff_ptr = miner.stuff_buf;
        let mut repeat_val = repeat;
        let mut out_ptr = miner.best_hashes_buf;
        unsafe {
            launch_kernel(
                x16rs_cuda_single as *const c_void,
                (1, 1, 1),
                (miner.local_size, 1, 1),
                &[
                    &mut stuff_ptr as *mut _ as *mut c_void,
                    &mut repeat_val as *mut _ as *mut c_void,
                    &mut out_ptr as *mut _ as *mut c_void,
                ],
            )?;
            check(cudaDeviceSynchronize())?;
            check(cudaMemcpy(
                out.as_mut_ptr() as *mut c_void,
                miner.best_hashes_buf,
                HASH_BYTES,
                CUDA_MEMCPY_DEVICE_TO_HOST,
            ))?;
        }
        Ok(out)
    }

    fn lex_gt(a: &[u8], b: &[u8]) -> bool {
        for (x, y) in a.iter().zip(b.iter()) {
            if x > y {
                return true;
            }
            if x < y {
                return false;
            }
        }
        false
    }
}

#[cfg(cuda_available)]
use driver::*;

#[cfg(not(cuda_available))]
fn cuda_list_devices() -> CudaResult<Vec<CudaDeviceInfo>> {
    Err(CudaError::NotCompiled)
}

#[cfg(not(cuda_available))]
fn cuda_init_miner(_: i32, _: u32, _: u32) -> CudaResult<CudaMiner> {
    Err(CudaError::NotCompiled)
}

#[cfg(not(cuda_available))]
fn cuda_free_miner(_: &CudaMiner) -> CudaResult<()> {
    Ok(())
}

#[cfg(not(cuda_available))]
fn cuda_mine_batch(
    _: &CudaMiner,
    _: &[u8],
    _: u32,
    _: u32,
    _: u32,
) -> CudaResult<(u32, [u8; HASH_BYTES])> {
    Err(CudaError::NotCompiled)
}

#[cfg(not(cuda_available))]
fn cuda_block_hash_single(
    _: &CudaMiner,
    _: &[u8],
    _: u32,
) -> CudaResult<[u8; HASH_BYTES]> {
    Err(CudaError::NotCompiled)
}

