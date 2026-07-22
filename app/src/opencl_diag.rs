//! OpenCL device scan, AMD platform diagnostics, and auto-selection helpers.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenClDeviceEntry {
    pub index: u32,
    pub name: String,
    pub slug: String,
    pub vendor: String,
    pub compute_units: u32,
    pub vram_mb: u64,
    pub max_work_group_size: u32,
    pub is_discrete: bool,
    pub is_amd: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenClPlatformEntry {
    pub index: u32,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub amd_app_build: u32,
    pub devices: Vec<OpenClDeviceEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenClSelection {
    pub platform_id: u32,
    pub device_id: u32,
    pub device_slug: String,
    pub device_name: String,
    pub amd_app_build: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenClScan {
    pub platforms: Vec<OpenClPlatformEntry>,
    pub warnings: Vec<String>,
    pub recommended: Option<OpenClSelection>,
}

/// Parse `3679` from strings like `OpenCL 2.1 AMD-APP (3679.0)`.
pub fn parse_amd_app_build(version: &str) -> Option<u32> {
    let upper = version.to_uppercase();
    let start = upper.find("AMD-APP")?;
    let tail = &version[start..];
    let open = tail.find('(')?;
    let close = tail.find(')')?;
    let inside = &tail[open + 1..close];
    inside.split('.').next()?.trim().parse().ok()
}

#[cfg(test)]
mod parse_tests {
    use super::parse_amd_app_build;

    #[test]
    fn parses_amd_app_build() {
        assert_eq!(
            parse_amd_app_build("OpenCL 2.1 AMD-APP (3679.0)"),
            Some(3679)
        );
        assert_eq!(
            parse_amd_app_build("OpenCL 2.1 AMD-APP (3652.0)"),
            Some(3652)
        );
        assert_eq!(parse_amd_app_build("OpenCL 1.2"), None);
    }
}

/// Integrated GPU heuristics (e.g. gfx1036 with 1 CU on dual-GPU boards).
pub fn is_igpu_slug(_slug: &str, compute_units: u32, board_name: &str) -> bool {
    let board_l = board_name.to_lowercase();
    if board_l.contains("radeon(tm) graphics") && !board_l.contains("xt") {
        return true;
    }
    compute_units <= 2
}

pub fn is_discrete_amd(slug: &str, compute_units: u32, board_name: &str) -> bool {
    !is_igpu_slug(slug, compute_units, board_name)
}

pub fn is_amd_vendor(vendor: &str) -> bool {
    let v = vendor.to_lowercase();
    v.contains("amd") || v.contains("advanced micro devices")
}

pub fn count_amd_platforms(platforms: &[OpenClPlatformEntry]) -> usize {
    platforms
        .iter()
        .filter(|p| is_amd_vendor(&p.vendor))
        .count()
}

/// Discrete GPU device indices on a platform (empty device_ids → mine all).
pub fn discrete_device_indices(platforms: &[OpenClPlatformEntry], platform_id: u32) -> Vec<u32> {
    platforms
        .iter()
        .find(|platform| platform.index == platform_id)
        .map(|p| {
            p.devices
                .iter()
                .filter(|d| d.is_discrete)
                .map(|d| d.index)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(feature = "ocl")]
pub fn scan_opencl() -> OpenClScan {
    use ocl::{Device, Platform};

    let mut platforms = Vec::new();
    let mut warnings = Vec::new();

    for (pi, platform) in Platform::list().iter().enumerate() {
        let name = platform.name().unwrap_or_else(|_| "?".into());
        let vendor = platform.vendor().unwrap_or_else(|_| "?".into());
        let version = platform.version().unwrap_or_else(|_| "?".into());
        let amd_app_build = parse_amd_app_build(&version).unwrap_or(0);
        let mut devices = Vec::new();
        for (di, device) in Device::list_all(platform)
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            let dname = device.name().unwrap_or_else(|_| "?".into());
            let dvendor = device.vendor().unwrap_or_else(|_| "?".into());
            let slug = crate::gpu_arch::arch_slug(&dname);
            let compute_units = device_compute_units(device);
            let vram_mb = device_global_mem_bytes(device) / (1024 * 1024);
            let max_wg = device.max_wg_size().map(|v| v as u32).unwrap_or(256);
            let vendor_kind = crate::gpu_arch::detect_vendor(&dvendor, &dname);
            let is_amd = vendor_kind == crate::gpu_arch::GpuVendor::Amd;
            let discrete = match vendor_kind {
                crate::gpu_arch::GpuVendor::Amd => is_discrete_amd(&slug, compute_units, &dname),
                crate::gpu_arch::GpuVendor::Nvidia => compute_units > 2 && vram_mb >= 2_048,
                crate::gpu_arch::GpuVendor::Intel => {
                    dname.to_ascii_lowercase().contains("arc ")
                        && compute_units > 2
                        && vram_mb >= 2_048
                }
                crate::gpu_arch::GpuVendor::Unknown => false,
            };
            devices.push(OpenClDeviceEntry {
                index: di as u32,
                name: dname,
                slug,
                vendor: dvendor,
                compute_units,
                vram_mb,
                max_work_group_size: max_wg,
                is_discrete: discrete,
                is_amd,
            });
        }
        platforms.push(OpenClPlatformEntry {
            index: pi as u32,
            name,
            vendor,
            version,
            amd_app_build,
            devices,
        });
    }

    let amd_platforms: Vec<_> = platforms
        .iter()
        .filter(|p| is_amd_vendor(&p.vendor))
        .collect();
    if amd_platforms.len() > 1 {
        let builds: Vec<String> = amd_platforms
            .iter()
            .map(|p| format!("platform {} AMD-APP {}", p.index, p.amd_app_build))
            .collect();
        warnings.push(format!(
            "Multiple AMD OpenCL platforms detected ({}). Prefer the newest AMD-APP build; disable iGPU in BIOS/Device Manager if possible.",
            builds.join(", ")
        ));
    }

    let recommended = recommend_opencl_device(&platforms);
    if let Some(ref sel) = recommended {
        let limits = crate::gpu_arch::ArchLimits::for_slug(&sel.device_slug);
        if limits.is_experimental() && amd_platforms.len() > 1 {
            warnings.push(
                "gfx1201 (RX 9070 XT): duplicate AMD OpenCL platforms — miner will cap work_groups to 64 on this architecture."
                    .to_string(),
            );
        }
    } else {
        warnings.push("No compatible discrete OpenCL GPU found.".to_string());
    }

    OpenClScan {
        platforms,
        warnings,
        recommended,
    }
}

#[cfg(feature = "ocl")]
fn device_compute_units(device: &ocl::Device) -> u32 {
    device
        .info(ocl::core::DeviceInfo::MaxComputeUnits)
        .ok()
        .and_then(|v| {
            if let ocl::core::DeviceInfoResult::MaxComputeUnits(n) = v {
                Some(n as u32)
            } else {
                None
            }
        })
        .unwrap_or(0)
}

#[cfg(feature = "ocl")]
fn device_global_mem_bytes(device: &ocl::Device) -> u64 {
    device
        .info(ocl::core::DeviceInfo::GlobalMemSize)
        .ok()
        .and_then(|v| {
            if let ocl::core::DeviceInfoResult::GlobalMemSize(n) = v {
                Some(n)
            } else {
                None
            }
        })
        .unwrap_or(0)
}

/// Pick newest AMD platform + highest-tier discrete device (prefers gfx1201 / most CUs).
pub fn recommend_discrete_amd(platforms: &[OpenClPlatformEntry]) -> Option<OpenClSelection> {
    let mut best_key: Option<(u32, u32, u64)> = None;
    let mut best_sel: Option<OpenClSelection> = None;
    for plat in platforms {
        if !is_amd_vendor(&plat.vendor) {
            continue;
        }
        for dev in &plat.devices {
            if !dev.is_discrete {
                continue;
            }
            let key = (plat.amd_app_build, dev.compute_units, dev.vram_mb);
            if best_key.map(|bk| key > bk).unwrap_or(true) {
                best_key = Some(key);
                best_sel = Some(OpenClSelection {
                    platform_id: plat.index,
                    device_id: dev.index,
                    device_slug: dev.slug.clone(),
                    device_name: dev.name.clone(),
                    amd_app_build: plat.amd_app_build,
                });
            }
        }
    }
    best_sel
}

/// Prefer AMD (the primary tested path), then fall back to another discrete
/// OpenCL GPU. This remains an OpenCL-only selection; CUDA is never required.
pub fn recommend_opencl_device(platforms: &[OpenClPlatformEntry]) -> Option<OpenClSelection> {
    if let Some(amd) = recommend_discrete_amd(platforms) {
        return Some(amd);
    }
    platforms
        .iter()
        .flat_map(|platform| {
            platform
                .devices
                .iter()
                .filter(|device| device.is_discrete)
                .map(move |device| (platform, device))
        })
        .max_by_key(|(_, device)| (device.vram_mb, device.compute_units))
        .map(|(platform, device)| OpenClSelection {
            platform_id: platform.index,
            device_id: device.index,
            device_slug: device.slug.clone(),
            device_name: device.name.clone(),
            amd_app_build: platform.amd_app_build,
        })
}

/// Resolve config platform/device to the best matching OpenCL runtime selection.
pub fn resolve_opencl_selection(
    platforms: &[OpenClPlatformEntry],
    configured_platform: u32,
    configured_device: u32,
) -> (u32, u32, Vec<String>) {
    let mut notes = Vec::new();
    let Some(plat) = platforms.iter().find(|p| p.index == configured_platform) else {
        if let Some(rec) = recommend_opencl_device(platforms) {
            notes.push(format!(
                "[OpenCL] platform_id={} invalid — using recommended platform {} device {} ({})",
                configured_platform, rec.platform_id, rec.device_id, rec.device_slug
            ));
            return (rec.platform_id, rec.device_id, notes);
        }
        return (configured_platform, configured_device, notes);
    };

    let Some(dev) = plat.devices.iter().find(|d| d.index == configured_device) else {
        if let Some(rec) = recommend_opencl_device(platforms) {
            notes.push(format!(
                "[OpenCL] device_id={} not found on platform {} — using {} ({})",
                configured_device, configured_platform, rec.device_id, rec.device_slug
            ));
            return (rec.platform_id, rec.device_id, notes);
        }
        return (configured_platform, configured_device, notes);
    };

    if dev.is_discrete {
        let slug = dev.slug.clone();
        let mut best_plat = configured_platform;
        let mut best_device = configured_device;
        let mut best_build = plat.amd_app_build;
        for p in platforms {
            if !is_amd_vendor(&p.vendor) {
                continue;
            }
            if p.amd_app_build <= best_build {
                continue;
            }
            if let Some(candidate) = p.devices.iter().find(|d| d.slug == slug && d.is_discrete) {
                best_plat = p.index;
                best_device = candidate.index;
                best_build = p.amd_app_build;
            }
        }
        if best_plat != configured_platform {
            notes.push(format!(
                "[OpenCL] Auto-selected platform {} (AMD-APP {}) for {} — config had platform {} (AMD-APP {})",
                best_plat, best_build, slug, configured_platform, plat.amd_app_build
            ));
        }
        if is_igpu_slug(&dev.slug, dev.compute_units, &dev.name) {
            if let Some(rec) = recommend_opencl_device(platforms) {
                notes.push(format!(
                    "[OpenCL] device {} looks like iGPU — switching to {} ({})",
                    dev.name, rec.device_name, rec.device_slug
                ));
                return (rec.platform_id, rec.device_id, notes);
            }
        }
        return (best_plat, best_device, notes);
    }

    if let Some(rec) = recommend_opencl_device(platforms) {
        notes.push(format!(
            "[OpenCL] Configured device is not discrete — using {} ({})",
            rec.device_name, rec.device_slug
        ));
        return (rec.platform_id, rec.device_id, notes);
    }

    (configured_platform, configured_device, notes)
}

pub fn print_scan_report(scan: &OpenClScan) {
    println!("OpenCL diagnostic scan\n");
    for plat in &scan.platforms {
        println!(
            "Platform {}: {}  vendor={}  version={}  AMD-APP build={}",
            plat.index, plat.name, plat.vendor, plat.version, plat.amd_app_build
        );
        for dev in &plat.devices {
            let kind = if dev.is_discrete {
                "discrete"
            } else if dev.is_amd {
                "iGPU/APU"
            } else {
                "other"
            };
            println!(
                "  device {}: {} ({})  CU={}  VRAM={}MB  max_wg={}  [{}]",
                dev.index,
                dev.name,
                dev.slug,
                dev.compute_units,
                dev.vram_mb,
                dev.max_work_group_size,
                kind
            );
        }
        println!();
    }
    if let Some(rec) = &scan.recommended {
        println!(
            "Recommended: platform_id={} device_id={}  {} ({})  AMD-APP {}",
            rec.platform_id, rec.device_id, rec.device_name, rec.device_slug, rec.amd_app_build
        );
    }
    if !scan.warnings.is_empty() {
        println!("\nWarnings:");
        for w in &scan.warnings {
            println!("  ! {}", w);
        }
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    fn plat(idx: u32, build: u32, devices: Vec<OpenClDeviceEntry>) -> OpenClPlatformEntry {
        OpenClPlatformEntry {
            index: idx,
            name: "AMD".into(),
            vendor: "Advanced Micro Devices, Inc.".to_string(),
            version: format!("OpenCL 2.1 AMD-APP ({build}.0)"),
            amd_app_build: build,
            devices,
        }
    }

    fn dev(idx: u32, slug: &str, cu: u32, discrete: bool) -> OpenClDeviceEntry {
        OpenClDeviceEntry {
            index: idx,
            name: slug.into(),
            slug: slug.into(),
            vendor: "Advanced Micro Devices, Inc.".into(),
            compute_units: cu,
            vram_mb: 16384,
            max_work_group_size: 256,
            is_discrete: discrete,
            is_amd: true,
        }
    }

    #[test]
    fn resolve_upgrades_to_newer_amd_platform() {
        let platforms = vec![
            plat(0, 3679, vec![dev(4, "gfx1201", 32, true)]),
            plat(1, 3652, vec![dev(1, "gfx1201", 32, true)]),
        ];
        let (p, d, notes) = resolve_opencl_selection(&platforms, 1, 1);
        assert_eq!(p, 0);
        assert_eq!(d, 4);
        assert!(!notes.is_empty());
    }

    #[test]
    fn discrete_indices_use_platform_identity_not_vector_position() {
        let platforms = vec![
            plat(7, 3679, vec![dev(2, "gfx1201", 32, true)]),
            plat(9, 3652, vec![dev(5, "gfx1100", 84, true)]),
        ];
        assert_eq!(discrete_device_indices(&platforms, 7), vec![2]);
        assert_eq!(discrete_device_indices(&platforms, 9), vec![5]);
        assert!(discrete_device_indices(&platforms, 0).is_empty());
    }

    #[test]
    fn recommend_skips_igpu() {
        let platforms = vec![plat(
            0,
            3679,
            vec![dev(0, "gfx1036", 1, false), dev(1, "gfx1201", 32, true)],
        )];
        let rec = recommend_discrete_amd(&platforms).unwrap();
        assert_eq!(rec.device_id, 1);
        assert_eq!(rec.device_slug, "gfx1201");
    }

    #[test]
    fn opencl_recommendation_falls_back_to_non_amd_discrete_gpu() {
        let device = OpenClDeviceEntry {
            index: 0,
            name: "NVIDIA GeForce RTX 4070".into(),
            slug: "rtx4070".into(),
            vendor: "NVIDIA Corporation".into(),
            compute_units: 46,
            vram_mb: 12_288,
            max_work_group_size: 1024,
            is_discrete: true,
            is_amd: false,
        };
        let platform = OpenClPlatformEntry {
            index: 0,
            name: "NVIDIA CUDA OpenCL".into(),
            vendor: "NVIDIA Corporation".into(),
            version: "OpenCL 3.0".into(),
            amd_app_build: 0,
            devices: vec![device],
        };
        let rec = recommend_opencl_device(&[platform]).unwrap();
        assert_eq!(rec.device_slug, "rtx4070");
    }
}
