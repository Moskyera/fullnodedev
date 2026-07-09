//! List OpenCL platforms/devices — used by `list_opencl` binary for AMD miner setup.

use ocl::{Device, Platform};

pub fn list_opencl_devices() {
    let platforms = Platform::list();
    if platforms.is_empty() {
        println!("No OpenCL platforms found.");
        return;
    }
    println!("OpenCL platforms and devices:\n");
    for (pi, platform) in platforms.iter().enumerate() {
        let name = platform.name().unwrap_or_else(|_| "?".into());
        let vendor = platform.vendor().unwrap_or_else(|_| "?".into());
        let version = platform.version().unwrap_or_else(|_| "?".into());
        let amd_platform = vendor.to_lowercase().contains("amd");
        println!(
            "Platform {pi}: {name}  vendor={vendor}  version={version}{}",
            if amd_platform { "  [AMD]" } else { "" }
        );
        let devices = Device::list_all(platform).unwrap_or_default();
        if devices.is_empty() {
            println!("  (no devices)");
            continue;
        }
        for (di, device) in devices.iter().enumerate() {
            let dname = device.name().unwrap_or_else(|_| "?".into());
            let dvendor = device.vendor().unwrap_or_else(|_| "?".into());
            let amd = dvendor.to_lowercase().contains("amd")
                || dname.to_lowercase().contains("radeon")
                || dname.to_lowercase().contains("gfx");
            println!(
                "  device {di}: {dname}  vendor={dvendor}{}",
                if amd { "  [AMD GPU — use in poworker/diaworker gpu section]" } else { "" }
            );
        }
        println!();
    }
    println!("Config hints (poworker.config.ini / diaworker.config.ini):");
    println!("  [gpu]");
    println!("  use_opencl = true");
    println!("  platform_id = <platform number above>");
    println!("  device_ids = <device number, or comma-separated list>");
    println!("  opencl_dir = ../../x16rs/opencl/   (when running from target/debug)");
}