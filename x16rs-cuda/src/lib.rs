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

/// The device allocations one miner instance owns. Kept together behind a mutex
/// inside `CudaMiner` so a sticky-fault recovery can destroy the CUDA context and
/// swap in a freshly allocated set without the caller having to rebuild the miner.
#[cfg_attr(not(cuda_available), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
struct DeviceBuffers {
    stuff: *mut c_void,
    best_hashes: *mut c_void,
    best_nonces: *mut c_void,
    global_hashes: *mut c_void,
    global_order: *mut c_void,
}

#[cfg_attr(not(cuda_available), allow(dead_code))]
impl DeviceBuffers {
    fn null() -> Self {
        DeviceBuffers {
            stuff: std::ptr::null_mut(),
            best_hashes: std::ptr::null_mut(),
            best_nonces: std::ptr::null_mut(),
            global_hashes: std::ptr::null_mut(),
            global_order: std::ptr::null_mut(),
        }
    }

    /// True when any buffer is missing, i.e. the set must not be handed to a kernel.
    fn is_incomplete(&self) -> bool {
        self.stuff.is_null()
            || self.best_hashes.is_null()
            || self.best_nonces.is_null()
            || self.global_hashes.is_null()
            || self.global_order.is_null()
    }
}

#[derive(Debug)]
pub struct CudaMiner {
    device: i32,
    buffers: std::sync::Mutex<DeviceBuffers>,
    /// Sticky-fault context rebuilds not yet followed by a clean batch. Bounds the
    /// automatic recovery so a permanently broken card cannot reset the device in a
    /// tight loop.
    sticky_resets: std::sync::atomic::AtomicU32,
    workgroups: u32,
    local_size: u32,
    unit_size: u32,
}

// Device pointers are owned exclusively and are only reachable through the mutex;
// each launch calls cudaSetDevice first.
unsafe impl Send for CudaMiner {}
unsafe impl Sync for CudaMiner {}

#[derive(Debug)]
pub enum CudaError {
    NotCompiled,
    Driver { code: i32, message: String },
    InvalidArgs(String),
}

/// CUDA runtime error codes that poison the whole device context: once one is
/// raised, every later runtime call on that device returns the same code until the
/// context is destroyed, so shrinking the launch size cannot help - only a
/// cudaDeviceReset plus a full reallocation can. Values are the stable
/// `cudaError_t` enumerants.
const STICKY_CUDA_ERROR_CODES: [i32; 13] = [
    214, // cudaErrorECCUncorrectable
    220, // cudaErrorNvlinkUncorrectable
    700, // cudaErrorIllegalAddress
    702, // cudaErrorLaunchTimeout
    709, // cudaErrorContextIsDestroyed
    710, // cudaErrorAssert
    714, // cudaErrorHardwareStackError
    715, // cudaErrorIllegalInstruction
    716, // cudaErrorMisalignedAddress
    717, // cudaErrorInvalidAddressSpace
    718, // cudaErrorInvalidPc
    719, // cudaErrorLaunchFailure
    999, // cudaErrorUnknown
];

impl CudaError {
    /// Raw `cudaError_t` code of a driver failure, so a caller can tell a per-launch
    /// failure (where the work-group backoff is the right answer) from one that
    /// killed the context.
    pub fn code(&self) -> Option<i32> {
        match self {
            CudaError::Driver { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// True when the fault destroyed the CUDA context, so only a device reset can
    /// bring the card back. Non-sticky failures such as cudaErrorMemoryAllocation
    /// (2), cudaErrorInvalidConfiguration (9) and cudaErrorLaunchOutOfResources
    /// (701) leave the context usable and must NOT trigger a reset.
    pub fn is_sticky(&self) -> bool {
        match self.code() {
            Some(code) => STICKY_CUDA_ERROR_CODES.contains(&code),
            None => false,
        }
    }
}

impl std::fmt::Display for CudaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CudaError::NotCompiled => write!(f, "x16rs-cuda built without CUDA kernels"),
            CudaError::Driver { code, message } => write!(f, "CUDA: {message} (code {code})"),
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
        // cudaDeviceGetAttribute is a stable runtime API: it returns the compute
        // capability and MP count by enum, without depending on the byte offset of
        // those fields inside cudaDeviceProp (which shifts across CUDA versions and
        // yielded bogus values when read via a hardcoded pad). The device NAME is
        // still read from cudaGetDeviceProperties (name is at offset 0, always safe
        // with an oversized struct); cudaDeviceGetName is a DRIVER-API symbol not
        // present in cudart, so it must not be used here.
        fn cudaDeviceGetAttribute(value: *mut i32, attr: i32, device: i32) -> CudaError_t;
        fn cudaMalloc(ptr: *mut *mut c_void, size: usize) -> CudaError_t;
        fn cudaFree(ptr: *mut c_void) -> CudaError_t;
        fn cudaMemcpy(dst: *mut c_void, src: *const c_void, count: usize, kind: i32)
        -> CudaError_t;
        fn cudaDeviceSynchronize() -> CudaError_t;
        // Destroys the primary context of the CURRENT device (and with it every
        // allocation on it). The only way back from a sticky fault short of
        // restarting the process.
        fn cudaDeviceReset() -> CudaError_t;
        fn cudaGetErrorString(err: CudaError_t) -> *const i8;
        fn cudaFuncGetAttributes(attr: *mut CudaFuncAttributes, func: *const c_void)
        -> CudaError_t;
    }

    // Mirrors CUDA's `cudaFuncAttributes` (leading fields only; trailing reserved for
    // forward-compat with newer toolkits). Used to clamp the launch block size to the
    // kernel's own `maxThreadsPerBlock` — a register-heavy kernel can have a per-kernel
    // limit below the device's 1024, and launching above it returns
    // cudaErrorInvalidConfiguration (9).
    #[repr(C)]
    struct CudaFuncAttributes {
        shared_size_bytes: usize,
        const_size_bytes: usize,
        local_size_bytes: usize,
        max_threads_per_block: i32,
        num_regs: i32,
        ptx_version: i32,
        binary_version: i32,
        cache_mode_ca: i32,
        max_dynamic_shared_size_bytes: i32,
        preferred_shmem_carveout: i32,
        // Generous tail so the toolkit's (possibly newer/larger) cudaFuncAttributes
        // never writes past this buffer; we only read the leading fields above.
        _reserved: [i32; 48],
    }

    impl CudaFuncAttributes {
        fn zeroed() -> Self {
            CudaFuncAttributes {
                shared_size_bytes: 0,
                const_size_bytes: 0,
                local_size_bytes: 0,
                max_threads_per_block: 0,
                num_regs: 0,
                ptx_version: 0,
                binary_version: 0,
                cache_mode_ca: 0,
                max_dynamic_shared_size_bytes: 0,
                preferred_shmem_carveout: 0,
                _reserved: [0; 48],
            }
        }
    }

    /// Query a kernel's resource attributes and return a block size clamped to its
    /// `maxThreadsPerBlock` (never above `desired`, never zero).
    unsafe fn clamped_block_size(func: *const c_void, desired: u32, label: &str) -> u32 {
        let mut attrs = CudaFuncAttributes::zeroed();
        let rc = unsafe { cudaFuncGetAttributes(&mut attrs, func) };
        if rc != CUDA_SUCCESS {
            eprintln!(
                "[cuda] cudaFuncGetAttributes({}) failed rc={}; using {}",
                label, rc, desired
            );
            return desired.max(1);
        }
        eprintln!(
            "[cuda] {}: numRegs={} staticShared={}B localPerThread={}B maxThreadsPerBlock={} ptx={} bin={}",
            label,
            attrs.num_regs,
            attrs.shared_size_bytes,
            attrs.local_size_bytes,
            attrs.max_threads_per_block,
            attrs.ptx_version,
            attrs.binary_version,
        );
        let kmax = if attrs.max_threads_per_block > 0 {
            attrs.max_threads_per_block as u32
        } else {
            desired
        };
        desired.min(kmax).max(1)
    }

    const CUDA_MEMCPY_HOST_TO_DEVICE: i32 = 1;
    const CUDA_MEMCPY_DEVICE_TO_HOST: i32 = 2;

    // cudaErrorDeviceUninitialized: reported when a context rebuild left the miner
    // without device buffers, so no launch may be attempted.
    const CUDA_ERROR_DEVICE_UNINITIALIZED: i32 = 201;

    // Stable cudaDeviceAttr enum values (CUDA runtime API).
    const CUDA_DEV_ATTR_MULTIPROCESSOR_COUNT: i32 = 16;
    const CUDA_DEV_ATTR_COMPUTE_CAPABILITY_MAJOR: i32 = 75;
    const CUDA_DEV_ATTR_COMPUTE_CAPABILITY_MINOR: i32 = 76;

    // Oversized tail so cudaGetDeviceProperties (which writes the FULL struct)
    // never overflows across CUDA versions. Only `name` (offset 0) is read from
    // it; compute capability + MP count come from cudaDeviceGetAttribute.
    #[repr(C)]
    struct CudaDeviceProp {
        name: [i8; 256],
        _rest: [u8; 2048],
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
                // Carry the raw code, not just the text, so sticky context faults can
                // be told apart from per-launch failures (see CudaError::is_sticky).
                Err(CudaError::Driver {
                    code: err,
                    message: cstr.to_string_lossy().into_owned(),
                })
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
                _rest: [0; 2048],
            };
            check(unsafe { cudaGetDeviceProperties(&mut prop, idx) })?;
            let name = unsafe { CStr::from_ptr(prop.name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let mut major = 0i32;
            let mut minor = 0i32;
            let mut mp = 0i32;
            check(unsafe {
                cudaDeviceGetAttribute(&mut major, CUDA_DEV_ATTR_COMPUTE_CAPABILITY_MAJOR, idx)
            })?;
            check(unsafe {
                cudaDeviceGetAttribute(&mut minor, CUDA_DEV_ATTR_COMPUTE_CAPABILITY_MINOR, idx)
            })?;
            check(unsafe {
                cudaDeviceGetAttribute(&mut mp, CUDA_DEV_ATTR_MULTIPROCESSOR_COUNT, idx)
            })?;
            out.push(CudaDeviceInfo {
                index: idx,
                name,
                compute_major: major,
                compute_minor: minor,
                multiprocessor_count: mp,
            });
        }
        Ok(out)
    }

    /// Never rebuild the context more than this many times in a row without a clean
    /// batch in between; past that the card is broken, not hiccuping.
    const MAX_STICKY_CONTEXT_RESETS: u32 = 5;

    /// Allocate the full buffer set for one miner instance. On a partial failure
    /// every buffer already obtained is freed, so a failed init (or a failed realloc
    /// after a context rebuild) leaves nothing stranded on the card.
    unsafe fn alloc_device_buffers(
        wg: u32,
        local_size: u32,
        unit_size: u32,
    ) -> CudaResult<DeviceBuffers> {
        let mut bufs = DeviceBuffers::null();
        let global_slots = (wg as usize) * (local_size as usize) * (unit_size as usize);
        let allocated = (|| -> CudaResult<()> {
            check(unsafe { cudaMalloc(&mut bufs.stuff, STUFF_BYTES) })?;
            check(unsafe { cudaMalloc(&mut bufs.best_hashes, (wg as usize) * HASH_BYTES) })?;
            check(unsafe { cudaMalloc(&mut bufs.best_nonces, (wg as usize) * 4) })?;
            check(unsafe { cudaMalloc(&mut bufs.global_hashes, global_slots * HASH_BYTES) })?;
            check(unsafe { cudaMalloc(&mut bufs.global_order, global_slots * 4) })?;
            Ok(())
        })();
        if let Err(e) = allocated {
            unsafe { free_device_buffers(&mut bufs) };
            return Err(e);
        }
        Ok(bufs)
    }

    /// Free every non-null buffer and null the handles, so a second free (Drop after
    /// an explicit teardown) cannot touch a released pointer.
    unsafe fn free_device_buffers(bufs: &mut DeviceBuffers) {
        unsafe {
            if !bufs.stuff.is_null() {
                cudaFree(bufs.stuff);
            }
            if !bufs.best_hashes.is_null() {
                cudaFree(bufs.best_hashes);
            }
            if !bufs.best_nonces.is_null() {
                cudaFree(bufs.best_nonces);
            }
            if !bufs.global_hashes.is_null() {
                cudaFree(bufs.global_hashes);
            }
            if !bufs.global_order.is_null() {
                cudaFree(bufs.global_order);
            }
        }
        *bufs = DeviceBuffers::null();
    }

    /// Lock the buffer set, ignoring poisoning: a panic in another thread must not
    /// take the GPU down for the rest of a 24/7 run - the pointers behind the mutex
    /// are plain handles and cannot be left half-updated.
    fn lock_buffers(miner: &CudaMiner) -> std::sync::MutexGuard<'_, DeviceBuffers> {
        miner.buffers.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn cuda_init_miner(
        device_index: i32,
        workgroups: u32,
        unit_size: u32,
    ) -> CudaResult<CudaMiner> {
        check(unsafe { cudaSetDevice(device_index) })?;
        let local_size = DEFAULT_LOCAL_SIZE;
        let wg = workgroups;

        // The batch kernel's shared `local_nonces[256]` and its power-of-two tree
        // reduction make DEFAULT_LOCAL_SIZE a hard structural requirement: unlike the
        // single-hash path it cannot simply be clamped to the kernel's own
        // maxThreadsPerBlock without corrupting the reduction. So check here that a
        // 256-thread block is launchable and refuse the device loudly if it is not,
        // instead of letting every runtime batch fail with
        // cudaErrorInvalidConfiguration (9) and silently degrade to CPU recovery
        // forever. This also logs the batch kernel's numRegs/shared/maxThreadsPerBlock,
        // the same visibility the single-hash kernel already gets.
        let batch_block =
            unsafe { clamped_block_size(x16rs_cuda_main as *const c_void, local_size, "batch") };
        if batch_block < local_size {
            return Err(CudaError::InvalidArgs(format!(
                "device #{}: x16rs_cuda_main supports only {} threads/block but the batch reduction requires {}; this device/build is unsupported",
                device_index, batch_block, local_size
            )));
        }

        let buffers = unsafe { alloc_device_buffers(wg, local_size, unit_size) }?;

        let miner = CudaMiner {
            device: device_index,
            buffers: std::sync::Mutex::new(buffers),
            sticky_resets: std::sync::atomic::AtomicU32::new(0),
            workgroups: wg,
            local_size,
            unit_size,
        };

        // cudaMalloc succeeding proves nothing about whether the kernel actually
        // launches, so run one real batch before handing the miner out. A card that
        // would fail every batch is rejected at startup, where the caller's
        // no-silent-fallback guard reports it, instead of grinding capped CPU recovery
        // for the life of the process. On the error path `miner` drops here and its
        // Drop frees the buffers.
        if let Err(e) = cuda_self_test(&miner) {
            eprintln!("[cuda] batch kernel self-test failed on device #{device_index}: {e}");
            return Err(e);
        }

        Ok(miner)
    }

    /// One small real batch launch, used at init to prove the kernel runs. It calls
    /// the launch path directly so the sticky-fault auto-recovery does not kick in:
    /// at startup a broken device must be reported, not reset and retried.
    fn cuda_self_test(miner: &CudaMiner) -> CudaResult<()> {
        let stuff = [0u8; STUFF_BYTES];
        let guard = lock_buffers(miner);
        let bufs = *guard;
        unsafe { mine_batch_inner(miner, &bufs, &stuff, 0, 1, 1) }.map(|_| ())
    }

    pub fn cuda_free_miner(miner: &CudaMiner) -> CudaResult<()> {
        // Best effort: if the context is already poisoned cudaSetDevice returns the
        // sticky code, but the frees must still be attempted (and the handles nulled)
        // rather than leaving them dangling behind an early return.
        let _ = check(unsafe { cudaSetDevice(miner.device) });
        let mut bufs = lock_buffers(miner);
        unsafe { free_device_buffers(&mut bufs) };
        Ok(())
    }

    unsafe fn launch_kernel(
        func: *const c_void,
        grid: (u32, u32, u32),
        block: (u32, u32, u32),
        args: &[*mut c_void],
    ) -> CudaResult<()> {
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct Dim3 {
            x: u32,
            y: u32,
            z: u32,
        }
        // RUNTIME API cudaLaunchKernel — real signature:
        //   cudaError_t cudaLaunchKernel(const void*, dim3, dim3, void**, size_t, cudaStream_t)
        // dim3 is passed BY VALUE and `args` comes BEFORE sharedMem/stream. The previous
        // declaration used the DRIVER API cuLaunchKernel layout (grid/block as six u32s,
        // then sharedMem, stream, args, extra) but linked against cudaLaunchKernel. That
        // scrambled the ABI: gridDim.y/z read the high halves of registers holding single
        // u32s -> garbage grid dims -> every launch failed with
        // cudaErrorInvalidConfiguration (9), regardless of block size or shared memory.
        #[link(name = "cudart")]
        unsafe extern "C" {
            fn cudaLaunchKernel(
                func: *const c_void,
                grid_dim: Dim3,
                block_dim: Dim3,
                args: *mut *mut c_void,
                shared_mem: usize,
                stream: *mut c_void,
            ) -> CudaError_t;
        }

        let mut arg_ptrs = args.to_vec();
        check(unsafe {
            cudaLaunchKernel(
                func,
                Dim3 {
                    x: grid.0,
                    y: grid.1,
                    z: grid.2,
                },
                Dim3 {
                    x: block.0,
                    y: block.1,
                    z: block.2,
                },
                arg_ptrs.as_mut_ptr(),
                0,
                ptr::null_mut(),
            )
        })
    }

    /// Rebuild the CUDA context after a sticky fault and reallocate the buffers, so
    /// the next batch runs on a healthy device instead of returning the same poisoned
    /// error forever. Bounded by MAX_STICKY_CONTEXT_RESETS consecutive attempts; the
    /// counter is cleared by the first clean batch.
    unsafe fn recover_sticky_context(miner: &CudaMiner, bufs: &mut DeviceBuffers) {
        use std::sync::atomic::Ordering;
        let attempt = miner.sticky_resets.fetch_add(1, Ordering::Relaxed) + 1;
        if attempt > MAX_STICKY_CONTEXT_RESETS {
            if attempt == MAX_STICKY_CONTEXT_RESETS + 1 {
                eprintln!(
                    "[cuda] ALERT device #{} still faults after {} context rebuilds; GPU mining is OFF until this process restarts - mining continues on CPU recovery only, so check the card (ECC, overclock, driver) now",
                    miner.device, MAX_STICKY_CONTEXT_RESETS
                );
            }
            return;
        }
        eprintln!(
            "[cuda] sticky device fault on #{}; rebuilding the CUDA context (attempt {}/{})",
            miner.device, attempt, MAX_STICKY_CONTEXT_RESETS
        );
        // A sticky fault has already destroyed the context, so cudaFree on the old
        // pointers would only return the same error. cudaDeviceReset tears down the
        // context together with every allocation on it: drop the stale handles first
        // so nothing can be used after the reset, then allocate from scratch.
        *bufs = DeviceBuffers::null();
        let rc = unsafe { cudaDeviceReset() };
        if rc != CUDA_SUCCESS {
            eprintln!(
                "[cuda] cudaDeviceReset on #{} failed rc={}; GPU stays unavailable",
                miner.device, rc
            );
            return;
        }
        if let Err(e) = check(unsafe { cudaSetDevice(miner.device) }) {
            eprintln!("[cuda] re-selecting device #{} after reset failed: {e}", miner.device);
            return;
        }
        match unsafe { alloc_device_buffers(miner.workgroups, miner.local_size, miner.unit_size) } {
            Ok(fresh) => {
                *bufs = fresh;
                eprintln!(
                    "[cuda] device #{} context rebuilt; GPU mining resumes on the next batch",
                    miner.device
                );
            }
            Err(e) => {
                eprintln!(
                    "[cuda] reallocating device #{} buffers after the reset failed: {e}",
                    miner.device
                );
            }
        }
    }

    pub fn cuda_mine_batch(
        miner: &CudaMiner,
        block_intro: &[u8],
        nonce_start: u32,
        repeat: u32,
        workgroups: u32,
    ) -> CudaResult<(u32, [u8; HASH_BYTES])> {
        use std::sync::atomic::Ordering;
        // Hold the buffer lock for the whole batch: a concurrent sticky-fault rebuild
        // must never swap the pointers out from under a running launch.
        let mut bufs = lock_buffers(miner);
        if bufs.is_incomplete() {
            // An earlier rebuild could not reallocate. Retry it (still bounded by the
            // reset budget) rather than launching the kernel against null pointers.
            unsafe { recover_sticky_context(miner, &mut bufs) };
            if bufs.is_incomplete() {
                return Err(CudaError::Driver {
                    code: CUDA_ERROR_DEVICE_UNINITIALIZED,
                    message: format!(
                        "device #{} has no usable buffers after a sticky fault; GPU mining stays disabled",
                        miner.device
                    ),
                });
            }
        }
        let snapshot = *bufs;
        match unsafe {
            mine_batch_inner(miner, &snapshot, block_intro, nonce_start, repeat, workgroups)
        } {
            Ok(best) => {
                miner.sticky_resets.store(0, Ordering::Relaxed);
                Ok(best)
            }
            Err(e) => {
                if e.is_sticky() {
                    unsafe { recover_sticky_context(miner, &mut bufs) };
                }
                Err(e)
            }
        }
    }

    /// The batch launch itself. `bufs` must be a complete set for `miner`.
    unsafe fn mine_batch_inner(
        miner: &CudaMiner,
        bufs: &DeviceBuffers,
        block_intro: &[u8],
        nonce_start: u32,
        repeat: u32,
        workgroups: u32,
    ) -> CudaResult<(u32, [u8; HASH_BYTES])> {
        check(unsafe { cudaSetDevice(miner.device) })?;
        check(unsafe {
            cudaMemcpy(
                bufs.stuff,
                block_intro.as_ptr() as *const c_void,
                STUFF_BYTES,
                CUDA_MEMCPY_HOST_TO_DEVICE,
            )
        })?;

        let mut stuff_ptr = bufs.stuff;
        let mut nonce_val = nonce_start;
        let mut repeat_val = repeat;
        let mut unit_val = miner.unit_size;
        let mut hashes_ptr = bufs.global_hashes;
        let mut order_ptr = bufs.global_order;
        let mut best_hashes_ptr = bufs.best_hashes;
        let mut best_nonces_ptr = bufs.best_nonces;

        // The block size is fixed, NOT clamped like the single-hash path: the kernel's
        // shared local_nonces[256] and its power-of-two tree reduction require exactly
        // DEFAULT_LOCAL_SIZE threads. cuda_init_miner already proved the kernel accepts
        // that block size on this device, so a launch cannot fail on block size here.
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
                bufs.best_hashes,
                hashes.len(),
                CUDA_MEMCPY_DEVICE_TO_HOST,
            )
        })?;
        check(unsafe {
            cudaMemcpy(
                nonces.as_mut_ptr() as *mut c_void,
                bufs.best_nonces,
                nonces.len() * 4,
                CUDA_MEMCPY_DEVICE_TO_HOST,
            )
        })?;

        // Each workgroup's kernel reduction returns the lexicographically SMALLEST hash
        // it found (diff_big_hash keeps the smaller of each pair), because mining wants
        // the hash closest to zero (hash < target). So aggregate across workgroups by
        // keeping the MINIMUM too — replace the running best when the candidate is
        // smaller, i.e. when best > candidate.
        let mut best_nonce = 0u32;
        let mut best_hash = [0u8; HASH_BYTES];
        for i in 0..workgroups as usize {
            let hash = &hashes[i * HASH_BYTES..(i + 1) * HASH_BYTES];
            if i == 0 || lex_gt(&best_hash, hash) {
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
        // Hold the lock for the whole launch so a concurrent rebuild cannot free the
        // buffers this call is using.
        let guard = lock_buffers(miner);
        let bufs = *guard;
        if bufs.is_incomplete() {
            return Err(CudaError::Driver {
                code: CUDA_ERROR_DEVICE_UNINITIALIZED,
                message: format!(
                    "device #{} has no usable buffers after a sticky fault",
                    miner.device
                ),
            });
        }
        check(unsafe { cudaSetDevice(miner.device) })?;
        check(unsafe {
            cudaMemcpy(
                bufs.stuff,
                block_intro.as_ptr() as *const c_void,
                STUFF_BYTES,
                CUDA_MEMCPY_HOST_TO_DEVICE,
            )
        })?;
        let mut out = [0u8; HASH_BYTES];
        let mut stuff_ptr = bufs.stuff;
        let mut repeat_val = repeat;
        let mut out_ptr = bufs.best_hashes;
        // The single-hash kernel does its work on thread 0; the rest only cooperatively
        // fill the shared tables (the fill loop strides by blockDim.x, so any block size
        // is correct). Clamp to the kernel's own maxThreadsPerBlock to avoid
        // cudaErrorInvalidConfiguration on register-heavy builds.
        let block = unsafe {
            clamped_block_size(
                x16rs_cuda_single as *const c_void,
                miner.local_size,
                "single",
            )
        };
        unsafe {
            launch_kernel(
                x16rs_cuda_single as *const c_void,
                (1, 1, 1),
                (block, 1, 1),
                &[
                    &mut stuff_ptr as *mut _ as *mut c_void,
                    &mut repeat_val as *mut _ as *mut c_void,
                    &mut out_ptr as *mut _ as *mut c_void,
                ],
            )?;
            check(cudaDeviceSynchronize())?;
            check(cudaMemcpy(
                out.as_mut_ptr() as *mut c_void,
                bufs.best_hashes,
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
fn cuda_block_hash_single(_: &CudaMiner, _: &[u8], _: u32) -> CudaResult<[u8; HASH_BYTES]> {
    Err(CudaError::NotCompiled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn driver_error(code: i32) -> CudaError {
        CudaError::Driver {
            code,
            message: "test".into(),
        }
    }

    #[test]
    fn sticky_faults_are_recognized() {
        // These poison the CUDA context: every later runtime call returns the same
        // code, so only a device reset can bring the card back. Missing one of them
        // means a 24/7 rig silently mines on capped CPU recovery until restarted.
        for code in [214, 220, 700, 702, 709, 710, 714, 715, 716, 717, 718, 719, 999] {
            assert!(
                driver_error(code).is_sticky(),
                "cuda code {code} must be treated as sticky"
            );
        }
    }

    #[test]
    fn recoverable_faults_are_not_sticky() {
        // cudaErrorMemoryAllocation (2), cudaErrorInvalidConfiguration (9) and
        // cudaErrorLaunchOutOfResources (701) leave the context intact; resetting the
        // device for them would throw away a working context for nothing.
        for code in [2, 9, 701] {
            assert!(
                !driver_error(code).is_sticky(),
                "cuda code {code} must not trigger a context rebuild"
            );
        }
        assert!(!CudaError::NotCompiled.is_sticky());
        assert!(!CudaError::InvalidArgs("bad".into()).is_sticky());
    }

    #[test]
    fn driver_error_carries_the_raw_code() {
        assert_eq!(driver_error(700).code(), Some(700));
        assert_eq!(CudaError::NotCompiled.code(), None);
        assert_eq!(CudaError::InvalidArgs("bad".into()).code(), None);
        // The operator-visible text keeps the driver message and adds the code so a
        // support log identifies the exact fault class.
        assert_eq!(driver_error(700).to_string(), "CUDA: test (code 700)");
    }

    #[test]
    fn buffer_set_is_incomplete_until_every_pointer_is_present() {
        let mut bufs = DeviceBuffers::null();
        assert!(bufs.is_incomplete());
        bufs.stuff = 1usize as *mut c_void;
        bufs.best_hashes = 1usize as *mut c_void;
        bufs.best_nonces = 1usize as *mut c_void;
        bufs.global_hashes = 1usize as *mut c_void;
        assert!(bufs.is_incomplete(), "one missing buffer must still be incomplete");
        bufs.global_order = 1usize as *mut c_void;
        assert!(!bufs.is_incomplete());
    }
}
