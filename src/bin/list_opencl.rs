fn main() {
    #[cfg(feature = "ocl")]
    {
        let json = std::env::args().any(|a| a == "--json");
        if json {
            println!("{}", app::opencl_list::list_opencl_json());
        } else if !app::opencl_list::list_opencl_devices() {
            eprintln!("No usable OpenCL GPU was detected.");
            std::process::exit(2);
        }
    }
    #[cfg(not(feature = "ocl"))]
    {
        eprintln!("Rebuild with OpenCL support: cargo build --release --features ocl");
        std::process::exit(1);
    }
}
