//! OpenCL buffers, kernels, and device resource construction.

use std::sync::Mutex;

use crate::gpu_arch::GpuVendor;
use crate::gpu_oom::{GpuBatchError, from_ocl_error};
use ocl::enums::{DeviceInfo, DeviceInfoResult};
use ocl::flags::{CommandQueueProperties, MemFlags};
use ocl::{Buffer, Context, Device, Event, Kernel, Program, Queue};

pub(crate) const HASH_WIDTH: usize = 32;
pub(crate) const STUFF_BUFFER_CAP: usize = 512;

pub(crate) fn pinned_host_write_flags() -> MemFlags {
    MemFlags::new()
        .alloc_host_ptr()
        .read_only()
        .host_write_only()
}

pub(crate) fn pinned_host_read_flags() -> MemFlags {
    MemFlags::new()
        .alloc_host_ptr()
        .write_only()
        .host_read_only()
}

pub(crate) fn create_command_queue(
    context: &Context,
    device: &Device,
) -> std::result::Result<(Queue, bool), String> {
    let ooo = CommandQueueProperties::new().out_of_order();
    match Queue::new(context, device.clone(), Some(ooo)) {
        Ok(queue) => {
            println!("[OpenCL] Out-of-order command queue enabled");
            Ok((queue, true))
        }
        Err(ooo_error) => Queue::new(context, device.clone(), None)
            .map(|queue| {
                println!("[OpenCL] In-order command queue (OOO not supported)");
                (queue, false)
            })
            .map_err(|e| format!("cannot create command queue: {e}; OOO attempt: {ooo_error}")),
    }
}

pub fn write_stuff_to_gpu(
    opencl: &OpenCLResources,
    data: &[u8],
    wait: Option<&Event>,
) -> std::result::Result<Event, String> {
    if data.len() > STUFF_BUFFER_CAP {
        return Err(format!(
            "OpenCL stuff buffer overflow ({} > {})",
            data.len(),
            STUFF_BUFFER_CAP
        ));
    }
    let mut padded = vec![0u8; STUFF_BUFFER_CAP];
    padded[..data.len()].copy_from_slice(data);
    let mut write_event = Event::empty();
    let mut cmd = opencl.buffer_stuff.write(&padded).enew(&mut write_event);
    if let Some(dep) = wait {
        cmd = cmd.ewait(dep);
    }
    cmd.enq()
        .map_err(|e| format!("stuff buffer write: {}", e))?;
    Ok(write_event)
}

pub struct OpenCLResources {
    /// Effective work_groups after VRAM clamp for this device.
    pub workgroups: u32,
    pub platform_index: u32,
    pub device_index: u32,
    pub arch_slug: String,
    pub vram_bytes: u64,
    /// GPU buffers sized for this unit_size (runtime values must not exceed it).
    pub allocated_unitsize: u32,
    pub vendor: GpuVendor,
    diamond: bool,
    pub needs_queue_finish: bool,
    program: Program,
    pub queue: Queue,
    pub buffer_best_nonces: Buffer<u32>,
    pub buffer_best_nonces_diamond: Buffer<u64>,
    buffer_global_hashes: Buffer<u8>,
    buffer_global_order: Buffer<u32>,
    pub buffer_best_hashes: Buffer<u8>,
    /// Reused input buffer — avoids per-kernel GPU allocation.
    buffer_stuff: Buffer<u8>,
    /// Cached OpenCL kernel — rebuilt only when `unit_size` changes.
    kernel_slot: Mutex<KernelSlot>,
}

pub(crate) fn soft_recover_opencl(res: &mut OpenCLResources) {
    if res.needs_queue_finish {
        let _ = res.queue.finish();
    }
    if let Ok(mut slot) = res.kernel_slot.lock() {
        slot.kernel = None;
        slot.unit_size = 0;
    }
}

struct KernelSlot {
    kernel: Option<Kernel>,
    unit_size: u32,
}
pub(crate) fn device_global_mem_bytes(device: &Device) -> u64 {
    match device.info(DeviceInfo::GlobalMemSize) {
        Ok(DeviceInfoResult::GlobalMemSize(v)) => v,
        _ => 0,
    }
}

pub(crate) fn device_compute_units(device: &Device) -> u32 {
    match device.info(DeviceInfo::MaxComputeUnits) {
        Ok(DeviceInfoResult::MaxComputeUnits(v)) => v,
        _ => 0,
    }
}

fn build_block_kernel(
    res: &OpenCLResources,
    unit_size: u32,
) -> std::result::Result<Kernel, String> {
    Kernel::builder()
        .program(&res.program)
        .name("x16rs_main")
        .queue(res.queue.clone())
        .arg(&res.buffer_stuff)
        .arg(0u32)
        .arg(0u32)
        .arg(unit_size)
        .arg(&res.buffer_global_hashes)
        .arg(&res.buffer_global_order)
        .arg(&res.buffer_best_hashes)
        .arg(&res.buffer_best_nonces)
        .build()
        .map_err(|e| format!("kernel build: {}", e))
}

fn build_diamond_kernel(
    res: &OpenCLResources,
    unit_size: u32,
) -> std::result::Result<Kernel, String> {
    Kernel::builder()
        .program(&res.program)
        .name("x16rs_diamond")
        .queue(res.queue.clone())
        .arg(&res.buffer_stuff)
        .arg(0u64)
        .arg(0u32)
        .arg(unit_size)
        .arg(&res.buffer_global_hashes)
        .arg(&res.buffer_global_order)
        .arg(&res.buffer_best_hashes)
        .arg(&res.buffer_best_nonces_diamond)
        .arg(0u32) // stuff_len: 61 or 93
        .build()
        .map_err(|e| format!("kernel build: {}", e))
}

fn run_cached_kernel(
    res: &OpenCLResources,
    unit_size: u32,
    num_work_groups: u32,
    local_work_size: u32,
    wait: Option<&Event>,
    update: impl FnOnce(&mut Kernel) -> std::result::Result<(), String>,
) -> std::result::Result<Event, GpuBatchError> {
    if unit_size > res.allocated_unitsize {
        return Err(GpuBatchError::Other(format!(
            "unit_size {} exceeds allocated buffer size {}",
            unit_size, res.allocated_unitsize
        )));
    }
    if num_work_groups > res.workgroups {
        return Err(GpuBatchError::Other(format!(
            "num_work_groups {} exceeds allocated buffer count {}",
            num_work_groups, res.workgroups
        )));
    }
    let global_work_size = num_work_groups.saturating_mul(local_work_size);
    let mut slot = res
        .kernel_slot
        .lock()
        .map_err(|e| GpuBatchError::Other(e.to_string()))?;
    if slot.kernel.is_none() || slot.unit_size != unit_size {
        let k = if res.diamond {
            build_diamond_kernel(res, unit_size).map_err(|e| GpuBatchError::Other(e))?
        } else {
            build_block_kernel(res, unit_size).map_err(|e| GpuBatchError::Other(e))?
        };
        slot.kernel = Some(k);
        slot.unit_size = unit_size;
    }
    let kernel = slot
        .kernel
        .as_mut()
        .ok_or_else(|| GpuBatchError::Other("kernel cache empty".to_string()))?;
    update(kernel).map_err(|e| GpuBatchError::Other(e))?;
    let mut kernel_event = Event::empty();
    unsafe {
        let mut cmd = kernel
            .cmd()
            .global_work_size(global_work_size)
            .local_work_size(local_work_size)
            .enew(&mut kernel_event);
        if let Some(dep) = wait {
            cmd = cmd.ewait(dep);
        }
        cmd.enq().map_err(|e| from_ocl_error(&e))?;
    }
    Ok(kernel_event)
}

fn wait_event(event: &Event, label: &str) -> std::result::Result<(), String> {
    event
        .wait_for()
        .map_err(|e| format!("{} wait: {}", label, e))
}

pub fn read_block_gpu_results(
    res: &OpenCLResources,
    wait: &Event,
    hashes: &mut [u8],
    nonces: &mut [u32],
) -> std::result::Result<(), String> {
    let mut hash_event = Event::empty();
    let mut nonce_event = Event::empty();
    res.buffer_best_hashes
        .read(hashes)
        .ewait(wait)
        .enew(&mut hash_event)
        .enq()
        .map_err(|e| format!("read hashes enqueue: {}", e))?;
    res.buffer_best_nonces
        .read(nonces)
        .ewait(wait)
        .enew(&mut nonce_event)
        .enq()
        .map_err(|e| format!("read nonces enqueue: {}", e))?;
    wait_event(&hash_event, "hash read")?;
    wait_event(&nonce_event, "nonce read")?;
    Ok(())
}

pub fn read_diamond_gpu_results(
    res: &OpenCLResources,
    wait: &Event,
    hashes: &mut [u8],
    nonces: &mut [u64],
) -> std::result::Result<(), String> {
    let mut hash_event = Event::empty();
    let mut nonce_event = Event::empty();
    res.buffer_best_hashes
        .read(hashes)
        .ewait(wait)
        .enew(&mut hash_event)
        .enq()
        .map_err(|e| format!("read hashes enqueue: {}", e))?;
    res.buffer_best_nonces_diamond
        .read(nonces)
        .ewait(wait)
        .enew(&mut nonce_event)
        .enq()
        .map_err(|e| format!("read nonces enqueue: {}", e))?;
    wait_event(&hash_event, "hash read")?;
    wait_event(&nonce_event, "nonce read")?;
    Ok(())
}

/// Block mining kernel (u32 nonce).
pub fn enqueue_mining_kernel(
    res: &OpenCLResources,
    nonce_start: u32,
    repeat: u32,
    unit_size: u32,
    num_work_groups: u32,
    local_work_size: u32,
    wait: Option<&Event>,
) -> std::result::Result<Event, GpuBatchError> {
    run_cached_kernel(
        res,
        unit_size,
        num_work_groups,
        local_work_size,
        wait,
        |kernel| {
            kernel
                .set_arg(1, nonce_start)
                .map_err(|e| format!("set_arg nonce: {}", e))?;
            kernel
                .set_arg(2, repeat)
                .map_err(|e| format!("set_arg repeat: {}", e))?;
            kernel
                .set_arg(3, unit_size)
                .map_err(|e| format!("set_arg unit_size: {}", e))?;
            Ok(())
        },
    )
}

/// Diamond mining kernel (u64 nonce).
/// `stuff_len` is the prehash byte length (61 without custom message, 93 with).
pub fn enqueue_diamond_kernel(
    res: &OpenCLResources,
    nonce_start: u64,
    repeat: u32,
    unit_size: u32,
    num_work_groups: u32,
    local_work_size: u32,
    stuff_len: u32,
    wait: Option<&Event>,
) -> std::result::Result<Event, GpuBatchError> {
    run_cached_kernel(
        res,
        unit_size,
        num_work_groups,
        local_work_size,
        wait,
        |kernel| {
            kernel
                .set_arg(1, nonce_start)
                .map_err(|e| format!("set_arg nonce: {}", e))?;
            kernel
                .set_arg(2, repeat)
                .map_err(|e| format!("set_arg repeat: {}", e))?;
            kernel
                .set_arg(3, unit_size)
                .map_err(|e| format!("set_arg unit_size: {}", e))?;
            kernel
                .set_arg(8, stuff_len)
                .map_err(|e| format!("set_arg stuff_len: {}", e))?;
            Ok(())
        },
    )
}

fn run_gfx1201_groestl_self_test(res: &OpenCLResources) -> std::result::Result<(), String> {
    const INPUT_HEX: &str = "73710d4acc7ace564b0239839f88c735ad499a667a197974634a52292282fa04";
    const EXPECTED_HEX: &str = "d4f2ebda478be732d5e6efe5b4c6588c7057a781c3bbd8a610fb3534210b6a7f";

    let input = hex::decode(INPUT_HEX).map_err(|e| format!("self-test input decode: {e}"))?;
    let expected =
        hex::decode(EXPECTED_HEX).map_err(|e| format!("self-test expected decode: {e}"))?;
    let input_buffer = Buffer::<u8>::builder()
        .queue(res.queue.clone())
        .flags(ocl::core::MEM_READ_WRITE)
        .len(HASH_WIDTH)
        .build()
        .map_err(|e| format!("self-test input buffer: {e}"))?;
    let output_buffer = Buffer::<u8>::builder()
        .queue(res.queue.clone())
        .flags(ocl::core::MEM_READ_WRITE)
        .len(HASH_WIDTH)
        .build()
        .map_err(|e| format!("self-test output buffer: {e}"))?;

    input_buffer
        .write(&input)
        .enq()
        .map_err(|e| format!("self-test input write: {e}"))?;
    res.queue
        .finish()
        .map_err(|e| format!("self-test input wait: {e}"))?;

    let kernel = Kernel::builder()
        .program(&res.program)
        .name("x16rs_test_groestl2")
        .queue(res.queue.clone())
        .arg(&input_buffer)
        .arg(&output_buffer)
        .build()
        .map_err(|e| format!("self-test kernel build: {e}"))?;
    unsafe {
        kernel
            .cmd()
            .global_work_size(1)
            .local_work_size(1)
            .enq()
            .map_err(|e| format!("self-test kernel enqueue: {e}"))?;
    }
    res.queue
        .finish()
        .map_err(|e| format!("self-test kernel wait: {e}"))?;

    let mut actual = [0u8; HASH_WIDTH];
    output_buffer
        .read(&mut actual[..])
        .enq()
        .map_err(|e| format!("self-test output read: {e}"))?;
    res.queue
        .finish()
        .map_err(|e| format!("self-test output wait: {e}"))?;

    if actual.as_slice() != expected.as_slice() {
        return Err(format!(
            "gfx1201 Groestl integrity self-test failed: gpu={} cpu={}",
            hex::encode(actual),
            EXPECTED_HEX
        ));
    }
    Ok(())
}

pub(crate) fn build_opencl_resources(
    program: &Program,
    queue: &Queue,
    workgroups: u32,
    unitsize: u32,
    global_work_size: u32,
    vendor: GpuVendor,
    vram_bytes: u64,
    diamond: bool,
    out_of_order: bool,
    needs_queue_finish: bool,
    arch_slug: &str,
) -> std::result::Result<OpenCLResources, String> {
    let readback_flags = pinned_host_read_flags();
    let buffer_best_nonces = Buffer::<u32>::builder()
        .queue(queue.clone())
        .flags(readback_flags)
        .len(workgroups as usize)
        .build()
        .map_err(|e| format!("buffer_best_nonces: {}", e))?;
    let buffer_best_nonces_diamond = Buffer::<u64>::builder()
        .queue(queue.clone())
        .flags(readback_flags)
        .len(workgroups as usize)
        .build()
        .map_err(|e| format!("buffer_best_nonces_diamond: {}", e))?;
    let buffer_global_hashes = Buffer::<u8>::builder()
        .queue(queue.clone())
        .flags(ocl::core::MEM_READ_WRITE)
        .len(HASH_WIDTH * unitsize as usize * global_work_size as usize)
        .build()
        .map_err(|e| format!("buffer_global_hashes: {}", e))?;
    let buffer_global_order = Buffer::<u32>::builder()
        .queue(queue.clone())
        .flags(ocl::core::MEM_READ_WRITE)
        .len(unitsize as usize * global_work_size as usize)
        .build()
        .map_err(|e| format!("buffer_global_order: {}", e))?;
    let buffer_best_hashes = Buffer::<u8>::builder()
        .queue(queue.clone())
        .flags(readback_flags)
        .len(HASH_WIDTH * workgroups as usize)
        .build()
        .map_err(|e| format!("buffer_best_hashes: {}", e))?;
    let buffer_stuff = Buffer::<u8>::builder()
        .queue(queue.clone())
        .flags(pinned_host_write_flags())
        .len(STUFF_BUFFER_CAP)
        .build()
        .map_err(|e| format!("buffer_stuff: {}", e))?;
    if out_of_order {
        println!("[OpenCL] Pinned host buffers enabled for stuff + readback");
    }
    let resources = OpenCLResources {
        workgroups,
        platform_index: 0,
        device_index: 0,
        arch_slug: arch_slug.to_string(),
        allocated_unitsize: unitsize,
        vendor,
        vram_bytes,
        diamond,
        needs_queue_finish,
        program: program.clone(),
        queue: queue.clone(),
        buffer_best_nonces,
        buffer_best_nonces_diamond,
        buffer_global_hashes,
        buffer_global_order,
        buffer_best_hashes,
        buffer_stuff,
        kernel_slot: Mutex::new(KernelSlot {
            kernel: None,
            unit_size: 0,
        }),
    };
    if arch_slug == "gfx1201" && !diamond {
        run_gfx1201_groestl_self_test(&resources)?;
        println!("[OpenCL] gfx1201 Groestl integrity self-test passed");
    }
    Ok(resources)
}
