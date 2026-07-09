fn main() {
    #[cfg(feature = "ocl")]
    {
        app::opencl_list::list_opencl_devices();
    }
    #[cfg(not(feature = "ocl"))]
    {
        eprintln!("Rebuild with OpenCL support: cargo build --release --features ocl");
        std::process::exit(1);
    }
}