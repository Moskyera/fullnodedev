use std::ffi::CString;
use std::path::Path;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::sync::Mutex;
use crate::gpu_arch::{self, GpuVendor};
use ocl::enums::{DeviceInfo, DeviceInfoResult, ProgramInfoResult, ProgramInfo};
use ocl::flags::{CommandQueueProperties, MemFlags};
use ocl::{Buffer, Context, Device, Event, Kernel, Platform, Program, Queue};

#[allow(dead_code)]
const STUFF_BUFFER_CAP: usize = 512;

fn pinned_host_write_flags() -> MemFlags {
    MemFlags::new()
        .alloc_host_ptr()
        .read_only()
        .host_write_only()
}

fn pinned_host_read_flags() -> MemFlags {
    MemFlags::new()
        .alloc_host_ptr()
        .write_only()
        .host_read_only()
}

fn create_command_queue(context: &Context, device: &Device) -> (Queue, bool) {
    let ooo = CommandQueueProperties::new().out_of_order();
    match Queue::new(context, device.clone(), Some(ooo)) {
        Ok(queue) => {
            println!("[OpenCL] Out-of-order command queue enabled");
            (queue, true)
        }
        Err(_) => {
            let queue = Queue::new(context, device.clone(), None)
                .expect("Can't create OpenCL event queue");
            println!("[OpenCL] In-order command queue (OOO not supported)");
            (queue, false)
        }
    }
}

fn write_stuff_to_gpu(
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
    let mut cmd = opencl
        .buffer_stuff
        .write(&padded)
        .enew(&mut write_event);
    if let Some(dep) = wait {
        cmd = cmd.ewait(dep);
    }
    cmd.enq()
        .map_err(|e| format!("stuff buffer write: {}", e))?;
    Ok(write_event)
}

struct OpenCLResources {
    /// Effective work_groups after VRAM clamp for this device.
    workgroups: u32,
    /// GPU buffers sized for this unit_size (runtime values must not exceed it).
    allocated_unitsize: u32,
    vendor: GpuVendor,
    compute_units: u32,
    vram_bytes: u64,
    diamond: bool,
    out_of_order: bool,
    program: Program,
    queue: Queue,
    buffer_best_nonces: Buffer::<u32>,
    buffer_best_nonces_diamond: Buffer::<u64>,
    buffer_global_hashes: Buffer::<u8>,
    buffer_global_order: Buffer::<u32>,
    buffer_best_hashes: Buffer::<u8>,
    /// Reused input buffer — avoids per-kernel GPU allocation.
    buffer_stuff: Buffer::<u8>,
    /// Cached OpenCL kernel — rebuilt only when `unit_size` changes.
    kernel_slot: Mutex<KernelSlot>,
}

struct KernelSlot {
    kernel: Option<Kernel>,
    unit_size: u32,
}

fn initialize_opencl(
    diamond_mining: bool,
    opencldir: &String,
    platformid: &u32,
    deviceids: &String,
    workgroups: &u32,
    localsize: &u32,
    unitsize: &u32,
) -> Vec<OpenCLResources> {
    if *localsize != 256 {
        eprintln!(
            "[Warn] OpenCL local_size={} is incompatible with kernel fixed local arrays(256), fallback to CPU miner.",
            localsize
        );
        return Vec::new();
    }

    // Binary file location
    let kernel_file = if diamond_mining { format!(r"{}x16rs_diamond.cl", opencldir) } else { format!(r"{}x16rs_main.cl", opencldir) };
    let kernel_path = Path::new(&kernel_file);

    // Context creation for OpenCL instance
    let platforms = Platform::list();
    let platform = platforms
        .get(*platformid as usize)
        .expect("The specified platform id is invalid")
        .clone();

    let name = platform.name().expect("Error");
    let vendor = platform.vendor().expect("Error");
    let version: String = platform.version().expect("Error");
    println!("Platform name: {}", name);
    println!("Manufacturer: {}", vendor);
    println!("Version: {}", version);

    let mut cnf_devices: Vec<u32> = deviceids.split(',')
        .filter(|s| !s.trim().is_empty())
        .filter_map(|s| s.trim().parse::<u32>().ok())
        .collect();

    // Set all devices when empty
    if cnf_devices.is_empty() {
        let platform_devices = Device::list_all(&platform).expect("Error getting device list");
        // Iterate all OpenCL devices
        for (idx, _) in platform_devices.iter().enumerate() {
            cnf_devices.push(idx as u32);
        }
    }

    // Create Device vector
    let mut devices: Vec<Device> = [].to_vec();
    for (_, &device_id) in cnf_devices.iter().enumerate() {
        let device = Device::by_idx_wrap(platform, device_id.try_into().unwrap()).expect("Can't find OpenCL device");
        devices.push(device);
    }

    let mut opencl_resource_devices = Vec::with_capacity(devices.len() as usize);
    for (idx, &device) in devices.iter().enumerate() {
        let device_name = device.name().expect("Can't get device name");
        let device_vendor = device.vendor().unwrap_or_default();
        let vendor = gpu_arch::detect_vendor(&device_vendor, &device_name);
        let vram_bytes = device_global_mem_bytes(&device);
        let compute_units = device_compute_units(&device);
        let mut wg = gpu_arch::suggest_workgroups(*workgroups, compute_units, vendor);
        if compute_units > 0 {
            println!(
                "[OpenCL] CU={} suggested work_groups={} (config {})",
                compute_units, wg, workgroups
            );
        }
        if vram_bytes > 0 {
            let clamped = clamp_workgroups_for_vram(vram_bytes, *localsize, *unitsize, wg);
            if clamped < wg {
                println!(
                    "[efficiency] VRAM clamp: work_groups {} -> {} ({} MB available)",
                    wg,
                    clamped,
                    vram_bytes / (1024 * 1024)
                );
                wg = clamped;
            }
        }
        let num_work_items = wg * localsize;
        let global_work_size = num_work_items;
        
        println!("-----------------------------------------");
        println!("Device {}: {}", cnf_devices[idx], device_name);
        println!("-----------------------------------------");
        
        // Create context
        let context = Context::builder()
            .platform(platform)
            .devices(device)
            .build()
            .expect("Can't create OpenCL context");

        if !Path::new(&opencldir).is_dir() {
            panic!("OpenCL dir not found: {}", opencldir);
        }

        let slug = gpu_arch::arch_slug(&device_name);
        let amd_fast = vendor == GpuVendor::Amd;
        if amd_fast {
            println!("AMD fast-path: enabling OpenCL amd_bfe optimizations for this device");
        }
        if vendor == GpuVendor::Nvidia {
            println!("NVIDIA OpenCL path: arch={}", slug);
        }
        let safe_name = gpu_arch::safe_device_filename(&device_name);
        let diamond_tag = if diamond_mining { "_dia" } else { "" };
        let binary_file = format!(
            r"{}{}_{}_{}{}.bin",
            opencldir, safe_name, cnf_devices[idx], slug, diamond_tag
        );
        let binary_path = Path::new(&binary_file);

        // Recompile when any .cl under opencldir is newer than the cached binary.
        let need_recompile = if binary_path.exists() {
            let binary_modified = fs::metadata(&binary_path)
                .and_then(|meta| meta.modified())
                .expect("Can't find binary file last edit time");
            let kernel_modified = newest_opencl_source_mtime(&opencldir, kernel_path)
                .expect("Can't find kernel file last edit time");
            kernel_modified > binary_modified
        } else {
            true
        };

        let program = if !need_recompile {
            // Read program from binary file
            let mut binary_file = File::open(&binary_path).expect("No se pudo abrir el archivo binario");
            let mut binary_data = Vec::new();
            binary_file
                .read_to_end(&mut binary_data)
                .expect("Can't read binary file");
            println!("Loading OpenCL from the binary...");
            let binaries = [&binary_data[..]];
            Program::with_binary(
                &context,
                &[device],
                &binaries,
                &CString::new("").unwrap(),
            )
            .expect("Can't create OpenCL program with the binary file")
        } else {
            println!("Compiling...");
            // Compile from source
            compile_program_from_source(
                &context,
                &device,
                &kernel_path,
                &binary_path,
                opencldir.clone(),
                vendor,
                &slug,
                amd_fast,
            )
        };
        
        let (queue, out_of_order) = create_command_queue(&context, &device);

        match build_opencl_resources(
            &program,
            &queue,
            wg,
            *unitsize,
            global_work_size,
            vendor,
            compute_units,
            vram_bytes,
            diamond_mining,
            out_of_order,
        ) {
            Ok(res) => opencl_resource_devices.push(res),
            Err(e) => {
                eprintln!("[efficiency] OpenCL buffer init failed at work_groups={}: {}", wg, e);
                let mut reduced = wg / 2;
                let mut built = false;
                while reduced >= 256 {
                    let gws = reduced * localsize;
                    if let Ok(res) = build_opencl_resources(
                        &program,
                        &queue,
                        reduced,
                        *unitsize,
                        gws,
                        vendor,
                        compute_units,
                        vram_bytes,
                        diamond_mining,
                        out_of_order,
                    ) {
                        println!("[efficiency] Recovered with work_groups={}", reduced);
                        opencl_resource_devices.push(res);
                        built = true;
                        break;
                    }
                    reduced /= 2;
                }
                if !built {
                    eprintln!("[efficiency] Skipping device {} — insufficient VRAM", cnf_devices[idx]);
                }
            }
        }
    }

    opencl_resource_devices
}

fn device_global_mem_bytes(device: &Device) -> u64 {
    match device.info(DeviceInfo::GlobalMemSize) {
        Ok(DeviceInfoResult::GlobalMemSize(v)) => v,
        _ => 0,
    }
}

fn device_compute_units(device: &Device) -> u32 {
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
) -> std::result::Result<Event, String> {
    if unit_size > res.allocated_unitsize {
        return Err(format!(
            "unit_size {} exceeds allocated buffer size {}",
            unit_size, res.allocated_unitsize
        ));
    }
    let global_work_size = num_work_groups.saturating_mul(local_work_size);
    let mut slot = res.kernel_slot.lock().map_err(|e| e.to_string())?;
    if slot.kernel.is_none() || slot.unit_size != unit_size {
        let k = if res.diamond {
            build_diamond_kernel(res, unit_size)?
        } else {
            build_block_kernel(res, unit_size)?
        };
        slot.kernel = Some(k);
        slot.unit_size = unit_size;
    }
    let kernel = slot
        .kernel
        .as_mut()
        .ok_or_else(|| "kernel cache empty".to_string())?;
    update(kernel)?;
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
        cmd.enq()
            .map_err(|e| format!("kernel enqueue: {}", e))?;
    }
    Ok(kernel_event)
}

fn wait_event(event: &Event, label: &str) -> std::result::Result<(), String> {
    event
        .wait_for()
        .map_err(|e| format!("{} wait: {}", label, e))
}

fn read_block_gpu_results(
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

fn read_diamond_gpu_results(
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
fn enqueue_mining_kernel(
    res: &OpenCLResources,
    nonce_start: u32,
    repeat: u32,
    unit_size: u32,
    num_work_groups: u32,
    local_work_size: u32,
    wait: Option<&Event>,
) -> std::result::Result<Event, String> {
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
fn enqueue_diamond_kernel(
    res: &OpenCLResources,
    nonce_start: u64,
    repeat: u32,
    unit_size: u32,
    num_work_groups: u32,
    local_work_size: u32,
    wait: Option<&Event>,
) -> std::result::Result<Event, String> {
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

fn build_opencl_resources(
    program: &Program,
    queue: &Queue,
    workgroups: u32,
    unitsize: u32,
    global_work_size: u32,
    vendor: GpuVendor,
    compute_units: u32,
    vram_bytes: u64,
    diamond: bool,
    out_of_order: bool,
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
    Ok(OpenCLResources {
        workgroups,
        allocated_unitsize: unitsize,
        vendor,
        compute_units,
        vram_bytes,
        diamond,
        out_of_order,
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
    })
}

fn newest_opencl_source_mtime(
    opencldir: &str,
    kernel_path: &Path,
) -> std::io::Result<std::time::SystemTime> {
    use std::time::SystemTime;
    let mut newest = fs::metadata(kernel_path)?.modified()?;
    let dir = Path::new(opencldir);
    if dir.is_dir() {
        let stack = [dir.to_path_buf()];
        let mut pending = stack.to_vec();
        while let Some(path) = pending.pop() {
            let entries = match fs::read_dir(&path) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    pending.push(p);
                    continue;
                }
                if p.extension().and_then(|e| e.to_str()) != Some("cl") {
                    continue;
                }
                if let Ok(meta) = fs::metadata(&p) {
                    if let Ok(m) = meta.modified() {
                        if m > newest {
                            newest = m;
                        }
                    }
                }
            }
        }
    }
    Ok(newest)
}

fn compile_program_from_source(
    context: &Context,
    device: &Device,
    kernel_path: &Path,
    binary_path: &Path,
    opencldir: String,
    vendor: GpuVendor,
    arch_slug: &str,
    amd_fast: bool,
) -> Program {
    // Create program from source files
    let kernel_src = fs::read_to_string(kernel_path)
        .expect("Can't find kernel file");

    let arch_defs = gpu_arch::compile_defines(vendor, arch_slug, amd_fast);
    let compile_options = format!(
        r"-cl-std=CL2.0 -cl-fast-relaxed-math -cl-mad-enable -cl-uniform-work-group-size -I {}{}",
        opencldir, arch_defs
    );
    println!("[OpenCL] compile opts:{}", arch_defs);
    let program_build = Program::builder()
        .src(&kernel_src)
        .devices(device)
        .cmplr_opt(compile_options)
        .build(context);

    let program: Program = match program_build {
        Ok(prog) => {
            prog
        }
        Err(e) => {
            eprintln!("OpenCL program compilation error: {}", e);
            panic!("OpenCL program compilation failed");
        }
    };

    // Get the binary result and save in file
    let program_info_result = program
        .info(ProgramInfo::Binaries)
        .expect("Can't read binary data from compiled kernel");

    // Extract Vec<Vec<u8>> from ProgramInfoResult enum
    let binaries = match program_info_result {
        ProgramInfoResult::Binaries(binaries) => binaries,
        _ => {
            panic!("Compiled files and binaries doesn't match");
        }
    };

    if let Some(binary) = binaries.get(0) {
        println!("Saving OpenCL program in binary file...");
        let mut binary_file = File::create(binary_path)
            .expect("Can't create binary data file");
        binary_file
            .write_all(binary)
            .expect("Can't save binary data");
    } else {
        println!("Can't find binaries from program");
    }

    program
}

