//! CUDA block miner for Hacash x16rs PoW.
//!
//! Enable with `--features cuda` and NVIDIA CUDA Toolkit installed (`CUDA_PATH`).

use std::ffi::c_void;

pub const STUFF_BYTES: usize = 89;
pub const HASH_BYTES: usize = 32;
pub const DEFAULT_LOCAL_SIZE: u32 = 256;

/// How many pool shares one CUDA batch can hand back.
///
/// Same number and the same arithmetic as the OpenCL path
/// (`app/src/opencl_gpu/resources.rs`), because the quantity it bounds is a
/// property of the POOL, not of the card: a pool sets `share_bits` so one worker
/// submits on the order of one share a second, so the expected hits per batch is
/// `batch seconds * shares per second`. Even a 0.4 s batch at one share a second
/// is an expected 0.4 hits, three orders of magnitude under 1024. Reaching 1024
/// would take a pool serving a single card thousands of shares a second, which is
/// past its own per-height cap. Overflow here is a misconfigured pool, not a tail
/// event, and the host says so out loud instead of quietly dropping income.
///
/// The cost is 1024 * (4 + 32) bytes = 36 KiB of device memory per miner, next to
/// the hundreds of MiB `global_hashes` already takes, plus at most 1024 CPU
/// re-hashes per batch for the integrity check every entry must pass.
pub const SHARE_LIST_CAPACITY: usize = 1024;

/// Parameters `x16rs_cuda_main` takes: the original eight, plus the five share
/// list arguments appended after `best_nonces`.
///
/// cudaLaunchKernel is handed an untyped `void**`, so nothing in the toolchain
/// checks this against the `.cu`. Typing the launch argument array as
/// `[*mut c_void; X16RS_CUDA_MAIN_ARGS]` at least turns a forgotten entry into a
/// compile error instead of a kernel reading a register that holds something else.
#[cfg_attr(not(cuda_available), allow(dead_code))]
const X16RS_CUDA_MAIN_ARGS: usize = 8 + 5;

/// Everything one CUDA block batch produced.
#[derive(Debug, Clone)]
pub struct CudaBatchOutput {
    /// The batch's single strongest nonce/hash, from the work-group tree
    /// reduction. Unchanged, and still the only thing solo mining reads.
    pub best: (u32, [u8; HASH_BYTES]),
    /// Every nonce whose hash beat the share target, up to `SHARE_LIST_CAPACITY`.
    /// Always empty when the caller passed no share target.
    pub shares: Vec<(u32, [u8; HASH_BYTES])>,
    /// How many hits the kernel counted in TOTAL, including the ones that did not
    /// fit in `shares`. Greater than `shares.len()` means the batch is
    /// undersampling and the miner is earning less than it mined.
    pub share_hits: u64,
}

/// The `share_capacity` a batch is launched with.
///
/// This is the whole solo guarantee in one place: no share target means 0, 0 makes
/// the kernel skip the appending block entirely, and the host neither uploads a
/// target nor reads a share buffer back.
pub fn share_capacity_for(share_target: Option<&[u8; HASH_BYTES]>) -> u32 {
    match share_target {
        Some(_) => SHARE_LIST_CAPACITY as u32,
        None => 0,
    }
}

/// How many entries of the fixed-size list a counter reading makes live.
///
/// The kernel's counter is the TOTAL number of hits, so it can exceed the
/// capacity; reading past the capacity would hand back uninitialized device
/// memory as if it were mined shares.
pub fn stored_share_count(hits: u64) -> usize {
    hits.min(SHARE_LIST_CAPACITY as u64) as usize
}

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
    /// Share target the kernel appends against (32 bytes). Only written on a POOL
    /// batch; a solo batch never touches it.
    share_target: *mut c_void,
    /// Single atomic counter: how many nonces beat the share target in this batch,
    /// INCLUDING the ones that did not fit in the list below.
    share_found: *mut c_void,
    share_nonces: *mut c_void,
    share_hashes: *mut c_void,
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
            share_target: std::ptr::null_mut(),
            share_found: std::ptr::null_mut(),
            share_nonces: std::ptr::null_mut(),
            share_hashes: std::ptr::null_mut(),
        }
    }

    /// True when any buffer is missing, i.e. the set must not be handed to a kernel.
    ///
    /// The share buffers count even for a solo miner: the kernel takes them as
    /// arguments on every launch, and a null pointer bound to a kernel argument is
    /// exactly the kind of thing that works until the day someone passes a non-zero
    /// capacity.
    fn is_incomplete(&self) -> bool {
        self.stuff.is_null()
            || self.best_hashes.is_null()
            || self.best_nonces.is_null()
            || self.global_hashes.is_null()
            || self.global_order.is_null()
            || self.share_target.is_null()
            || self.share_found.is_null()
            || self.share_nonces.is_null()
            || self.share_hashes.is_null()
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

    /// Mine a batch; returns the best nonce + hash for the batch.
    ///
    /// Kept exactly as it was for the solo, benchmark and auto-tune callers, which
    /// want one result per batch and nothing else. Pool mining goes through
    /// [`CudaMiner::mine_block_batch_shares`].
    pub fn mine_block_batch(
        &self,
        height: u64,
        block_intro: &[u8],
        nonce_start: u32,
        workgroups: u32,
    ) -> CudaResult<(u32, [u8; HASH_BYTES])> {
        self.mine_block_batch_shares(height, block_intro, nonce_start, workgroups, None)
            .map(|out| out.best)
    }

    /// Mine a batch and, when `share_target` is set, hand back every nonce whose
    /// hash beat it, not only the strongest one.
    ///
    /// `share_target` is `None` for solo mining, and that is the whole guarantee
    /// that solo behaviour is untouched: the kernel is launched with
    /// share_capacity=0, which skips the appending block, and the host moves not one
    /// extra byte over the PCIe bus in either direction.
    pub fn mine_block_batch_shares(
        &self,
        height: u64,
        block_intro: &[u8],
        nonce_start: u32,
        workgroups: u32,
        share_target: Option<&[u8; HASH_BYTES]>,
    ) -> CudaResult<CudaBatchOutput> {
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
            share_target,
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

    // These declarations exist to take the ADDRESS of each kernel's host-side stub
    // for cudaLaunchKernel; the arguments themselves travel through the untyped
    // `void**` array built in mine_batch_inner. Keep the parameter list in step with
    // cuda/block_miner.cu anyway: it is the only place a reader can check the order
    // and the widths against the launch array, and the compiler will not do it.
    //
    // Widths, checked against block_miner.cu one by one: the four pointers before
    // the share list and the four share pointers are all 64-bit device addresses on
    // every supported target, `unsigned int` on the device side is 32 bits so
    // nonce_start / x16rs_repeat / unit_size / share_capacity are u32, and
    // share_capacity is LAST, after the four share pointers, exactly as in the .cu
    // and in the OpenCL model kernel.
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
            share_target: *const c_void,
            share_found: *mut c_void,
            share_nonces: *mut c_void,
            share_hashes: *mut c_void,
            share_capacity: u32,
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
            // 36 KiB and change, allocated once whether or not this miner ever pools.
            // Allocating them lazily would mean reallocating on the first pool batch,
            // i.e. a cudaMalloc on the hot path and one more state to get wrong; the
            // solo guarantee is share_capacity=0, not a missing allocation.
            check(unsafe { cudaMalloc(&mut bufs.share_target, HASH_BYTES) })?;
            check(unsafe { cudaMalloc(&mut bufs.share_found, 4) })?;
            check(unsafe { cudaMalloc(&mut bufs.share_nonces, SHARE_LIST_CAPACITY * 4) })?;
            check(unsafe { cudaMalloc(&mut bufs.share_hashes, SHARE_LIST_CAPACITY * HASH_BYTES) })?;
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
            if !bufs.share_target.is_null() {
                cudaFree(bufs.share_target);
            }
            if !bufs.share_found.is_null() {
                cudaFree(bufs.share_found);
            }
            if !bufs.share_nonces.is_null() {
                cudaFree(bufs.share_nonces);
            }
            if !bufs.share_hashes.is_null() {
                cudaFree(bufs.share_hashes);
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
        // Solo shape (no share target), because this proves the launch works; the
        // share list is an output path bolted beside it and adds nothing a launch
        // failure would show up in.
        unsafe { mine_batch_inner(miner, &bufs, &stuff, 0, 1, 1, None) }.map(|_| ())
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
            eprintln!(
                "[cuda] re-selecting device #{} after reset failed: {e}",
                miner.device
            );
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
        share_target: Option<&[u8; HASH_BYTES]>,
    ) -> CudaResult<CudaBatchOutput> {
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
            mine_batch_inner(
                miner,
                &snapshot,
                block_intro,
                nonce_start,
                repeat,
                workgroups,
                share_target,
            )
        } {
            Ok(output) => {
                miner.sticky_resets.store(0, Ordering::Relaxed);
                Ok(output)
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
        share_target: Option<&[u8; HASH_BYTES]>,
    ) -> CudaResult<CudaBatchOutput> {
        check(unsafe { cudaSetDevice(miner.device) })?;
        check(unsafe {
            cudaMemcpy(
                bufs.stuff,
                block_intro.as_ptr() as *const c_void,
                STUFF_BYTES,
                CUDA_MEMCPY_HOST_TO_DEVICE,
            )
        })?;

        // Solo is 0 here and never enters either branch below, so a solo batch does
        // exactly the two transfers it always did: the 89-byte intro up, the best
        // hash and nonce back.
        let share_capacity = share_capacity_for(share_target);
        if let Some(target) = share_target {
            check(unsafe {
                cudaMemcpy(
                    bufs.share_target,
                    target.as_ptr() as *const c_void,
                    HASH_BYTES,
                    CUDA_MEMCPY_HOST_TO_DEVICE,
                )
            })?;
            // The counter has to start this batch at zero, or the first slot of the
            // list would be written past the end of the previous batch's entries and
            // the overflow report would be the sum of every batch so far.
            let zero = [0u32; 1];
            check(unsafe {
                cudaMemcpy(
                    bufs.share_found,
                    zero.as_ptr() as *const c_void,
                    4,
                    CUDA_MEMCPY_HOST_TO_DEVICE,
                )
            })?;
        }

        let mut stuff_ptr = bufs.stuff;
        let mut nonce_val = nonce_start;
        let mut repeat_val = repeat;
        let mut unit_val = miner.unit_size;
        let mut hashes_ptr = bufs.global_hashes;
        let mut order_ptr = bufs.global_order;
        let mut best_hashes_ptr = bufs.best_hashes;
        let mut best_nonces_ptr = bufs.best_nonces;
        let mut share_target_ptr = bufs.share_target;
        let mut share_found_ptr = bufs.share_found;
        let mut share_nonces_ptr = bufs.share_nonces;
        let mut share_hashes_ptr = bufs.share_hashes;
        let mut share_capacity_val = share_capacity;

        // One entry per kernel parameter, in declaration order, each entry a pointer
        // to the VALUE being passed (so a pointer argument is a pointer to the device
        // pointer variable above, and a u32 argument is a pointer to that u32). The
        // fixed length is the only automatic check there is that all thirteen are
        // present: cudaLaunchKernel takes void** and would happily read whatever
        // follows a short array.
        let args: [*mut c_void; X16RS_CUDA_MAIN_ARGS] = [
            &mut stuff_ptr as *mut _ as *mut c_void,
            &mut nonce_val as *mut _ as *mut c_void,
            &mut repeat_val as *mut _ as *mut c_void,
            &mut unit_val as *mut _ as *mut c_void,
            &mut hashes_ptr as *mut _ as *mut c_void,
            &mut order_ptr as *mut _ as *mut c_void,
            &mut best_hashes_ptr as *mut _ as *mut c_void,
            &mut best_nonces_ptr as *mut _ as *mut c_void,
            &mut share_target_ptr as *mut _ as *mut c_void,
            &mut share_found_ptr as *mut _ as *mut c_void,
            &mut share_nonces_ptr as *mut _ as *mut c_void,
            &mut share_hashes_ptr as *mut _ as *mut c_void,
            &mut share_capacity_val as *mut _ as *mut c_void,
        ];

        // The block size is fixed, NOT clamped like the single-hash path: the kernel's
        // shared local_nonces[256] and its power-of-two tree reduction require exactly
        // DEFAULT_LOCAL_SIZE threads. cuda_init_miner already proved the kernel accepts
        // that block size on this device, so a launch cannot fail on block size here.
        unsafe {
            launch_kernel(
                x16rs_cuda_main as *const c_void,
                (workgroups, 1, 1),
                (miner.local_size, 1, 1),
                &args,
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

        let (shares, share_hits) = if share_capacity == 0 {
            (Vec::new(), 0u64)
        } else {
            unsafe { read_share_list(bufs) }?
        };

        Ok(CudaBatchOutput {
            best: (best_nonce, best_hash),
            shares,
            share_hits,
        })
    }

    /// Read how many nonces beat the share target, then the entries that fit.
    ///
    /// The counter comes back first because it decides how much of the list is live:
    /// pulling the whole 36 KiB every batch would be pure PCIe traffic for a batch
    /// that found nothing, and reading past the counter would hand back stale or
    /// uninitialized device memory dressed up as mined shares. The returned total is
    /// the kernel's raw count, so a value above the capacity is exactly the
    /// undersampling signal the host has to report.
    unsafe fn read_share_list(
        bufs: &DeviceBuffers,
    ) -> CudaResult<(Vec<(u32, [u8; HASH_BYTES])>, u64)> {
        let mut found = [0u32; 1];
        check(unsafe {
            cudaMemcpy(
                found.as_mut_ptr() as *mut c_void,
                bufs.share_found,
                4,
                CUDA_MEMCPY_DEVICE_TO_HOST,
            )
        })?;
        let total = found[0] as u64;
        let stored = stored_share_count(total);
        if stored == 0 {
            return Ok((Vec::new(), total));
        }

        let mut nonces = vec![0u32; stored];
        let mut hashes = vec![0u8; stored * HASH_BYTES];
        check(unsafe {
            cudaMemcpy(
                nonces.as_mut_ptr() as *mut c_void,
                bufs.share_nonces,
                stored * 4,
                CUDA_MEMCPY_DEVICE_TO_HOST,
            )
        })?;
        check(unsafe {
            cudaMemcpy(
                hashes.as_mut_ptr() as *mut c_void,
                bufs.share_hashes,
                stored * HASH_BYTES,
                CUDA_MEMCPY_DEVICE_TO_HOST,
            )
        })?;

        let mut shares = Vec::with_capacity(stored);
        for (i, nonce) in nonces.iter().enumerate() {
            let mut hash = [0u8; HASH_BYTES];
            hash.copy_from_slice(&hashes[i * HASH_BYTES..(i + 1) * HASH_BYTES]);
            shares.push((*nonce, hash));
        }
        Ok((shares, total))
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
    _: Option<&[u8; HASH_BYTES]>,
) -> CudaResult<CudaBatchOutput> {
    Err(CudaError::NotCompiled)
}

#[cfg(not(cuda_available))]
fn cuda_block_hash_single(_: &CudaMiner, _: &[u8], _: u32) -> CudaResult<[u8; HASH_BYTES]> {
    Err(CudaError::NotCompiled)
}

/// On-device cross-check of the pool share list against the CPU.
///
/// The CUDA equivalent of the OpenCL integration test in
/// `app/src/opencl_gpu/block.rs`, and the only thing that can prove the ported
/// kernel on real silicon: everything in `mod tests` below is host arithmetic.
/// Runs wherever the kernels were compiled AND a device answers; skips loudly
/// otherwise, so `cargo test -p x16rs-cuda --features cuda` on a build machine
/// without a card still reports honestly instead of failing.
#[cfg(all(test, cuda_available))]
mod gpu_share_list_tests {
    use super::*;

    /// Mainnet repeat is 16 from height 750000 on, so the kernel under test runs
    /// the same algorithm mix a real block does.
    const REPEAT16_HEIGHT: u64 = 800_000;
    const WORKGROUPS: u32 = 2;
    const UNIT_SIZE: u32 = 3;
    const BATCH_NONCES: u32 = WORKGROUPS * DEFAULT_LOCAL_SIZE * UNIT_SIZE;
    const NONCE_START: u32 = 4_000;

    /// Genesis intro, reused because it is a real 89-byte block intro: the kernel
    /// folds the message padding into a constant that hard-codes byte 88 as 0x00
    /// (a real intro ends in the two zero bytes of its transaction count), so an
    /// invented intro would make the card and the CPU hash different messages.
    const GENESIS_INTRO: &str = "010000000000005c57b08c0000000000000000000000000000000000000000000000000000000000000000ad557702fc70afaf70a855e7b8a4400159643cb5a7fc8a89ba2bce6f818a9b0100000001098b3445000000000000";

    /// The kernel writes the nonce big-endian at byte offset 79, so the CPU
    /// reference has to hash exactly those bytes.
    fn cpu_hash(intro: &[u8], nonce: u32) -> [u8; HASH_BYTES] {
        let mut stuff = intro.to_vec();
        stuff[79..83].copy_from_slice(&nonce.to_be_bytes());
        x16rs::block_hash(REPEAT16_HEIGHT, &stuff)
    }

    #[test]
    fn the_share_list_matches_the_cpu_and_leaves_the_best_result_untouched() {
        let miner = match CudaMiner::new(0, WORKGROUPS, UNIT_SIZE) {
            Ok(miner) => miner,
            Err(e) => {
                eprintln!(
                    "no usable CUDA device ({e}); skipping the on-device share list cross-check"
                );
                return;
            }
        };
        let intro = hex::decode(GENESIS_INTRO).expect("genesis intro hex");
        assert_eq!(intro.len(), STUFF_BYTES);

        // What the CPU says about this exact window, which is the authority.
        let mut cpu: Vec<(u32, [u8; HASH_BYTES])> = (NONCE_START..NONCE_START + BATCH_NONCES)
            .map(|nonce| (nonce, cpu_hash(&intro, nonce)))
            .collect();
        let cpu_best = *cpu
            .iter()
            .min_by(|a, b| a.1.cmp(&b.1))
            .expect("non empty window");

        // 1. SOLO: no share target, so the kernel skips the whole added block and
        //    the single best result has to be the CPU's, byte for byte.
        let solo = miner
            .mine_block_batch_shares(REPEAT16_HEIGHT, &intro, NONCE_START, WORKGROUPS, None)
            .expect("solo batch");
        assert_eq!(
            solo.best.1,
            cpu_hash(&intro, solo.best.0),
            "the hash the card reports for its own nonce must be the CPU's"
        );
        assert_eq!(solo.best, cpu_best, "solo best must match the CPU exactly");
        assert!(solo.shares.is_empty(), "solo must never build a share list");
        assert_eq!(solo.share_hits, 0);

        // 2. POOL, easiest possible target: every nonce is payable, so the counter
        //    sees the whole window and the list reports the overflow instead of
        //    quietly capping. BATCH_NONCES is deliberately above the capacity.
        assert!(BATCH_NONCES as usize > SHARE_LIST_CAPACITY);
        let pool = miner
            .mine_block_batch_shares(
                REPEAT16_HEIGHT,
                &intro,
                NONCE_START,
                WORKGROUPS,
                Some(&[0xffu8; HASH_BYTES]),
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
        assert_eq!(pool.shares.len(), SHARE_LIST_CAPACITY);
        assert_eq!(stored_share_count(pool.share_hits), pool.shares.len());
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
        let strict = miner
            .mine_block_batch_shares(
                REPEAT16_HEIGHT,
                &intro,
                NONCE_START,
                WORKGROUPS,
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

        // 4. And back to solo on the SAME miner: the counter is cleared per batch,
        //    so a pool batch cannot leak its hits into the next solo one.
        let solo_again = miner
            .mine_block_batch_shares(REPEAT16_HEIGHT, &intro, NONCE_START, WORKGROUPS, None)
            .expect("second solo batch");
        assert_eq!(solo_again.best, cpu_best);
        assert!(solo_again.shares.is_empty());
        assert_eq!(solo_again.share_hits, 0);
    }
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
        for code in [
            214, 220, 700, 702, 709, 710, 714, 715, 716, 717, 718, 719, 999,
        ] {
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
        // Every field, one at a time: a buffer added to the struct but forgotten in
        // the completeness check would let a launch run against a null pointer.
        let full = || {
            let mut bufs = DeviceBuffers::null();
            bufs.stuff = 1usize as *mut c_void;
            bufs.best_hashes = 1usize as *mut c_void;
            bufs.best_nonces = 1usize as *mut c_void;
            bufs.global_hashes = 1usize as *mut c_void;
            bufs.global_order = 1usize as *mut c_void;
            bufs.share_target = 1usize as *mut c_void;
            bufs.share_found = 1usize as *mut c_void;
            bufs.share_nonces = 1usize as *mut c_void;
            bufs.share_hashes = 1usize as *mut c_void;
            bufs
        };
        assert!(DeviceBuffers::null().is_incomplete());
        assert!(!full().is_incomplete());

        let holes: [fn(&mut DeviceBuffers); 9] = [
            |b| b.stuff = std::ptr::null_mut(),
            |b| b.best_hashes = std::ptr::null_mut(),
            |b| b.best_nonces = std::ptr::null_mut(),
            |b| b.global_hashes = std::ptr::null_mut(),
            |b| b.global_order = std::ptr::null_mut(),
            |b| b.share_target = std::ptr::null_mut(),
            |b| b.share_found = std::ptr::null_mut(),
            |b| b.share_nonces = std::ptr::null_mut(),
            |b| b.share_hashes = std::ptr::null_mut(),
        ];
        for (i, punch) in holes.iter().enumerate() {
            let mut bufs = full();
            punch(&mut bufs);
            assert!(
                bufs.is_incomplete(),
                "buffer #{i} missing must make the set incomplete"
            );
        }
    }

    #[test]
    fn solo_launches_the_kernel_with_a_zero_share_capacity() {
        // This is the entire solo guarantee, and it is one comparison: no share
        // target means capacity 0, capacity 0 makes the kernel skip the appending
        // block, and mine_batch_inner then skips the target upload, the counter
        // clear and the whole readback. A solo batch moves the same bytes it always
        // did.
        assert_eq!(share_capacity_for(None), 0);
        assert_eq!(
            share_capacity_for(Some(&[0xffu8; HASH_BYTES])),
            SHARE_LIST_CAPACITY as u32
        );
        assert_eq!(
            share_capacity_for(Some(&[0u8; HASH_BYTES])),
            SHARE_LIST_CAPACITY as u32,
            "the hardest possible target still opens the list; only None closes it"
        );
    }

    #[test]
    fn the_readback_never_reads_past_the_counter_or_the_capacity() {
        // The kernel counter is the TOTAL, so it can exceed the list. Reading
        // `total` entries would copy uninitialized device memory back and offer it
        // to the pool as mined shares.
        assert_eq!(stored_share_count(0), 0);
        assert_eq!(stored_share_count(1), 1);
        assert_eq!(
            stored_share_count(SHARE_LIST_CAPACITY as u64 - 1),
            SHARE_LIST_CAPACITY - 1
        );
        assert_eq!(
            stored_share_count(SHARE_LIST_CAPACITY as u64),
            SHARE_LIST_CAPACITY
        );
        assert_eq!(
            stored_share_count(SHARE_LIST_CAPACITY as u64 + 1),
            SHARE_LIST_CAPACITY
        );
        assert_eq!(stored_share_count(9_000), SHARE_LIST_CAPACITY);
        assert_eq!(stored_share_count(u64::MAX), SHARE_LIST_CAPACITY);
    }

    #[test]
    fn overflow_is_the_counter_minus_what_was_stored() {
        // What the host reports as lost income. The counter must be the total, not
        // the stored count, or an undersampling batch would look perfectly healthy.
        for (hits, want_stored, want_dropped) in [
            (0u64, 0usize, 0u64),
            (7, 7, 0),
            (SHARE_LIST_CAPACITY as u64, SHARE_LIST_CAPACITY, 0),
            (SHARE_LIST_CAPACITY as u64 + 1, SHARE_LIST_CAPACITY, 1),
            (
                9_000,
                SHARE_LIST_CAPACITY,
                9_000 - SHARE_LIST_CAPACITY as u64,
            ),
        ] {
            let stored = stored_share_count(hits);
            assert_eq!(stored, want_stored, "stored count for {hits} hits");
            assert_eq!(
                hits.saturating_sub(stored as u64),
                want_dropped,
                "dropped count for {hits} hits"
            );
        }
    }

    #[test]
    fn the_launch_argument_array_matches_the_kernel_signature() {
        // cudaLaunchKernel takes an untyped void**, so nothing in the toolchain
        // compares the launch array against cuda/block_miner.cu. This constant is
        // what types that array, so pin it against the .cu parameter list: eight
        // original parameters, then share_target, share_found, share_nonces,
        // share_hashes, share_capacity. Change one and this test says read the .cu.
        assert_eq!(X16RS_CUDA_MAIN_ARGS, 13);
    }

    #[test]
    fn the_share_capacity_fits_in_the_kernels_unsigned_int() {
        // share_capacity crosses the FFI as a u32 and is compared against a u32 slot
        // index on the device. A capacity that did not fit would wrap and turn the
        // bounds check into a way to write off the end of the list.
        assert!(SHARE_LIST_CAPACITY as u64 <= u32::MAX as u64);
        assert_eq!(
            share_capacity_for(Some(&[0u8; HASH_BYTES])) as usize,
            SHARE_LIST_CAPACITY
        );
    }
}
