use std::io;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use app::opencl_diag::OpenClScan;

const DIAGNOSE_TIMEOUT: Duration = Duration::from_secs(8);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(20);
const KILL_GRACE_PERIOD: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Default)]
pub struct OpenClStatus {
    pub warnings: Vec<String>,
    pub recommended_platform: Option<u32>,
    pub recommended_device: Option<u32>,
    pub recommended_slug: String,
    pub recommended_name: String,
    pub recommended_vram_mb: u64,
    pub recommended_compute_units: u32,
    usable_devices: Vec<(u32, u32)>,
}

impl OpenClStatus {
    pub fn has_usable_device(&self) -> bool {
        self.recommended_platform.is_some() && self.recommended_device.is_some()
    }

    pub fn selection_is_usable(&self, platform_id: u32, device_id: u32) -> bool {
        self.usable_devices.contains(&(platform_id, device_id))
    }

    pub fn device_summary(&self) -> String {
        if !self.has_usable_device() {
            return "No compatible OpenCL GPU detected".to_string();
        }
        let vram_gb = self.recommended_vram_mb as f64 / 1024.0;
        format!(
            "{} ({}, {:.1} GB VRAM, {} CU)",
            self.recommended_name, self.recommended_slug, vram_gb, self.recommended_compute_units
        )
    }
}

pub fn load_opencl_status(work_dir: &Path) -> OpenClStatus {
    if let Some(scan) = run_diagnose_exe(work_dir).or_else(|| read_cached_scan(work_dir)) {
        return scan_to_status(&scan);
    }
    OpenClStatus::default()
}

/// Fast startup path: use the last setup scan without launching a process.
/// A live scan is scheduled by the panel after its first frame is available.
pub fn load_cached_opencl_status(work_dir: &Path) -> OpenClStatus {
    read_cached_scan(work_dir)
        .map(|scan| scan_to_status(&scan))
        .unwrap_or_default()
}

fn run_diagnose_exe(work_dir: &Path) -> Option<OpenClScan> {
    let exe = work_dir.join("diagnose_opencl.exe");
    #[cfg(unix)]
    let exe = work_dir.join("diagnose_opencl");
    if !exe.is_file() {
        return None;
    }
    let mut command = Command::new(&exe);
    crate::platform::configure_background_command(&mut command);
    command.arg("--json");
    let out = run_command_with_timeout(&mut command, DIAGNOSE_TIMEOUT).ok()??;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn run_command_with_timeout(
    command: &mut Command,
    timeout: Duration,
) -> io::Result<Option<Output>> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;

    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().map(Some),
            Ok(None) if Instant::now() >= deadline => {
                terminate_child_bounded(&mut child);
                return Ok(None);
            }
            Ok(None) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                thread::sleep(remaining.min(PROCESS_POLL_INTERVAL));
            }
            Err(error) => {
                terminate_child_bounded(&mut child);
                return Err(error);
            }
        }
    }
}

fn terminate_child_bounded(child: &mut Child) {
    let _ = child.kill();
    let deadline = Instant::now() + KILL_GRACE_PERIOD;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => thread::sleep(PROCESS_POLL_INTERVAL),
        }
    }
}

fn read_cached_scan(work_dir: &Path) -> Option<OpenClScan> {
    let cache = work_dir.join("diagnose-opencl.json");
    let raw = std::fs::read_to_string(&cache).ok()?;
    serde_json::from_str(&raw).ok()
}

fn scan_to_status(scan: &OpenClScan) -> OpenClStatus {
    let recommended = scan.recommended.as_ref();
    let recommended_entry = recommended.and_then(|selection| {
        scan.platforms
            .iter()
            .find(|p| p.index == selection.platform_id)
            .and_then(|p| p.devices.iter().find(|d| d.index == selection.device_id))
    });
    OpenClStatus {
        warnings: scan.warnings.clone(),
        recommended_platform: recommended.map(|r| r.platform_id),
        recommended_device: recommended.map(|r| r.device_id),
        recommended_slug: recommended
            .map(|r| r.device_slug.clone())
            .unwrap_or_default(),
        recommended_name: recommended
            .map(|r| r.device_name.clone())
            .unwrap_or_default(),
        recommended_vram_mb: recommended_entry.map(|d| d.vram_mb).unwrap_or(0),
        recommended_compute_units: recommended_entry.map(|d| d.compute_units).unwrap_or(0),
        usable_devices: scan
            .platforms
            .iter()
            .flat_map(|p| {
                p.devices
                    .iter()
                    .filter(|d| d.is_discrete)
                    .map(move |d| (p.index, d.index))
            })
            .collect(),
    }
}

/// Apply the scan recommendation on first run, or repair a saved selection
/// that disappeared after a driver update.
pub fn apply_recommended_opencl(
    status: &OpenClStatus,
    platform_id: &mut u32,
    device_id: &mut u32,
    opencl_configured_in_ini: bool,
) -> Option<String> {
    if opencl_configured_in_ini && status.selection_is_usable(*platform_id, *device_id) {
        return None;
    }
    let (rp, rd) = (status.recommended_platform?, status.recommended_device?);
    let changed = rp != *platform_id || rd != *device_id;
    *platform_id = rp;
    *device_id = rd;
    if !changed && opencl_configured_in_ini {
        return None;
    }
    Some(format!(
        "OpenCL: automatic device {} / platform {} ({})",
        rd, rp, status.recommended_name
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT_CHILD_ENV: &str = "HACASH_PANEL_TIMEOUT_CHILD";

    fn timeout_test_command(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .arg("--exact")
            .arg("opencl_status::tests::timeout_child_entrypoint")
            .arg("--nocapture")
            .env(TIMEOUT_CHILD_ENV, mode);
        command
    }

    #[test]
    fn timeout_child_entrypoint() {
        if std::env::var(TIMEOUT_CHILD_ENV).as_deref() == Ok("slow") {
            thread::sleep(Duration::from_secs(5));
        }
    }

    #[test]
    fn bounded_command_collects_successful_output() {
        let mut command = timeout_test_command("fast");
        let output = run_command_with_timeout(&mut command, Duration::from_secs(2))
            .expect("command should run")
            .expect("command should not time out");
        assert!(output.status.success());
    }

    #[test]
    fn bounded_command_kills_a_hung_diagnostic() {
        let mut command = timeout_test_command("slow");
        let started = Instant::now();
        let output = run_command_with_timeout(&mut command, Duration::from_millis(80))
            .expect("timeout should be handled");
        assert!(output.is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn stale_saved_selection_is_repaired() {
        let status = OpenClStatus {
            recommended_platform: Some(2),
            recommended_device: Some(1),
            recommended_name: "Test GPU".into(),
            usable_devices: vec![(2, 1)],
            ..Default::default()
        };
        let mut platform = 0;
        let mut device = 0;
        assert!(apply_recommended_opencl(&status, &mut platform, &mut device, true).is_some());
        assert_eq!((platform, device), (2, 1));
    }

    #[test]
    fn valid_manual_selection_is_preserved() {
        let status = OpenClStatus {
            recommended_platform: Some(2),
            recommended_device: Some(1),
            usable_devices: vec![(0, 0), (2, 1)],
            ..Default::default()
        };
        let mut platform = 0;
        let mut device = 0;
        assert!(apply_recommended_opencl(&status, &mut platform, &mut device, true).is_none());
        assert_eq!((platform, device), (0, 0));
    }
}
