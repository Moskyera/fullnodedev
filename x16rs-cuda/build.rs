use std::env;
use std::path::PathBuf;

fn discover_cuda_root() -> Option<String> {
    if cfg!(windows) {
        let base = PathBuf::from(r"C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA");
        let mut versions: Vec<_> = std::fs::read_dir(&base).ok()?.filter_map(|e| e.ok()).collect();
        versions.sort_by_key(|e| e.file_name());
        for entry in versions.into_iter().rev() {
            let nvcc = entry.path().join("bin").join("nvcc.exe");
            if nvcc.is_file() {
                return entry.path().to_str().map(|s| s.to_string());
            }
        }
    }
    None
}

fn main() {
    println!("cargo:rustc-check-cfg=cfg(cuda_available)");

    if env::var("CARGO_FEATURE_CUDA").is_err() {
        return;
    }

    fn nvcc_exists(root: &str) -> bool {
        let nvcc = if cfg!(windows) { "nvcc.exe" } else { "nvcc" };
        PathBuf::from(root).join("bin").join(nvcc).is_file()
    }

    let cuda_root = env::var("CUDA_PATH")
        .ok()
        .filter(|p| nvcc_exists(p))
        .or_else(|| env::var("CUDA_HOME").ok().filter(|p| nvcc_exists(p)))
        .or_else(discover_cuda_root);

    let Some(cuda_root) = cuda_root else {
        println!(
            "cargo:warning=CUDA Toolkit not found (set CUDA_PATH or install NVIDIA CUDA) — build without GPU kernels"
        );
        return;
    };

    let nvcc = PathBuf::from(&cuda_root).join("bin").join(if cfg!(windows) {
        "nvcc.exe"
    } else {
        "nvcc"
    });

    println!("cargo:warning=Using CUDA Toolkit at {}", cuda_root);
    println!("cargo:rerun-if-env-changed=CUDA_PATH");
    println!("cargo:rerun-if-env-changed=CUDA_HOME");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let opencl_dir = manifest_dir.join("../x16rs/opencl");
    let cuda_dir = manifest_dir.join("cuda");

    // cc-rs defaults pass GCC flags (-ffunction-sections) that nvcc rejects.
    unsafe {
        env::set_var("CRATE_CC_NO_DEFAULTS", "1");
    }

    let mut build = cc::Build::new();
    build.cuda(true);
    build.cudart("none");
    build.compiler(nvcc);
    build.warnings(false);
    build.opt_level(3);
    build.file(cuda_dir.join("block_miner.cu"));
    build.include(&cuda_dir);
    build.include(&opencl_dir);
    build.define("NVIDIA_GPU", None);
    build.define("__CUDA__", None);
    build.define("__ENDIAN_LITTLE__", None);

    // RTX 20xx / 30xx / 40xx fat binary
    for arch in [
        "arch=compute_75,code=sm_75",
        "arch=compute_86,code=sm_86",
        "arch=compute_89,code=sm_89",
    ] {
        build.flag("-gencode").flag(arch);
    }

    if cfg!(windows) {
        build.flag("-Xcompiler=/MD");
    }

    let lib_dir = PathBuf::from(&cuda_root).join(if cfg!(windows) {
        "lib/x64"
    } else {
        "lib64"
    });
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=cudart");

    build.compile("x16rs_cuda");
    println!("cargo:rustc-cfg=cuda_available");
    println!("cargo:rerun-if-changed=cuda/block_miner.cu");
    println!("cargo:rerun-if-changed=cuda/ocl_compat.cuh");
    println!("cargo:rerun-if-changed=../x16rs/opencl");
}