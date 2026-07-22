//! OpenCL platform/device enumeration and context initialization.

use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::efficiency::clamp_workgroups_for_vram_with_floor;
use crate::gpu_arch::{self, ArchLimits, GpuVendor};
use crate::opencl_diag::OpenClScan;
use ocl::enums::DeviceInfo;
use ocl::{Context, Device, Platform, Program};

use super::compile::{
    OPENCL_CACHE_PREFIX, compile_program_from_source, newest_opencl_source_mtime,
    opencl_cache_fingerprint, prune_opencl_cache, read_cached_program_binary,
};
use super::resources::{
    OpenCLResources, build_opencl_resources, create_command_queue, device_compute_units,
    device_global_mem_bytes,
};

fn parse_device_ids(deviceids: &str) -> Result<Option<Vec<u32>>, String> {
    if deviceids.trim().is_empty() {
        return Ok(None);
    }

    let mut parsed = Vec::new();
    for raw in deviceids.split(',') {
        let token = raw.trim();
        if token.is_empty() {
            return Err("device_ids contains an empty item".to_string());
        }
        let id = token
            .parse::<u32>()
            .map_err(|_| format!("invalid OpenCL device id '{token}'"))?;
        if !parsed.contains(&id) {
            parsed.push(id);
        }
    }
    Ok(Some(parsed))
}

fn opencl_kernel_path(opencldir: &Path, diamond_mining: bool) -> PathBuf {
    let filename = if diamond_mining {
        "x16rs_diamond.cl"
    } else {
        "x16rs_main.cl"
    };
    opencldir.join(filename)
}

fn validated_device_ids(configured: &[u32], available: usize) -> Vec<u32> {
    let mut valid = Vec::with_capacity(configured.len().min(available));
    for &device_id in configured {
        let in_range = usize::try_from(device_id)
            .ok()
            .is_some_and(|index| index < available);
        if in_range && !valid.contains(&device_id) {
            valid.push(device_id);
        }
    }
    valid
}

fn next_recovery_workgroups(current: u32, floor: u32) -> Option<u32> {
    if current <= floor {
        None
    } else {
        Some((current / 2).max(floor))
    }
}

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

    let opencl_path = Path::new(opencldir);
    if !opencl_path.is_dir() {
        eprintln!("[OpenCL] Kernel directory not found: {opencldir}");
        return Vec::new();
    }
    let kernel_path = opencl_kernel_path(opencl_path, diamond_mining);

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

    let mut cnf_devices = match parse_device_ids(deviceids) {
        Ok(Some(ids)) => ids,
        Ok(None) => {
            let mut ids = crate::opencl_diag::discrete_device_indices(&scan.platforms, *platformid);
            if ids.is_empty() {
                if let Some(rec) = &scan.recommended {
                    ids.push(rec.device_id);
                }
            }
            ids
        }
        Err(error) => {
            eprintln!("[OpenCL] Invalid device_ids configuration: {error}");
            return Vec::new();
        }
    };
    let primary_device = *cnf_devices.first().unwrap_or(platformid);
    let (resolved_platform, resolved_device, notes) =
        crate::opencl_diag::resolve_opencl_selection(&scan.platforms, *platformid, primary_device);
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
    let Some(platform) = platforms.get(resolved_platform as usize).cloned() else {
        eprintln!(
            "[OpenCL] Platform {} is unavailable ({} platform(s) detected)",
            resolved_platform,
            platforms.len()
        );
        return Vec::new();
    };

    let name = platform.name().unwrap_or_else(|_| "unknown".into());
    let vendor = platform.vendor().unwrap_or_else(|_| "unknown".into());
    let version: String = platform.version().unwrap_or_else(|_| "unknown".into());
    if !quiet {
        println!("Platform name: {}", name);
        println!("Manufacturer: {}", vendor);
        println!("Version: {}", version);
    }

    // Resolve exact device indices. `Device::by_idx_wrap` is intentionally not
    // used here because it maps an invalid index back onto another GPU.
    let available_devices = match Device::list_all(&platform) {
        Ok(devices) => devices,
        Err(error) => {
            eprintln!("[OpenCL] Cannot enumerate devices: {error}");
            return Vec::new();
        }
    };
    for &device_id in &cnf_devices {
        let in_range = usize::try_from(device_id)
            .ok()
            .is_some_and(|index| index < available_devices.len());
        if !in_range {
            eprintln!(
                "[OpenCL] Device {device_id} is unavailable ({} device(s) detected)",
                available_devices.len()
            );
        }
    }
    let devices: Vec<(u32, Device)> = validated_device_ids(&cnf_devices, available_devices.len())
        .into_iter()
        .filter_map(|device_id| {
            available_devices
                .get(device_id as usize)
                .cloned()
                .map(|device| (device_id, device))
        })
        .collect();

    let mut opencl_resource_devices = Vec::with_capacity(devices.len());
    for (device_id, device) in devices {
        let device_name = device
            .name()
            .unwrap_or_else(|_| format!("device-{device_id}"));
        let device_vendor = device.vendor().unwrap_or_default();
        let vendor = gpu_arch::detect_vendor(&device_vendor, &device_name);
        let vram_bytes = device_global_mem_bytes(&device);
        let compute_units = device_compute_units(&device);
        let arch_limits = ArchLimits::for_slug(&gpu_arch::arch_slug(&device_name));
        let mut wg = gpu_arch::tune_workgroups(*workgroups, compute_units, vendor, arch_limits);
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
        println!("Device {}: {}", device_id, device_name);
        println!("-----------------------------------------");

        // Create context
        let context = match Context::builder()
            .platform(platform)
            .devices(device)
            .build()
        {
            Ok(context) => context,
            Err(e) => {
                eprintln!("[OpenCL] Cannot create context for {device_name}: {e}");
                continue;
            }
        };

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
        let driver_version = device
            .info(DeviceInfo::DriverVersion)
            .map(|info| format!("{info:?}"))
            .unwrap_or_else(|_| "unknown-driver".to_string());
        let compile_identity = format!(
            "{name}|{vendor:?}|{version}|{device_name}|{device_vendor}|{driver_version}|{slug}|{}|{diamond_mining}",
            gpu_arch::compile_defines(vendor, &slug, amd_fast)
        );
        let cache_fingerprint =
            match opencl_cache_fingerprint(opencl_path, &kernel_path, &compile_identity) {
                Ok(fingerprint) => fingerprint,
                Err(error) => {
                    eprintln!("[OpenCL] Invalid kernel source tree: {error}");
                    continue;
                }
            };
        let binary_path = opencl_path.join(format!(
            "{OPENCL_CACHE_PREFIX}{safe_name}_{device_id}_{slug}_{cache_fingerprint:016x}{diamond_tag}.bin"
        ));

        // Recompile when any .cl under opencldir is newer than the cached binary.
        let need_recompile = if binary_path.exists() {
            match (
                fs::metadata(&binary_path).and_then(|meta| meta.modified()),
                newest_opencl_source_mtime(opencl_path, &kernel_path),
            ) {
                (Ok(binary_modified), Ok(kernel_modified)) => kernel_modified > binary_modified,
                _ => true,
            }
        } else {
            true
        };

        let compile = || {
            println!("Compiling...");
            compile_program_from_source(
                &context,
                &device,
                &kernel_path,
                &binary_path,
                opencl_path,
                vendor,
                &slug,
                amd_fast,
            )
        };

        let program = if !need_recompile {
            println!("Loading OpenCL from the binary...");
            let cached = read_cached_program_binary(&binary_path)
                .ok()
                .and_then(|binary_data| {
                    Program::with_binary(
                        &context,
                        &[device],
                        &[&binary_data[..]],
                        &CString::default(),
                    )
                    .ok()
                });
            if cached.is_none() {
                eprintln!("[OpenCL] Cached binary is invalid; recompiling from source");
            }
            cached.or_else(|| compile())
        } else {
            compile()
        };
        let Some(program) = program else {
            eprintln!("[OpenCL] Skipping {device_name}: program initialization failed");
            continue;
        };
        if let Err(error) = prune_opencl_cache(opencl_path, &binary_path) {
            eprintln!("[OpenCL] Cannot prune stale cache binaries: {error}");
        }

        let (queue, out_of_order) = match create_command_queue(&context, &device) {
            Ok(queue) => queue,
            Err(e) => {
                eprintln!("[OpenCL] Skipping {device_name}: {e}");
                continue;
            }
        };

        let needs_queue_finish =
            gpu_arch::ArchLimits::needs_amd_queue_finish(&slug, amd_icd_count > 1);
        match build_opencl_resources(
            &program,
            &queue,
            wg,
            *unitsize,
            global_work_size,
            vendor,
            vram_bytes,
            diamond_mining,
            out_of_order,
            needs_queue_finish,
            &slug,
        ) {
            Ok(mut res) => {
                res.platform_index = resolved_platform;
                res.device_index = device_id;
                opencl_resource_devices.push(res);
            }
            Err(e) => {
                // A Groestl integrity self-test failure is deterministic — it is NOT
                // a VRAM shortage, so shrinking work_groups just re-runs the same
                // failing test and prints a misleading "insufficient VRAM". Report it
                // as the integrity failure it is and skip the recovery loop.
                if e.contains("integrity self-test") {
                    eprintln!(
                        "[efficiency] Skipping device {} — GPU hash integrity self-test failed (not a VRAM issue): {}",
                        device_id, e
                    );
                    continue;
                }
                eprintln!(
                    "[efficiency] OpenCL buffer init failed at work_groups={}: {}",
                    wg, e
                );
                let mut built = false;
                let wg_floor = arch_limits.init_buffer_floor_wg;
                let mut reduced = next_recovery_workgroups(wg, wg_floor);
                while let Some(candidate) = reduced {
                    let gws = candidate * localsize;
                    if let Ok(mut res) = build_opencl_resources(
                        &program,
                        &queue,
                        candidate,
                        *unitsize,
                        gws,
                        vendor,
                        vram_bytes,
                        diamond_mining,
                        out_of_order,
                        needs_queue_finish,
                        &slug,
                    ) {
                        if !quiet {
                            println!("[efficiency] Recovered with work_groups={candidate}");
                        }
                        res.platform_index = resolved_platform;
                        res.device_index = device_id;
                        opencl_resource_devices.push(res);
                        built = true;
                        break;
                    }
                    reduced = next_recovery_workgroups(candidate, wg_floor);
                }
                if !built {
                    eprintln!(
                        "[efficiency] Skipping device {} — insufficient VRAM",
                        device_id
                    );
                }
            }
        }
    }

    opencl_resource_devices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_parser_distinguishes_blank_from_invalid() {
        assert_eq!(parse_device_ids("").unwrap(), None);
        assert_eq!(parse_device_ids("0, 2,0").unwrap(), Some(vec![0, 2]));
        assert!(parse_device_ids("bogus").is_err());
        assert!(parse_device_ids("0,,1").is_err());
    }

    #[test]
    fn device_ids_are_exact_bounded_and_unique() {
        assert_eq!(validated_device_ids(&[0, 99], 1), vec![0]);
        assert_eq!(validated_device_ids(&[1, 0], 2), vec![1, 0]);
        assert_eq!(validated_device_ids(&[0, 0], 1), vec![0]);
        assert!(validated_device_ids(&[0], 0).is_empty());
    }

    #[test]
    fn recovery_workgroups_always_try_the_arch_floor() {
        assert_eq!(next_recovery_workgroups(48, 32), Some(32));
        assert_eq!(next_recovery_workgroups(33, 32), Some(32));
        assert_eq!(next_recovery_workgroups(32, 32), None);
        assert_eq!(next_recovery_workgroups(768, 256), Some(384));
        assert_eq!(next_recovery_workgroups(384, 256), Some(256));
        assert_eq!(next_recovery_workgroups(256, 256), None);
    }

    #[test]
    fn kernel_path_does_not_require_a_trailing_separator() {
        let directory = Path::new("folder with spaces").join("opencl");
        assert_eq!(
            opencl_kernel_path(&directory, false),
            directory.join("x16rs_main.cl")
        );
        assert_eq!(
            opencl_kernel_path(&directory, true),
            directory.join("x16rs_diamond.cl")
        );
    }
}
