//! OpenCL platform/device enumeration and context initialization.

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::ffi::CString;

use crate::efficiency::clamp_workgroups_for_vram_with_floor;
use crate::gpu_arch::{self, ArchLimits, GpuVendor};
use crate::opencl_diag::OpenClScan;
use ocl::{Context, Device, Platform, Program};

use super::compile::{compile_program_from_source, newest_opencl_source_mtime};
use super::resources::{
    build_opencl_resources, create_command_queue, device_compute_units, device_global_mem_bytes,
    OpenCLResources,
};

pub fn initialize_opencl(
    diamond_mining: bool,
    opencldir: &String,
    platformid: &u32,
    deviceids: &String,
    workgroups: &u32,
    localsize: &u32,
    unitsize: &u32,
    cached_scan: Option<&OpenClScan>,
    quiet: bool,
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

    let scan = match cached_scan {
        Some(s) => s,
        None => {
            let fresh = crate::opencl_diag::scan_opencl();
            return initialize_opencl(
                diamond_mining,
                opencldir,
                platformid,
                deviceids,
                workgroups,
                localsize,
                unitsize,
                Some(&fresh),
                quiet,
            );
        }
    };
    for w in &scan.warnings {
        eprintln!("[OpenCL] {}", w);
    }

    let mut cnf_devices: Vec<u32> = deviceids
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .filter_map(|s| s.trim().parse::<u32>().ok())
        .collect();
    if cnf_devices.is_empty() {
        cnf_devices = crate::opencl_diag::discrete_device_indices(&scan.platforms, *platformid);
        if cnf_devices.is_empty() {
            if let Some(rec) = &scan.recommended {
                cnf_devices.push(rec.device_id);
            }
        }
    }
    let primary_device = *cnf_devices.first().unwrap_or(platformid);
    let (resolved_platform, resolved_device, notes) = crate::opencl_diag::resolve_opencl_selection(
        &scan.platforms,
        *platformid,
        primary_device,
    );
    if !quiet {
        for n in &notes {
            println!("{}", n);
        }
    }
    if !cnf_devices.is_empty() {
        cnf_devices[0] = resolved_device;
    } else {
        cnf_devices.push(resolved_device);
    }
    let amd_icd_count = crate::opencl_diag::count_amd_platforms(&scan.platforms);

    let platforms = Platform::list();
    let platform = platforms
        .get(resolved_platform as usize)
        .expect("The specified platform id is invalid")
        .clone();

    let name = platform.name().expect("Error");
    let vendor = platform.vendor().expect("Error");
    let version: String = platform.version().expect("Error");
    if !quiet {
        println!("Platform name: {}", name);
        println!("Manufacturer: {}", vendor);
        println!("Version: {}", version);
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
        let arch_limits = ArchLimits::for_slug(&gpu_arch::arch_slug(&device_name));
        let mut wg =
            gpu_arch::tune_workgroups(*workgroups, compute_units, vendor, arch_limits);
        if !quiet && compute_units > 0 {
            println!(
                "[OpenCL] CU={} tuned work_groups={} (config {})",
                compute_units, wg, workgroups
            );
        }
        if vram_bytes > 0 {
            let clamped = clamp_workgroups_for_vram_with_floor(
                vram_bytes,
                *localsize,
                *unitsize,
                wg,
                arch_limits.panel_min_wg,
            );
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
        let amd_plat_count = crate::opencl_diag::count_amd_platforms(&scan.platforms);
        let capped = arch_limits.workgroups_cap(wg, amd_plat_count);
        if capped < wg && !quiet {
            println!(
                "[OpenCL] {}: work_groups {} -> {} ({} AMD platform(s))",
                slug, wg, capped, amd_plat_count
            );
            wg = capped;
        } else if capped < wg {
            wg = capped;
        }
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

        let needs_queue_finish =
            gpu_arch::ArchLimits::needs_amd_queue_finish(&slug, amd_icd_count > 1);
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
            needs_queue_finish,
            &slug,
        ) {
            Ok(mut res) => {
                res.platform_index = resolved_platform;
                res.device_index = cnf_devices[idx];
                opencl_resource_devices.push(res);
            }
            Err(e) => {
                eprintln!("[efficiency] OpenCL buffer init failed at work_groups={}: {}", wg, e);
                let mut reduced = wg / 2;
                let mut built = false;
                let wg_floor = arch_limits.init_buffer_floor_wg;
                while reduced >= wg_floor {
                    let gws = reduced * localsize;
                    if let Ok(mut res) = build_opencl_resources(
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
                        needs_queue_finish,
                        &slug,
                    ) {
                        if !quiet {
                            println!("[efficiency] Recovered with work_groups={}", reduced);
                        }
                        res.platform_index = resolved_platform;
                        res.device_index = cnf_devices[idx];
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
