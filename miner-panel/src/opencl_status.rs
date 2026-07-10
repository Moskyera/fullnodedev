use std::path::Path;
use std::process::Command;

use app::opencl_diag::OpenClScan;

#[derive(Clone, Debug, Default)]
pub struct OpenClStatus {
    pub warnings: Vec<String>,
    pub recommended_platform: Option<u32>,
    pub recommended_device: Option<u32>,
    pub recommended_slug: String,
}

pub fn load_opencl_status(work_dir: &Path) -> OpenClStatus {
    if let Some(scan) = run_diagnose_exe(work_dir).or_else(|| read_cached_scan(work_dir)) {
        return scan_to_status(&scan);
    }
    OpenClStatus::default()
}

fn run_diagnose_exe(work_dir: &Path) -> Option<OpenClScan> {
    let exe = work_dir.join("diagnose_opencl.exe");
    #[cfg(unix)]
    let exe = work_dir.join("diagnose_opencl");
    if !exe.is_file() {
        return None;
    }
    let out = Command::new(&exe).arg("--json").output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn read_cached_scan(work_dir: &Path) -> Option<OpenClScan> {
    let cache = work_dir.join("diagnose-opencl.json");
    let raw = std::fs::read_to_string(&cache).ok()?;
    serde_json::from_str(&raw).ok()
}

fn scan_to_status(scan: &OpenClScan) -> OpenClStatus {
    OpenClStatus {
        warnings: scan.warnings.clone(),
        recommended_platform: scan.recommended.as_ref().map(|r| r.platform_id),
        recommended_device: scan.recommended.as_ref().map(|r| r.device_id),
        recommended_slug: scan
            .recommended
            .as_ref()
            .map(|r| r.device_slug.clone())
            .unwrap_or_default(),
    }
}

/// Apply scan recommendation only when the user has not saved OpenCL ids in ini.
pub fn apply_recommended_opencl(
    status: &OpenClStatus,
    platform_id: &mut u32,
    device_id: &mut u32,
    opencl_configured_in_ini: bool,
) -> Option<String> {
    if opencl_configured_in_ini {
        return None;
    }
    let (rp, rd) = (status.recommended_platform?, status.recommended_device?);
    if rp == *platform_id && rd == *device_id {
        return None;
    }
    let msg = format!(
        "OpenCL: using platform {} device {} ({})",
        rp, rd, status.recommended_slug
    );
    *platform_id = rp;
    *device_id = rd;
    Some(msg)
}