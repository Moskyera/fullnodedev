use std::sync::atomic::{AtomicBool, Ordering::*};
use std::sync::Arc;

use std::time::*;

use reqwest::blocking::Client as HttpClient;
use serde_json::Value as JV;

use crate::efficiency::*;

use basis::difficulty::*;
use field::*;
use protocol::block::*;
use sys::*;

#[cfg(feature = "ocl")]
use crate::opencl_gpu::block::do_group_block_mining_opencl;
#[cfg(feature = "ocl")]
use crate::opencl_gpu::initialize_opencl;
#[cfg(feature = "cuda")]
include! {"cuda_pow.rs"}

#[path = "block_mining_runtime.rs"]
mod block_mining_runtime;

/*****************************************/

#[derive(Clone)]
pub struct PoWorkConf {
    pub rpcaddr: String,
    pub supervene: u32, // cpu core (configured)
    pub noncemax: u32,
    pub noticewait: u64,   // new block notice wait
    pub useopencl: bool,   // use opencl miner
    pub usecuda: bool,     // use cuda miner (NVIDIA)
    pub cudadevice: i32,   // cuda device index
    pub workgroups: u32,   // opencl work groups
    pub localsize: u32,    // opencl work units per work group
    pub unitsize: u32,     // opencl hashes per work unit
    pub opencldir: String, // opencl source dir
    pub debug: u32,        // enable debug mode
    pub platformid: u32,   // opencl platform id
    pub deviceids: String, // opencl device id list
    /// When OpenCL is on, also run Ryzen CPU miner threads (hybrid).
    pub cpu_assist: bool,
    pub gpu_profile: String,
    pub efficiency: EfficiencyConf,
    pub runtime: Arc<MiningRuntimeState>,
}

impl PoWorkConf {
    pub fn new(ini: &IniObj) -> PoWorkConf {
        let sec = &ini_section(ini, "default"); // default = root
        let sec_gpu = &ini_section(ini, "gpu");
        let efficiency = EfficiencyConf::from_ini(ini);
        let tuning = resolve_gpu_tuning(sec_gpu, &efficiency);
        let configured_supervene = ini_must_u64(sec, "supervene", 2) as u32;
        let active = efficiency.initial_active_supervene(configured_supervene);
        let runtime = MiningRuntimeState::new(tuning.workgroups, active);
        let cnf = PoWorkConf {
            rpcaddr: ini_must(sec, "connect", "127.0.0.1:8081"),
            supervene: configured_supervene,
            noncemax: ini_must_u64(sec, "nonce_max", u32::MAX as u64) as u32,
            noticewait: ini_must_u64(sec, "notice_wait", 45),
            useopencl: ini_must_bool(sec_gpu, "use_opencl", false) as bool,
            usecuda: ini_must_bool(sec_gpu, "use_cuda", false) as bool,
            cudadevice: ini_must_u64(sec_gpu, "cuda_device", 0) as i32,
            workgroups: tuning.workgroups,
            localsize: ini_must_u64(sec_gpu, "local_size", 256) as u32,
            unitsize: tuning.unitsize,
            opencldir: ini_must(sec_gpu, "opencl_dir", "opencl/"),
            debug: ini_must_u64(sec_gpu, "debug", 0) as u32,
            platformid: ini_must_u64(sec_gpu, "platform_id", 0) as u32,
            deviceids: ini_must(sec_gpu, "device_ids", ""),
            cpu_assist: ini_must_bool(sec_gpu, "cpu_assist", true) as bool,
            gpu_profile: tuning.profile.clone(),
            efficiency,
            runtime,
        };
        println!(
            "[efficiency] mode={} profile={} work_groups={} unit_size={} dynamic_supervene={}",
            cnf.efficiency.mode.label(),
            cnf.gpu_profile,
            cnf.workgroups,
            cnf.unitsize,
            cnf.efficiency.dynamic_supervene
        );
        cnf
    }

    /// Minimal config for integration tests.
    pub fn test_defaults(rpcaddr: String, supervene: u32, noncemax: u32) -> PoWorkConf {
        let mut cnf = PoWorkConf::new(&IniObj::new());
        cnf.rpcaddr = rpcaddr;
        cnf.supervene = supervene;
        cnf.noncemax = noncemax;
        cnf.useopencl = false;
        cnf.cpu_assist = false;
        cnf
    }
}

/*****************************************/

use std::sync::LazyLock;
static HTTP_CLIENT: LazyLock<HttpClient> =
    LazyLock::new(|| HttpClient::builder().no_proxy().build().unwrap());

pub fn poworker() {
    let cnfp = "./poworker.config.ini".to_string();
    let inicnf = sys::load_config(cnfp.clone());
    let cnf = PoWorkConf::new(&inicnf);
    if cnf.efficiency.benchmark_seconds > 0 {
        run_block_mining_benchmark(&cnf, &cnfp);
        return;
    }
    poworker_with_conf(cnf);
}

pub fn poworker_with_conf(cnf: PoWorkConf) {
    poworker_with_stop(cnf, None);
}

pub fn poworker_with_stop(cnf: PoWorkConf, stop_flag: Option<Arc<AtomicBool>>) {
    block_mining_runtime::start_block_mining_workers(&cnf, stop_flag.clone());

    // loop
    loop {
        if should_stop(&stop_flag) {
            return;
        }
        if !is_within_idle_schedule(
            cnf.efficiency.idle_start_hour,
            cnf.efficiency.idle_end_hour,
        ) {
            delay_continue_ms!(5000);
            continue;
        }
        if cnf.runtime.paused_unprofitable.load(Relaxed) {
            delay_continue_ms!(3000);
            continue;
        }
        cnf.runtime.apply_thermal_throttle(
            cnf.efficiency.max_temp_c,
            cnf.efficiency.throttle_workgroups,
            &cnf.efficiency.thermal_file,
            cnf.efficiency.thermal_gpu_index,
        );
        pull_pending_block_stuff(&cnf);
        delay_continue_ms!(25);
    }
}

fn should_stop(stop_flag: &Option<Arc<AtomicBool>>) -> bool {
    stop_flag.as_ref().map(|f| f.load(Relaxed)).unwrap_or(false)
}


///////////////////////////////

fn pull_pending_block_stuff(cnf: &PoWorkConf) {
    let curr_hei = block_mining_runtime::current_mining_height();

    // query pending
    let urlapi_pending = format!(
        "http://{}/query/miner/pending?stuff=true&t={}",
        &cnf.rpcaddr,
        sys::curtimes()
    );
    let res = HTTP_CLIENT.get(&urlapi_pending).send();
    let Ok(repv) = res else {
        println!("Error: cannot get block data at {}\n", &urlapi_pending);
        delay_return!(30);
    };
    let Ok(jsdata) = repv.text() else {
        println!(
            "Error: cannot read block data body at {}\n",
            &urlapi_pending
        );
        delay_return!(10);
    };
    let Ok(res) = serde_json::from_str::<JV>(&jsdata) else {
        println!(
            "Error: invalid block data json at {} (body len {})\n",
            &urlapi_pending,
            jsdata.len()
        );
        delay_return!(10);
    };
    let jstr = |k| res[k].as_str().unwrap_or("");
    let jnum = |k| res[k].as_u64().unwrap_or(0);
    let JV::String(ref _blkhd) = res["block_intro"] else {
        println!("Error: get block stuff error: {}", jstr("err"));
        delay_return!(15);
    };
    let pending_height = jnum("height");

    // set pending block stuff
    if pending_height > curr_hei {
        block_mining_runtime::set_pending_block_stuff(pending_height, res);
        if curr_hei == 0 {
            block_mining_runtime::may_print_turn_to_nex_block_mining(curr_hei, None);
        }
    }

    // with notice
    let mut rpid = vec![0].repeat(16);
    loop {
        getrandom::fill(&mut rpid).unwrap();
        let urlapi_notice = format!(
            "http://{}/query/miner/notice?wait={}&height={}&rqid={}",
            &cnf.rpcaddr,
            &cnf.noticewait,
            pending_height,
            &hex::encode(&rpid)
        );
        // println!("\n-------- {} -------- {}\n", &ctshow(), &urlapi_notice);
        let res = HTTP_CLIENT
            .get(&urlapi_notice)
            .timeout(Duration::from_secs(300))
            .send();
        let Ok(repv) = res else {
            println!("Error: cannot get miner notice at {}\n", &urlapi_notice);
            delay_return!(10);
        };
        let Ok(jsdata) = repv.text() else {
            println!("Error: cannot read miner notice at {}", &urlapi_notice);
            delay_return!(1);
        };
        let Ok(res2) = serde_json::from_str::<JV>(&jsdata) else {
            // println!("{}", &jsdata);
            panic!("miner notice error: {}", &jsdata);
        };
        let jnum = |k| res2[k].as_u64().unwrap_or(0);
        let res_hei = jnum("height");
        // println!("\n++++++++ {} {} {}\n", &jsdata, res_hei, current_height);
        if res_hei >= pending_height {
            // next block discover
            break;
        }
        // continue to wait
    }
}

fn push_block_mining_success(
    cnf: &PoWorkConf,
    success: &block_mining_runtime::BlockMiningResult,
) {
    let urlapi_success = format!(
        "http://{}/submit/miner/success?height={}&block_nonce={}&coinbase_nonce={}&t={}",
        &cnf.rpcaddr,
        success.height,
        success.head_nonce,
        success.coinbase_nonce.to_hex(),
        sys::curtimes()
    );
    let res_text = match HTTP_CLIENT.get(&urlapi_success).send() {
        Ok(resp) => resp.text().unwrap_or_default(),
        Err(e) => format!("Request failed: {}", e),
    };
    println!("{} {}", &urlapi_success, res_text);
    // print
    println!(
        "\n\n████████████████ [MINING SUCCESS] Find a block height {},\n██ hash {} to submit.",
        success.height,
        success.result_hash.to_hex()
    );
    println!("▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔")
}

#[cfg(feature = "ocl")]
fn bench_block_hps(
    opencl: &crate::opencl_gpu::OpenCLResources,
    cnf: &PoWorkConf,
    wg_eff: u32,
    us: u32,
    seconds: u64,
) -> f64 {
    let height = 1u64;
    let block_intro = BlockIntro::default().serialize();
    let batch = wg_eff.saturating_mul(cnf.localsize).saturating_mul(us);
    let deadline = Instant::now() + Duration::from_secs(seconds.max(3));
    let mut total_hashes = 0u64;
    let mut total_secs = 0.0f64;
    let mut success_batches = 0u32;
    let mut first_err: Option<String> = None;
    let mut nonce = 0u32;
    while Instant::now() < deadline {
        let ctn = Instant::now();
        match do_group_block_mining_opencl(
            opencl,
            height,
            block_intro.clone(),
            nonce,
            wg_eff,
            cnf.localsize,
            us,
        ) {
            Ok(_) => {
                success_batches += 1;
                total_hashes += batch as u64;
            }
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e.display());
                }
            }
        }
        let used = ctn.elapsed().as_secs_f64();
        if used > 0.0 {
            total_secs += used;
        }
        nonce = nonce.wrapping_add(batch);
    }
    if success_batches == 0 || total_secs <= 0.0 {
        if let Some(e) = first_err {
            eprintln!(
                "[benchmark] GPU error at wg={} unit_size={}: {}",
                wg_eff, us, e
            );
        }
        0.0
    } else {
        total_hashes as f64 / total_secs
    }
}

#[cfg(feature = "ocl")]
struct ProfileBenchResult {
    profile: &'static str,
    hps: f64,
    kh_per_j: f64,
}

#[cfg(feature = "ocl")]
fn pick_benchmark_profile(
    results: &[ProfileBenchResult],
    mode: EfficiencyMode,
    vendor: crate::gpu_arch::GpuVendor,
    min_tier: i8,
) -> &'static str {
    let viable: Vec<&ProfileBenchResult> = results
        .iter()
        .filter(|r| r.hps > 0.0 && profile_tier(r.profile) >= min_tier)
        .collect();
    let pool: Vec<&ProfileBenchResult> = if viable.is_empty() {
        results.iter().filter(|r| r.hps > 0.0).collect()
    } else {
        viable
    };
    if pool.is_empty() {
        return tier_profile_for_vendor(vendor, min_tier);
    }
    match mode {
        EfficiencyMode::Max => pool
            .iter()
            .max_by(|a, b| a.hps.partial_cmp(&b.hps).unwrap_or(std::cmp::Ordering::Equal))
            .map(|r| r.profile)
            .unwrap_or(tier_profile_for_vendor(vendor, min_tier)),
        _ => pool
            .iter()
            .max_by(|a, b| {
                a.kh_per_j
                    .partial_cmp(&b.kh_per_j)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|r| r.profile)
            .unwrap_or(tier_profile_for_vendor(vendor, min_tier)),
    }
}

fn run_block_mining_benchmark(cnf: &PoWorkConf, config_path: &str) {
    #[cfg(not(feature = "ocl"))]
    {
        let _ = (cnf, config_path);
        println!("[benchmark] Rebuild with --features ocl and use_opencl=true");
        return;
    }
    #[cfg(feature = "ocl")]
    {
        if !cnf.useopencl {
            println!("[benchmark] Set use_opencl=true in [gpu]");
            return;
        }
        let total_secs = cnf.efficiency.benchmark_seconds.max(15) as u64;
        let fine = cnf.efficiency.wants_fine_sweep();
        let profile_secs = if fine {
            (total_secs * 70 / 100).max(20)
        } else {
            total_secs
        };
        let sweep_secs = if fine {
            total_secs.saturating_sub(profile_secs).max(10)
        } else {
            0
        };

        // Allocate GPU buffers for the largest profile unit_size (amd_max uses 128).
        let init_unitsize = cnf.unitsize.max(128);
        let scan = crate::opencl_diag::scan_opencl();
        let opencl_resources = initialize_opencl(
            false,
            &cnf.opencldir,
            &cnf.platformid,
            &cnf.deviceids,
            &cnf.workgroups,
            &cnf.localsize,
            &init_unitsize,
            Some(&scan),
            false,
        );
        if opencl_resources.is_empty() {
            println!("[benchmark] No OpenCL devices");
            return;
        }

        for (dev_i, opencl) in opencl_resources.iter().enumerate() {
            let profiles = benchmark_profiles_for_vendor(opencl.vendor);
            let per = (profile_secs / profiles.len() as u64).max(4);
            println!(
                "[benchmark] Device #{}: {}s x {} profiles{}",
                dev_i,
                per,
                profiles.len(),
                if fine { " + fine sweep" } else { "" }
            );

            let min_tier = min_profile_tier_for_mode(cnf.efficiency.mode);
            let mut bench_results: Vec<ProfileBenchResult> = Vec::new();

            for profile in profiles {
                let (wg, us) = profile_tuning(profile);
                let wg_eff = opencl.workgroups.min(wg);
                let hps = bench_block_hps(opencl, cnf, wg_eff, us, per);
                let watts = cnf.efficiency.estimate_gpu_watts(profile);
                let kh_per_j = if watts > 0.0 {
                    hps / watts / 1000.0
                } else {
                    0.0
                };
                if hps <= 0.0 {
                    println!(
                        "[benchmark] dev{} {}: SKIPPED (OOM/failed, wg={})",
                        dev_i, profile, wg_eff
                    );
                    continue;
                }
                println!(
                    "[benchmark] dev{} {}: {} ({:.1} kH/J, wg={})",
                    dev_i,
                    profile,
                    rates_to_show(hps),
                    kh_per_j,
                    wg_eff
                );
                bench_results.push(ProfileBenchResult {
                    profile,
                    hps,
                    kh_per_j,
                });
            }

            let base_profile =
                pick_benchmark_profile(&bench_results, cnf.efficiency.mode, opencl.vendor, min_tier);
            let (base_wg, us) = profile_tuning(base_profile);
            let mut pick = BenchmarkPick::from_profile(base_profile);

            if fine && sweep_secs > 0 {
                let vram = opencl.vram_bytes;
                let wg_sweep_secs = sweep_secs / 2;
                let us_sweep_secs = sweep_secs.saturating_sub(wg_sweep_secs).max(6);
                let candidates = sweep_workgroup_candidates(base_wg, vram, cnf.localsize, us);
                let per_wg = (wg_sweep_secs / candidates.len() as u64).max(3);
                let mut best_sweep_hps = 0.0f64;
                let mut best_sweep_wg = pick.workgroups;
                println!(
                    "[benchmark] dev{} fine wg sweep: {} candidates x {}s",
                    dev_i,
                    candidates.len(),
                    per_wg
                );
                for wg_try in candidates {
                    let wg_eff = opencl.workgroups.min(wg_try);
                    let hps = bench_block_hps(opencl, cnf, wg_eff, us, per_wg);
                    println!(
                        "[benchmark] dev{} wg={}: {}",
                        dev_i,
                        wg_eff,
                        rates_to_show(hps)
                    );
                    if hps > 0.0 && hps > best_sweep_hps {
                        best_sweep_hps = hps;
                        best_sweep_wg = wg_eff;
                    }
                }
                if best_sweep_hps > 0.0 {
                    pick.workgroups = best_sweep_wg;
                }

                let us_candidates =
                    sweep_unitsize_candidates(pick.unitsize, opencl.allocated_unitsize);
                let per_us = (us_sweep_secs / us_candidates.len() as u64).max(3);
                let mut best_us_hps = 0.0f64;
                let mut best_us = pick.unitsize;
                println!(
                    "[benchmark] dev{} fine unit_size sweep: {:?} x {}s",
                    dev_i,
                    us_candidates,
                    per_us
                );
                for us_try in us_candidates {
                    let hps = bench_block_hps(opencl, cnf, pick.workgroups, us_try, per_us);
                    println!(
                        "[benchmark] dev{} unit_size={}: {}",
                        dev_i,
                        us_try,
                        rates_to_show(hps)
                    );
                    if hps > 0.0 && hps > best_us_hps {
                        best_us_hps = hps;
                        best_us = us_try;
                    }
                }
                if best_us_hps > 0.0 {
                    pick.unitsize = best_us;
                }
            }

            println!(
                "[benchmark] dev{} pick: profile={} work_groups={} unit_size={} (mode={})",
                dev_i,
                pick.profile,
                pick.workgroups,
                pick.unitsize,
                cnf.efficiency.mode.label()
            );

            if dev_i == 0 {
                if bench_results.iter().any(|r| r.hps > 0.0) {
                    match apply_benchmark_pick(config_path, &pick) {
                        Ok(()) => {
                            println!("[benchmark] Config updated — restart mining with the new tuning.")
                        }
                        Err(e) => println!("[benchmark] Could not patch ini: {}", e),
                    }
                } else {
                    println!(
                        "[benchmark] No successful profiles — config unchanged (check OpenCL driver / work_groups cap)."
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_group_mining_result_matches_manual_scan() {
        let height = 1u64;
        let block_intro = BlockIntro::default().serialize();
        let nonce_start = 11u32;
        let nonce_space = 256u32;

        let (best_nonce, best_hash) = block_mining_runtime::do_group_block_mining(
            height,
            block_intro.clone(),
            nonce_start,
            nonce_space,
        );

        let mut manual_nonce = 0u32;
        let mut manual_hash = [255u8; 32];
        let mut intro = block_intro;
        for nonce in nonce_start..nonce_start + nonce_space {
            intro[79..83].copy_from_slice(&nonce.to_be_bytes());
            let hx = x16rs::block_hash(height, &intro);
            if crate::hash_util::hash_more_power(&hx, &manual_hash) {
                manual_hash = hx;
                manual_nonce = nonce;
            }
        }

        assert_eq!(best_nonce, manual_nonce);
        assert_eq!(best_hash, manual_hash);
    }
}
