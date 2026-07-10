//! OpenCL program compile and source freshness checks.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use crate::gpu_arch::{self, GpuVendor};
use ocl::enums::ProgramInfoResult;
use ocl::{Context, Device, Program};
use ocl::enums::ProgramInfo;

pub(crate) fn newest_opencl_source_mtime(
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
pub(crate) fn compile_program_from_source(
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

