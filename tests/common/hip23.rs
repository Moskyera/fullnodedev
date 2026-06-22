//! Shared helpers for HIP-23 integration tests.
#![allow(dead_code)]

use basis::component::Env;
use std::sync::Arc;

use basis::interface::{Action, Context, State, StateOperat, Transaction, TransactionRead, TxExec};
use field::{Parse, Serialize, *};
use protocol::state::CoreState;
use protocol::tex::*;
use protocol::transaction::*;
use sys::Account;
use testkit::sim::context::make_ctx_with_state;
use testkit::sim::integration::ensure_standard_protocol_setup_for_tests;
use testkit::sim::state::ForkableMemState;

pub const TEST_HEIGHT: u64 = protocol::upgrade::ONLINE_OPEN_HEIGHT + 10_000;
/// Fee deducted from main on successful Type3 execute (matches `build_signed_type3` wire fee).
pub const TX_FEE_MEI: u64 = 1;

pub fn init_setup() {
    ensure_standard_protocol_setup_for_tests(x16rs::block_hash, false);
}

pub fn addr_of(acc: &Account) -> Address {
    Address::from(*acc.address())
}

pub fn make_ctx<'a>(height: u64, tx: &'a dyn TransactionRead) -> protocol::context::ContextInst<'a> {
    make_ctx_chain(height, 0, tx)
}

pub fn make_ctx_chain<'a>(
    height: u64,
    chain_id: u32,
    tx: &'a dyn TransactionRead,
) -> protocol::context::ContextInst<'a> {
    make_ctx_with_opts(height, chain_id, true, tx, Box::new(ForkableMemState::default()))
}

pub fn make_ctx_persisted<'a>(
    height: u64,
    state: Box<dyn basis::interface::State>,
    tx: &'a dyn TransactionRead,
) -> protocol::context::ContextInst<'a> {
    make_ctx_with_opts(height, 0, true, tx, state)
}

/// Production-like path: signature verification, duplicate-tx check, fee-address rules.
pub fn make_ctx_strict<'a>(
    height: u64,
    tx: &'a dyn TransactionRead,
) -> protocol::context::ContextInst<'a> {
    make_ctx_strict_chain(height, 0, tx)
}

pub fn make_ctx_strict_chain<'a>(
    height: u64,
    chain_id: u32,
    tx: &'a dyn TransactionRead,
) -> protocol::context::ContextInst<'a> {
    make_ctx_with_opts(height, chain_id, false, tx, Box::new(ForkableMemState::default()))
}

pub fn make_ctx_strict_persisted<'a>(
    height: u64,
    state: Box<dyn basis::interface::State>,
    tx: &'a dyn TransactionRead,
) -> protocol::context::ContextInst<'a> {
    make_ctx_with_opts(height, 0, false, tx, state)
}

fn make_ctx_with_opts<'a>(
    height: u64,
    chain_id: u32,
    fast_sync: bool,
    tx: &'a dyn TransactionRead,
    state: Box<dyn basis::interface::State>,
) -> protocol::context::ContextInst<'a> {
    let mut env = Env::default();
    // fast_sync skips sig/duplicate-tx/fee checks; see HIP23.md §11.
    env.chain.fast_sync = fast_sync;
    env.chain.id = chain_id;
    env.block.height = height;
    env.tx = create_tx_info(tx);
    make_ctx_with_state(env, state, tx)
}

pub fn seed_hac(ctx: &mut dyn Context, addr: &Address, mei: u64) {
    let mut state = CoreState::wrap(ctx.state());
    let mut bls = state.balance(addr).unwrap_or_default();
    bls.hacash = Amount::mei(mei);
    state.balance_set(addr, &bls);
}

pub fn seed_sat(ctx: &mut dyn Context, addr: &Address, sat: u64) {
    let mut state = CoreState::wrap(ctx.state());
    let mut bls = state.balance(addr).unwrap_or_default();
    bls.satoshi = SatoshiAuto::from_satoshi(&Satoshi::from(sat));
    state.balance_set(addr, &bls);
}

pub fn seed_diamond_owned(ctx: &mut dyn Context, name: &DiamondName, owner: &Address) {
    let mut state = CoreState::wrap(ctx.state());
    let mut dia = DiamondSto::new();
    dia.status = DIAMOND_STATUS_NORMAL;
    dia.address = *owner;
    state.diamond_set(name, &dia);
    let mut bls = state.balance(owner).unwrap_or_default();
    let cur = bls
        .diamond
        .to_diamond()
        .map(|d| d.uint())
        .unwrap_or(0);
    bls.diamond = DiamondNumberAuto::from_diamond(&DiamondNumber::from(cur + 1));
    state.balance_set(owner, &bls);
}

pub fn seed_asset(ctx: &mut dyn Context, owner: &Address, serial: u64, amount: u64) {
    let mut state = CoreState::wrap(ctx.state());
    let serial_f = Fold64::from(serial).unwrap();
    state.asset_set(
        &serial_f,
        &AssetSmelt {
            serial: serial_f,
            supply: Fold64::from(1_000_000).unwrap(),
            decimal: Uint1::from(2),
            issuer: *owner,
            ticket: BytesW1::from_str("HIP23").unwrap(),
            name: BytesW1::from_str("HIP23 Asset").unwrap(),
        },
    );
    let mut bls = state.balance(owner).unwrap_or_default();
    bls.asset_set(AssetAmt::from(serial, amount).unwrap())
        .unwrap();
    state.balance_set(owner, &bls);
}

pub fn hac_mei(ctx: &mut dyn Context, addr: &Address) -> u64 {
    CoreState::wrap(ctx.state())
        .balance(addr)
        .unwrap_or_default()
        .hacash
        .to_mei_u64()
        .unwrap()
}

pub fn sat_amount(ctx: &mut dyn Context, addr: &Address) -> u64 {
    CoreState::wrap(ctx.state())
        .balance(addr)
        .unwrap_or_default()
        .satoshi
        .to_satoshi()
        .uint()
}

pub fn diamond_count(ctx: &mut dyn Context, addr: &Address) -> u32 {
    CoreState::wrap(ctx.state())
        .balance(addr)
        .unwrap_or_default()
        .diamond
        .to_diamond()
        .map(|d| d.uint())
        .unwrap_or(0)
}

pub fn asset_amt(ctx: &mut dyn Context, addr: &Address, serial: u64) -> u64 {
    CoreState::wrap(ctx.state())
        .balance(addr)
        .unwrap_or_default()
        .asset(Fold64::from(serial).unwrap())
        .map(|a| a.amount.uint())
        .unwrap_or(0)
}

pub fn build_signed_type3(
    main_acc: &Account,
    actions: Vec<Box<dyn Action>>,
    gas_max: u8,
) -> TransactionType3 {
    let main = addr_of(main_acc);
    let mut tx = TransactionType3::new_by(main, Amount::unit238(1_000_000), 1_730_000_000);
    tx.gas_max = Uint1::from(gas_max);
    for act in actions {
        tx.actions.push(act).unwrap();
    }
    tx.fill_sign(main_acc).unwrap();
    tx
}

pub fn build_balanced_tex_swap(
    pay_acc: &Account,
    get_acc: &Account,
    hac_zhu: u64,
    serial: u64,
    asset_amt: u64,
) -> (TexCellAct, TexCellAct) {
    let pay = addr_of(pay_acc);
    let get = addr_of(get_acc);

    let mut pay_tex = TexCellAct::create_by(pay);
    if hac_zhu > 0 {
        pay_tex
            .add_cell(Box::new(CellTrsZhuPay::new(Fold64::from(hac_zhu).unwrap())))
            .unwrap();
    }
    if asset_amt > 0 {
        pay_tex
            .add_cell(Box::new(CellTrsAssetPay::new(
                AssetAmt::from(serial, asset_amt).unwrap(),
            )))
            .unwrap();
    }
    pay_tex.do_sign(pay_acc).unwrap();

    let mut get_tex = TexCellAct::create_by(get);
    if hac_zhu > 0 {
        get_tex
            .add_cell(Box::new(CellTrsZhuGet::new(Fold64::from(hac_zhu).unwrap())))
            .unwrap();
    }
    if asset_amt > 0 {
        get_tex
            .add_cell(Box::new(CellTrsAssetGet::new(
                AssetAmt::from(serial, asset_amt).unwrap(),
            )))
            .unwrap();
    }
    get_tex.do_sign(get_acc).unwrap();

    (pay_tex, get_tex)
}

pub fn assert_err_contains(err: &str, needle: &str) {
    assert!(
        err.contains(needle),
        "expected '{needle}' in error: {err}"
    );
}

/// Run a closure against persisted chain state (for seeding / balance reads).
pub fn with_persisted_state<F>(height: u64, state: Box<dyn State>, f: F) -> Box<dyn State>
where
    F: FnOnce(&mut dyn Context),
{
    let main_acc = Account::create_by("hip23-state-helper").unwrap();
    let tx = build_signed_type3(&main_acc, vec![], 0);
    let mut ctx = make_ctx_persisted(height, state, tx.as_read());
    f(&mut ctx);
    let (sta, _) = ctx.release();
    sta
}

/// Round-trip a signed TEX bundle through wire bytes (same signature, new instance).
pub fn clone_tex_wire(tex: &TexCellAct) -> TexCellAct {
    let mut buf = Vec::new();
    tex.serialize_to(&mut buf);
    let mut out = TexCellAct::new();
    out.parse(&buf).unwrap();
    out
}

/// Per-tx state fork/merge semantics (success commits, failure discards).
pub fn try_execute_tx_fork(
    height: u64,
    fast_sync: bool,
    tx: &dyn TransactionRead,
    state: &mut Box<dyn State>,
) -> Result<(), String> {
    let parent: Arc<Box<dyn State>> = state.clone_state().into();
    let sub = parent.fork_sub(Arc::downgrade(&parent));
    let mut env = Env::default();
    env.chain.fast_sync = fast_sync;
    env.block.height = height;
    env.tx = create_tx_info(tx);
    let mut ctx = make_ctx_with_state(env, sub, tx);
    let exec_res = tx.execute(&mut ctx);
    let (sta, _) = ctx.release();
    match exec_res {
        Ok(()) => {
            state.merge_sub(sta);
            Ok(())
        }
        Err(e) => Err(e),
    }
}