//! `x16rs_gate`, the gate every later kernel change is judged by.
//!
//!   x16rs_gate equiv     prove GPU == CPU byte for byte
//!   x16rs_gate baseline  fixed-work timing at repeat = 16
//!   x16rs_gate ab        paired A/B between two OpenCL kernel trees
//!
//! `equiv` and `baseline` take `--backend ocl` (default) or `--backend cuda`.
//! Both backends run the same corpus, the same CPU oracle and the same
//! comparison; see `app/src/x16rs_gate.rs`.
//!
//! Exit code 0 only on a pass. Anything else is a failure a script can see:
//!
//!   1  built without the backend's feature, so it would have compared nothing
//!   2  bad usage
//!   3  the gate ran and FAILED (a hash differed, or an algorithm was untested)
//!   4  the device or the run errored
//!   5  `ab`: the two trees disagree, so their speeds cannot be compared
//!
//! Exit 1 exists because the trap it guards against has already been walked into
//! once here: `ocl` and `cuda` are optional features, so a plain `cargo test`
//! compiles NONE of either backend and is green over code that was never built.
//! A gate that quietly compared zero hashes would be worse than no gate, so this
//! binary refuses to start instead.

#[cfg(any(feature = "ocl", feature = "cuda"))]
fn parse<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    for pair in args.windows(2) {
        if pair[0] == name {
            if let Ok(value) = pair[1].parse::<T>() {
                return value;
            }
            eprintln!("bad value for {name}: {}", pair[1]);
            std::process::exit(2);
        }
    }
    default
}

#[cfg(any(feature = "ocl", feature = "cuda"))]
fn parse_string(args: &[String], name: &str, default: &str) -> String {
    for pair in args.windows(2) {
        if pair[0] == name {
            return pair[1].clone();
        }
    }
    default.to_string()
}

#[cfg(feature = "ocl")]
fn default_opencl_dir() -> String {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("x16rs")
        .join("opencl")
        .to_string_lossy()
        .into_owned()
}

#[cfg(any(feature = "ocl", feature = "cuda"))]
const USAGE: &str = "usage:
  x16rs_gate equiv    [--backend ocl|cuda] [--headers N] [--batches N] [--prod-batches N]
                      [--prod-thresholds N] [--work-groups N] [--local-size N] [--unit-size N]
                      [--threads N]
                      ocl:  [--opencl-dir D] [--platform N] [--device IDS]
                      cuda: [--cuda-device N]
  x16rs_gate baseline [--backend ocl|cuda] [--work-groups N] [--local-size N] [--unit-size N]
                      [--height N] [--batches N] [--runs N] [--warmup N] [--headers N]
                      ocl:  [--opencl-dir D] [--platform N] [--device IDS]
                      cuda: [--cuda-device N]
  x16rs_gate ab       --opencl-dir A --opencl-dir-b B [--work-groups N] [--local-size N]
                      [--unit-size N] [--height N] [--batches N] [--pairs N] [--warmup N]
                      [--headers N]
                      OpenCL only: CUDA kernels are compiled into the binary by nvcc, so one
                      process cannot hold two of them.";

/// Print, before anything runs, what the run is about to prove and what the CPU
/// side of it will cost. The oracle is linear in the production window, and an
/// operator who is about to wait ten minutes should be told so by the tool, not
/// discover it.
#[cfg(any(feature = "ocl", feature = "cuda"))]
fn announce_equiv(
    prod_shape: app::x16rs_gate::Shape,
    prod_batches: u32,
    prod_thresholds: u32,
    threads: usize,
) {
    use app::x16rs_gate as gate;
    println!("[gate] CPU oracle threads = {threads}");
    println!(
        "[gate] every exhaustive window is {} nonces and EVERY one of them is compared \
         byte-for-byte against x16rs::block_hash",
        gate::SHARE_LIST_CAPACITY
    );
    if prod_batches > 0 {
        let window = prod_shape.nonces();
        let ranks = gate::threshold_ranks(window, gate::SHARE_LIST_CAPACITY, prod_thresholds);
        let (seconds, bytes) = gate::oracle_cost(window.saturating_mul(prod_batches as u64), threads);
        println!(
            "[gate] production shape {prod_shape} = {window} nonces x {prod_batches} batch(es); \
             {} count thresholds; ONE wrong hash anywhere in a window slips past them all with p = {:.2e}",
            ranks.len(),
            gate::threshold_miss_probability(window, &ranks)
        );
        println!(
            "[gate] the CPU oracle for that is about {:.0}s on {threads} threads and {} MiB",
            seconds,
            bytes / (1024 * 1024)
        );
    }
}

/// Did this error describe the kernel's output being wrong, or a failure to run?
///
/// Tagged at the source with `x16rs_gate::DETECTED` rather than matched by
/// phrase, so renaming a message cannot quietly turn a caught defect back into
/// "no GPU attached".
fn is_detection(error: &str) -> bool {
    error.starts_with(app::x16rs_gate::DETECTED)
}

fn main() {
    #[cfg(not(any(feature = "ocl", feature = "cuda")))]
    {
        eprintln!(
            "x16rs_gate was built with NEITHER GPU backend, so it would compare nothing.\n  \
             OpenCL: cargo build --release --features ocl  --bin x16rs_gate\n  \
             CUDA:   cargo build --release --features cuda --bin x16rs_gate\n\
             Note that `ocl` and `cuda` are optional features: a plain `cargo build` or \
             `cargo test` compiles neither backend."
        );
        std::process::exit(1);
    }

    #[cfg(any(feature = "ocl", feature = "cuda"))]
    {
        use app::x16rs_gate::{self, EquivParams, Shape};

        let args: Vec<String> = std::env::args().collect();
        let mode = args.get(1).cloned().unwrap_or_default();

        // Default to whatever this binary can actually do. A CUDA-only build
        // defaulting to OpenCL would greet a Colab operator with "no usable
        // OpenCL device" and teach them nothing.
        let default_backend = if cfg!(feature = "ocl") { "ocl" } else { "cuda" };
        let backend = parse_string(&args, "--backend", default_backend);
        let cuda_device: i32 = parse(&args, "--cuda-device", 0);
        let threads: usize = parse(
            &args,
            "--threads",
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(8),
        );

        println!(
            "[gate] version  = {} ({})",
            app::HACASH_NODE_VERSION,
            app::HACASH_NODE_BUILD_TIME
        );
        println!("[gate] backend  = {backend}");

        // Refuse loudly rather than compare nothing. This is the CUDA half of
        // the rule the OpenCL gate already had.
        match backend.as_str() {
            "ocl" | "opencl" => {
                if !cfg!(feature = "ocl") {
                    eprintln!(
                        "[gate] --backend ocl needs the OpenCL build: \
                         cargo build --release --features ocl --bin x16rs_gate"
                    );
                    std::process::exit(1);
                }
            }
            "cuda" | "nvidia" => {
                if !cfg!(feature = "cuda") {
                    eprintln!(
                        "[gate] --backend cuda needs the CUDA build: \
                         cargo build --release --features cuda --bin x16rs_gate\n\
                         [gate] and that build only contains kernels if nvcc was found: \
                         x16rs-cuda/build.rs prints `Using CUDA Toolkit at ...` when it was, and \
                         only then sets cfg(cuda_available). Without it the crate compiles, every \
                         call returns NotCompiled, and nothing is proved."
                    );
                    std::process::exit(1);
                }
                #[cfg(feature = "cuda")]
                if !x16rs_gate::cuda_kernels_available() {
                    eprintln!(
                        "[gate] this binary has the cuda feature but NO CUDA kernels: \
                         x16rs-cuda/build.rs did not find nvcc at build time, so cfg(cuda_available) \
                         is unset and every device call returns NotCompiled. Rebuild with CUDA_PATH \
                         set. Refusing to run rather than report a gate that compared nothing."
                    );
                    std::process::exit(1);
                }
            }
            other => {
                eprintln!("[gate] unknown --backend '{other}' (expected ocl or cuda)");
                std::process::exit(2);
            }
        }
        let use_cuda = matches!(backend.as_str(), "cuda" | "nvidia");

        #[cfg(feature = "ocl")]
        let opencl_dir = parse_string(&args, "--opencl-dir", &default_opencl_dir());
        #[cfg(not(feature = "ocl"))]
        let opencl_dir = String::new();
        let platform: u32 = parse(&args, "--platform", 0);
        let device = parse_string(&args, "--device", "0");
        if !use_cuda {
            println!("[gate] opencl_dir = {opencl_dir}");
            println!("[gate] platform = {platform}, device_ids = {device}");
        } else {
            println!("[gate] cuda_device = {cuda_device}");
        }

        match mode.as_str() {
            "equiv" => {
                let headers: u32 = parse(&args, "--headers", 4);
                let batches: u32 = parse(&args, "--batches", 1);
                let prod_batches: u32 = parse(&args, "--prod-batches", 2);

                // Skipping the exhaustive pass has to be deliberate.
                //
                // Either flag at zero makes its loop body never run. The device
                // is still opened, the production pass still satisfies every
                // other condition, and the gate printed PASS having compared not
                // one byte exhaustively. The byte-for-byte comparison is the
                // whole reason this exists, so a typo must not be able to remove
                // it and still return green.
                //
                // Production-only IS legitimate, for isolating a defect that
                // appears only at 48x256x48, so it stays available behind a flag
                // that says what it is doing.
                let production_only = args.iter().any(|a| a == "--production-only");
                if (headers == 0 || batches == 0) && !production_only {
                    eprintln!(
                        "[gate] --headers {headers} --batches {batches} would skip the exhaustive \
                         byte-for-byte comparison entirely, which is the point of this gate.\n\
                         [gate] If that is what you meant, pass --production-only and the verdict \
                         will say so."
                    );
                    std::process::exit(2);
                }
                if production_only && prod_batches == 0 {
                    eprintln!(
                        "[gate] --production-only with --prod-batches 0 leaves nothing to compare."
                    );
                    std::process::exit(2);
                }
                let prod_thresholds: u32 = parse(&args, "--prod-thresholds", 255);
                let prod_shape = Shape {
                    work_groups: parse(&args, "--work-groups", 48),
                    local_size: parse(&args, "--local-size", 256),
                    unit_size: parse(&args, "--unit-size", 48),
                };
                println!(
                    "[gate] equiv: {headers} header(s) x {batches} window(s) x {} launch shape(s) \
                     x repeats {:?}, plus {prod_batches} production-shape batch(es) at {prod_shape}",
                    x16rs_gate::EXHAUSTIVE_SHAPES.len(),
                    x16rs_gate::GATE_HEIGHTS
                        .iter()
                        .map(|h| x16rs::block_hash_repeat(*h))
                        .collect::<Vec<_>>(),
                );
                announce_equiv(prod_shape, prod_batches, prod_thresholds, threads);

                let params = EquivParams {
                    headers,
                    batches,
                    prod_shape,
                    prod_batches,
                    prod_thresholds,
                    threads,
                };
                let outcome = if use_cuda {
                    #[cfg(feature = "cuda")]
                    {
                        x16rs_gate::run_equivalence_cuda(cuda_device, params)
                    }
                    #[cfg(not(feature = "cuda"))]
                    unreachable!()
                } else {
                    #[cfg(feature = "ocl")]
                    {
                        x16rs_gate::run_equivalence(
                            &opencl_dir,
                            platform,
                            &device,
                            params.headers,
                            params.batches,
                            params.prod_shape,
                            params.prod_batches,
                            params.prod_thresholds,
                            params.threads,
                        )
                    }
                    #[cfg(not(feature = "ocl"))]
                    unreachable!()
                };
                match outcome {
                    Ok(report) => {
                        println!("\n================ BYTE-EQUIVALENCE GATE ================");
                        print!("{}", report.render());
                        if report.passed() {
                            if report.exhaustive_batches == 0 {
                                println!(
                                    "  RESULT: PASS (production shape only; NO exhaustive \
                                     byte-for-byte comparison was run)"
                                );
                            } else {
                                println!("  RESULT: PASS");
                            }
                            std::process::exit(0);
                        }
                        if let Some(why) = report.failure_reason() {
                            println!("  WHY: {why}");
                        }
                        println!("  RESULT: FAIL");
                        std::process::exit(3);
                    }
                    // A detection is not a device error, and the difference is the
                    // whole value of the gate.
                    //
                    // Several checks report a broken kernel by returning Err: the
                    // production count threshold, the best-hash reduction not being
                    // the window minimum, and a window dump with a wrong hit count,
                    // an out-of-window nonce, a duplicate or a missing one. Mapping
                    // every Err to exit 4 made the wrapper scripts announce "the
                    // gate could not open the device, nothing was compared" on a run
                    // where the gate had just caught the fault it exists to catch.
                    //
                    // That mattered most where it hurt most: the exhaustive shapes
                    // are tiny (1x256x4 up to 4x256x1), so a defect that only appears
                    // at the production shape is visible ONLY through one of these
                    // Err paths.
                    //
                    // So a message that names a comparison exits 3, like any other
                    // mismatch, and only a genuine failure to run exits 4.
                    Err(error) => {
                        eprintln!("[gate] ERROR: {error}");
                        if is_detection(&error) {
                            println!("  WHY: {error}");
                            println!("  RESULT: FAIL");
                            std::process::exit(3);
                        }
                        std::process::exit(4);
                    }
                }
            }
            "baseline" => {
                let shape = Shape {
                    work_groups: parse(&args, "--work-groups", 48),
                    local_size: parse(&args, "--local-size", 256),
                    unit_size: parse(&args, "--unit-size", 48),
                };
                let height: u64 = parse(&args, "--height", x16rs_gate::REPEAT16_HEIGHT);
                let batches: u32 = parse(&args, "--batches", 12);
                let runs: u32 = parse(&args, "--runs", 9);
                let warmup: u32 = parse(&args, "--warmup", 4);
                let headers: u32 = parse(&args, "--headers", 4);
                let outcome = if use_cuda {
                    #[cfg(feature = "cuda")]
                    {
                        x16rs_gate::run_baseline_cuda(
                            cuda_device,
                            shape,
                            height,
                            batches,
                            runs,
                            warmup,
                            headers,
                        )
                    }
                    #[cfg(not(feature = "cuda"))]
                    unreachable!()
                } else {
                    #[cfg(feature = "ocl")]
                    {
                        x16rs_gate::run_baseline(
                            &opencl_dir,
                            platform,
                            &device,
                            shape,
                            height,
                            batches,
                            runs,
                            warmup,
                            headers,
                        )
                    }
                    #[cfg(not(feature = "ocl"))]
                    unreachable!()
                };
                match outcome {
                    Ok(report) => {
                        println!("\n================ FIXED-WORK BASELINE ================");
                        print!("{}", report.render());
                        std::process::exit(0);
                    }
                    Err(error) => {
                        eprintln!("[gate] ERROR: {error}");
                        std::process::exit(4);
                    }
                }
            }
            "ab" => {
                #[cfg(not(feature = "ocl"))]
                {
                    eprintln!(
                        "[gate] `ab` is OpenCL only. It alternates two kernel TREES inside one \
                         process, which OpenCL allows because it compiles kernels at runtime from \
                         a directory. nvcc compiles block_miner.cu into this binary, so a CUDA \
                         process holds exactly one kernel build."
                    );
                    std::process::exit(2);
                }
                #[cfg(feature = "ocl")]
                {
                    if use_cuda {
                        eprintln!(
                            "[gate] `ab` is OpenCL only: nvcc compiles the CUDA kernels into this \
                             binary, so one process cannot hold two of them to alternate. Compare \
                             two CUDA builds with `baseline` across processes and treat anything \
                             under the ~2.6% between-process spread as unresolved."
                        );
                        std::process::exit(2);
                    }
                    let shape = Shape {
                        work_groups: parse(&args, "--work-groups", 48),
                        local_size: parse(&args, "--local-size", 256),
                        unit_size: parse(&args, "--unit-size", 48),
                    };
                    let dir_b = parse_string(&args, "--opencl-dir-b", &opencl_dir);
                    let height: u64 = parse(&args, "--height", x16rs_gate::REPEAT16_HEIGHT);
                    let batches: u32 = parse(&args, "--batches", 60);
                    let pairs: u32 = parse(&args, "--pairs", 11);
                    let warmup: u32 = parse(&args, "--warmup", 1200);
                    let headers: u32 = parse(&args, "--headers", 4);
                    match x16rs_gate::run_ab(
                        &opencl_dir,
                        &dir_b,
                        platform,
                        &device,
                        shape,
                        height,
                        batches,
                        pairs,
                        warmup,
                        headers,
                    ) {
                        Ok(report) => {
                            println!("\n================ PAIRED A/B ================");
                            print!("{}", report.render());
                            std::process::exit(if report.identical_output { 0 } else { 5 });
                        }
                        Err(error) => {
                            eprintln!("[gate] ERROR: {error}");
                            std::process::exit(4);
                        }
                    }
                }
            }
            _ => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
        }
    }
}
