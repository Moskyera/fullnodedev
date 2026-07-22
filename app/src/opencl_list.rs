//! List OpenCL platforms/devices — used by `list_opencl` / `diagnose_opencl` binaries.

#[cfg(feature = "ocl")]
fn select_opencl_dir_hint(
    local_layout: bool,
    dev_release_layout: bool,
) -> (&'static str, &'static str) {
    if local_layout {
        ("x16rs/opencl/", "(detected in the current miner folder)")
    } else if dev_release_layout {
        (
            "../../x16rs/opencl/",
            "(detected development target/release layout)",
        )
    } else {
        (
            "x16rs/opencl/",
            "(package default; run from the extracted miner folder)",
        )
    }
}

#[cfg(feature = "ocl")]
fn opencl_dir_hint() -> (&'static str, &'static str) {
    select_opencl_dir_hint(
        std::path::Path::new("x16rs/opencl/x16rs_main.cl").is_file(),
        std::path::Path::new("../../x16rs/opencl/x16rs_main.cl").is_file(),
    )
}

#[cfg(feature = "ocl")]
pub fn list_opencl_devices() -> bool {
    let scan = crate::opencl_diag::scan_opencl();
    crate::opencl_diag::print_scan_report(&scan);
    println!("\nConfig hints (HAC poworker.config.ini only; HACD is CPU-only):");
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
    let (opencl_dir, layout) = opencl_dir_hint();
    println!("  opencl_dir = {opencl_dir}   {layout}");
    scan.recommended.is_some()
}

#[cfg(feature = "ocl")]
pub fn list_opencl_json() -> String {
    let scan = crate::opencl_diag::scan_opencl();
    serde_json::to_string_pretty(&scan).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(all(test, feature = "ocl"))]
mod tests {
    use super::*;

    #[test]
    fn packaged_opencl_hint_is_preferred_and_beginner_safe() {
        assert_eq!(
            select_opencl_dir_hint(true, true),
            ("x16rs/opencl/", "(detected in the current miner folder)")
        );
        assert_eq!(select_opencl_dir_hint(false, true).0, "../../x16rs/opencl/");
        assert_eq!(select_opencl_dir_hint(false, false).0, "x16rs/opencl/");
    }
}
