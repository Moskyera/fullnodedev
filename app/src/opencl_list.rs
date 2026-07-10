//! List OpenCL platforms/devices — used by `list_opencl` / `diagnose_opencl` binaries.

#[cfg(feature = "ocl")]
pub fn list_opencl_devices() {
    let scan = crate::opencl_diag::scan_opencl();
    crate::opencl_diag::print_scan_report(&scan);
    println!("\nConfig hints (poworker.config.ini / diaworker.config.ini):");
    if let Some(rec) = &scan.recommended {
        println!("  [gpu]");
        println!("  use_opencl = true");
        println!("  platform_id = {}", rec.platform_id);
        println!("  device_ids = {}", rec.device_id);
    } else {
        println!("  [gpu]");
        println!("  use_opencl = true");
        println!("  platform_id = <platform number above>");
        println!("  device_ids = <device number>");
    }
    println!("  opencl_dir = ../../x16rs/opencl/   (when running from target/release)");
}

#[cfg(feature = "ocl")]
pub fn list_opencl_json() -> String {
    let scan = crate::opencl_diag::scan_opencl();
    serde_json::to_string_pretty(&scan).unwrap_or_else(|_| "{}".to_string())
}