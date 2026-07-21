use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    #[cfg(not(feature = "ocl"))]
    {
        eprintln!("Rebuild with OpenCL support: cargo build --release --features ocl");
        std::process::exit(1);
    }
    #[cfg(feature = "ocl")]
    {
        let args: Vec<String> = env::args().collect();
        let json_only = args.iter().any(|a| a == "--json");
        let write_report = args.iter().any(|a| a == "--report");
        let scan = app::opencl_diag::scan_opencl();

        if json_only {
            println!(
                "{}",
                serde_json::to_string_pretty(&scan).unwrap_or_else(|_| "{}".into())
            );
        } else {
            app::opencl_diag::print_scan_report(&scan);
        }

        if write_report {
            let mut path = PathBuf::from("diagnose-opencl.json");
            if let Some(p) = args.iter().position(|a| a == "--report") {
                if let Some(out) = args.get(p + 1) {
                    if !out.starts_with('-') {
                        path = PathBuf::from(out);
                    }
                }
            }
            let json = serde_json::to_string_pretty(&scan).unwrap_or_else(|_| "{}".into());
            if let Err(e) = fs::write(&path, &json) {
                eprintln!("Could not write {}: {}", path.display(), e);
                std::process::exit(1);
            }
            eprintln!("Wrote {}", path.display());
        }
    }
}
