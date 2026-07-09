use std::ffi::CString;
use std::path::Path;
use std::fs::{self, File};
use std::io::{Read, Write};
use ocl::enums::{ProgramInfoResult, ProgramInfo};
use ocl::{Buffer, Context, Device, EventList, Kernel, Platform, Program, Queue};

#[allow(dead_code)]
const STUFF_BUFFER_CAP: usize = 512;

fn write_stuff_to_gpu(opencl: &OpenCLResources, data: &[u8]) {
    if data.len() > STUFF_BUFFER_CAP {
        panic!("OpenCL stuff buffer overflow ({} > {})", data.len(), STUFF_BUFFER_CAP);
    }
    let mut padded = vec![0u8; STUFF_BUFFER_CAP];
    padded[..data.len()].copy_from_slice(data);
    opencl
        .buffer_stuff
        .write(&padded)
        .enq()
        .expect("Can't upload stuff buffer");
}

struct OpenCLResources {
    program: Program,
    queue: Queue,
    buffer_best_nonces: Buffer::<u32>,
    buffer_best_nonces_diamond: Buffer::<u64>,
    buffer_global_hashes: Buffer::<u8>,
    buffer_global_order: Buffer::<u32>,
    buffer_best_hashes: Buffer::<u8>,
    /// Reused input buffer — avoids per-kernel GPU allocation.
    buffer_stuff: Buffer::<u8>,
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

    let num_work_items = workgroups * localsize;
    let global_work_size = num_work_items;

    let mut opencl_resource_devices = Vec::with_capacity(devices.len() as usize);
    for (idx, &device) in devices.iter().enumerate() {
        
        println!("-----------------------------------------");
        let name = device.name().expect("Error");
        println!("Device {}: {}", cnf_devices[idx], name);
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

        let device_name = device.name().expect("Can't get device name");
        let amd_fast = device_prefers_amd_ops(&device);
        if amd_fast {
            println!("AMD fast-path: enabling OpenCL amd_bfe optimizations for this device");
        }
        let amd_tag = if amd_fast { "_amd" } else { "" };
        let diamond_tag = if diamond_mining { "_diamonds" } else { "" };
        let binary_file = format!(
            r"{}{}_{}{}{}.bin",
            opencldir, device_name, cnf_devices[idx], amd_tag, diamond_tag
        );
        let binary_path = Path::new(&binary_file);

        // Check if kernel was changed since last time (and need recompile)
        let need_recompile = if binary_path.exists() {
            let binary_modified = fs::metadata(&binary_path)
                .and_then(|meta| meta.modified())
                .expect("Can't find binary file last edit time");
            let kernel_modified = fs::metadata(&kernel_path)
                .and_then(|meta| meta.modified())
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
                amd_fast,
            )
        };
        
        // Create new queue
        let queue = Queue::new(&context, device.clone(), None)
        .expect("Can't create OpenCL event queue");

        opencl_resource_devices.push(OpenCLResources {
            program: program.clone(),
            queue: queue.clone(),
            buffer_best_nonces: Buffer::<u32>::builder()
                .queue(queue.clone())
                .flags(ocl::core::MEM_WRITE_ONLY)
                .len(*workgroups)
                .build()
                .expect("Can't create buffer_best_nonces"),
            buffer_best_nonces_diamond: Buffer::<u64>::builder()
                .queue(queue.clone())
                .flags(ocl::core::MEM_WRITE_ONLY)
                .len(*workgroups)
                .build()
                .expect("Can't create buffer_best_nonces_diamond"),
            buffer_global_hashes: Buffer::<u8>::builder()
                .queue(queue.clone())
                .flags(ocl::core::MEM_READ_WRITE)
                .len(HASH_WIDTH * *unitsize as usize * global_work_size as usize)
                .build()
                .expect("Can't create buffer_global_hashes"),
            buffer_global_order: Buffer::<u32>::builder()
                .queue(queue.clone())
                .flags(ocl::core::MEM_READ_WRITE)
                .len(*unitsize as usize * global_work_size as usize)
                .build()
                .expect("Can't create buffer_global_order"),
            buffer_best_hashes: Buffer::<u8>::builder()
                .queue(queue.clone())
                .flags(ocl::core::MEM_WRITE_ONLY)
                .len(HASH_WIDTH * *workgroups as usize )
                .build()
                .expect("Can't create buffer_best_hashes"),
            buffer_stuff: Buffer::<u8>::builder()
                .queue(queue.clone())
                .flags(ocl::core::MEM_READ_ONLY)
                .len(STUFF_BUFFER_CAP)
                .build()
                .expect("Can't create buffer_stuff"),
        });
    }

    opencl_resource_devices
}

fn device_prefers_amd_ops(device: &Device) -> bool {
    let vendor = device.vendor().unwrap_or_default().to_lowercase();
    let name = device.name().unwrap_or_default().to_lowercase();
    vendor.contains("amd")
        || vendor.contains("advanced micro devices")
        || name.contains("radeon")
        || name.contains("gfx")
}

fn compile_program_from_source(
    context: &Context,
    device: &Device,
    kernel_path: &Path,
    binary_path: &Path,
    opencldir: String,
    amd_fast: bool,
) -> Program {
    // Create program from source files
    let kernel_src = fs::read_to_string(kernel_path)
        .expect("Can't find kernel file");

    // Compile
    let amd_define = if amd_fast { " -DNO_AMD_OPS=0" } else { "" };
    let compile_options = format!(
        r"-cl-std=CL2.0 -cl-fast-relaxed-math -cl-mad-enable -cl-uniform-work-group-size -I {}{}",
        opencldir, amd_define
    );
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

