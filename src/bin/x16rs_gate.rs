//! `x16rs_gate`, the gate every later kernel change is judged by.
//!
//!   x16rs_gate equiv     prove GPU == CPU byte for byte
//!   x16rs_gate baseline  fixed-work timing at repeat = 16
//!
//! Exit code 0 only on a pass. Anything else is a failure a script can see.

#[cfg(feature = "ocl")]
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

#[cfg(feature = "ocl")]
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

fn main() {
    #[cfg(not(feature = "ocl"))]
    {
        eprintln!("x16rs_gate needs the OpenCL build: cargo build --release --features ocl --bin x16rs_gate");
        std::process::exit(1);
    }

    #[cfg(feature = "ocl")]
    {
        use app::x16rs_gate::{self, Shape};

        let args: Vec<String> = std::env::args().collect();
        let mode = args.get(1).cloned().unwrap_or_default();

        let opencl_dir = parse_string(&args, "--opencl-dir", &default_opencl_dir());
        let platform: u32 = parse(&args, "--platform", 0);
        let device = parse_string(&args, "--device", "0");
        let threads: usize = parse(
            &args,
            "--threads",
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(8),
        );

        println!("[gate] opencl_dir = {opencl_dir}");
        println!("[gate] platform = {platform}, device_ids = {device}");
        println!("[gate] version  = {} ({})", app::HACASH_NODE_VERSION, app::HACASH_NODE_BUILD_TIME);

        match mode.as_str() {
            "equiv" => {
                let headers: u32 = parse(&args, "--headers", 4);
                let batches: u32 = parse(&args, "--batches", 1);
                let prod_batches: u32 = parse(&args, "--prod-batches", 2);
                let prod_thresholds: u32 = parse(&args, "--prod-thresholds", 255);
                let prod_shape = Shape {
                    work_groups: parse(&args, "--work-groups", 48),
                    local_size: parse(&args, "--local-size", 256),
                    unit_size: parse(&args, "--unit-size", 48),
                };
                println!(
                    "[gate] equiv: {headers} headers x {batches} window(s) x 3 launch shapes x repeats {:?}, \
                     plus {prod_batches} production-shape batch(es) at {}x{}x{}",
                    app::x16rs_gate::GATE_HEIGHTS
                        .iter()
                        .map(|h| x16rs::block_hash_repeat(*h))
                        .collect::<Vec<_>>(),
                    prod_shape.work_groups,
                    prod_shape.local_size,
                    prod_shape.unit_size
                );
                println!("[gate] CPU oracle threads = {threads}");
                if prod_batches > 0 {
                    let window = prod_shape.work_groups as u64
                        * prod_shape.local_size as u64
                        * prod_shape.unit_size as u64;
                    let ranks = app::x16rs_gate::threshold_ranks(window, 1024, prod_thresholds);
                    println!(
                        "[gate] production window = {window} nonces, {} count thresholds; \
                         ONE wrong hash anywhere in a window slips past them all with p = {:.2e}",
                        ranks.len(),
                        app::x16rs_gate::threshold_miss_probability(window, &ranks)
                    );
                }
                match x16rs_gate::run_equivalence(
                    &opencl_dir,
                    platform,
                    &device,
                    headers,
                    batches,
                    prod_shape,
                    prod_batches,
                    prod_thresholds,
                    threads,
                ) {
                    Ok(report) => {
                        println!("\n================ BYTE-EQUIVALENCE GATE ================");
                        print!("{}", report.render());
                        if report.passed() {
                            println!("  RESULT: PASS");
                            std::process::exit(0);
                        }
                        println!("  RESULT: FAIL");
                        std::process::exit(3);
                    }
                    Err(error) => {
                        eprintln!("[gate] ERROR: {error}");
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
                match x16rs_gate::run_baseline(
                    &opencl_dir,
                    platform,
                    &device,
                    shape,
                    height,
                    batches,
                    runs,
                    warmup,
                    headers,
                ) {
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
            _ => {
                eprintln!(
                    "usage:\n  \
                     x16rs_gate equiv    [--opencl-dir D] [--platform N] [--device IDS] [--headers N] [--batches N]\n                      \
                     [--prod-batches N] [--work-groups N] [--local-size N] [--unit-size N] [--threads N]\n  \
                     x16rs_gate baseline [--opencl-dir D] [--platform N] [--device IDS] [--work-groups N]\n                      \
                     [--local-size N] [--unit-size N] [--height N] [--batches N] [--runs N] [--warmup N] [--headers N]\n  \
                     x16rs_gate ab       --opencl-dir A --opencl-dir-b B [--work-groups N] [--local-size N]\n                      \
                     [--unit-size N] [--height N] [--batches N] [--pairs N] [--warmup N] [--headers N]"
                );
                std::process::exit(2);
            }
        }
    }
}
