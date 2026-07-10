fn main() {
    #[cfg(feature = "ocl")]
    {
        let json = std::env::args().any(|a| a == "--json");
        if json {
            println!("{}", app::opencl_list::list_opencl_json());
        } else {
            app::opencl_list::list_opencl_devices();
        }
    }
    #[cfg(not(feature = "ocl"))]
    {
        eprintln!("Rebuild with OpenCL support: cargo build --release --features ocl");
        std::process::exit(1);
    }
}