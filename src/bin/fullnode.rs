// use basis::server::ApiCtx;
use app::fullnode::FullnodeBuilder;
use app::{fullnode::NilScaner, *};
use basis::config::*;
use basis::interface::*;
use chain::*;
use mint::HacashMinter;
use node::{core::HacashNode, memtxpool::*};
use server::*;
use sys::*;

/*

* fullnode main
*/
#[allow(dead_code)]
fn main() {
    println!(
        "[Version] full node v{}, build time: {}, database type: {}.",
        HACASH_NODE_VERSION, HACASH_NODE_BUILD_TIME, DB_VERSION
    );

    if let Err(e) = run() {
        println!("[Fatal] {}", e);
        std::process::exit(1);
    }
}

pub fn run() -> Rerr {
    run_with_scaner("./hacash.config.ini", Box::new(NilScaner {}))
}

pub fn run_with_config(cnfpath: &str) -> Rerr {
    run_with_scaner(cnfpath, Box::new(NilScaner {}))
}

pub fn install_standard_fullnode_stack() -> Rerr {
    let mut setup = protocol::setup::new_standard_protocol_setup(x16rs::block_hash);
    mint::setup::register_protocol_extensions(&mut setup);
    vm::setup::register_protocol_extensions(&mut setup);
    protocol::setup::install_once(setup);
    Ok(())
}

pub fn run_with_scaner(cnfpath: &str, scan: Box<dyn Scaner>) -> Rerr {
    install_standard_fullnode_stack()?;

    // scan api
    server::setup::api_servicer(scan.api_services());

    let mut builder = FullnodeBuilder::from_config_path(cnfpath)?;
    builder.install_ctrlc(true).scaner(scan);

    // Configure global VM contract cache pool (performance-only).
    let engcnf = builder.engine_conf();
    let size_mb = engcnf.contract_cache_size;
    let bytes = if size_mb.is_finite() && size_mb > 0.0 {
        (size_mb * 1024.0 * 1024.0) as usize
    } else {
        0
    };
    vm::configure_contract_cache(vm::machine::ContractCacheConfig {
        max_bytes: bytes,
        ..Default::default()
    });

    builder
        .diskdb(|dir| Box::new(db::DiskKV::open(dir)))
        .txpool(build_txpool)
        .minter(|ini| Ok(Box::new(HacashMinter::create(ini))))
        .engine(|dbfn, cnf, minter, scaner| {
            Ok(Box::new(ChainEngine::open(
                dbfn, cnf, minter, scaner, DB_VERSION,
            )))
        })
        .hnoder(|ini, txpool, engine| Ok(Box::new(HacashNode::open(ini, txpool, engine))))
        .server(|ini, hnoder| {
            #[allow(unused_mut)]
            let mut services: Vec<std::sync::Arc<dyn ApiService>> = vec![mint::api::service()];
            services.push(vm::api::service());
            let debug_open = ServerConf::new(ini).debug_open;
            Ok(Box::new(HttpServer::open(
                ini,
                hnoder.clone(),
                server::router(hnoder, vec![], services, debug_open),
            )))
        })
        .app(diabider::start_diamond_auto_bidding);

    // start run
    builder.run()
}

fn build_txpool(engcnf: &EngineConf) -> Ret<Box<dyn TxPool>> {
    let mut tpmaxs = maybe!(
        engcnf.miner_enable,
        vec![2000, 100], // miner node
        vec![10, 10]     // normal node
    );
    let fpmds = vec![true, false]; // is sort by fee_purity, normal or diamint
    cover(&mut tpmaxs, &engcnf.txpool_maxs);
    Ok(Box::new(MemTxPool::new(
        engcnf.lowest_fee_purity,
        tpmaxs,
        fpmds,
    )))
}
