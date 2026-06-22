//! Shared helpers for HIP-23 integration tests.
#![allow(dead_code)]

use basis::component::Env;
use basis::interface::{Action, Context, Transaction, TransactionRead};
use field::*;
use protocol::state::CoreState;
use protocol::tex::*;
use protocol::transaction::*;
use sys::Account;
use testkit::sim::context::make_ctx_with_state;
use testkit::sim::integration::enable_mint_setup;

pub const TEST_HEIGHT: u64 = protocol::upgrade::ONLINE_OPEN_HEIGHT + 10_000;
pub const TX_FEE_MEI: u64 = 1;

pub fn init_setup() {
    enable_mint_setup();
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
    let mut env = Env::default();
    // fast_sync skips sig/duplicate-tx/fee checks; see HIP23.md §11.
    env.chain.fast_sync = true;
    env.chain.id = chain_id;
    env.block.height = height;
    env.tx = create_tx_info(tx);
    make_ctx_with_state(env, Box::new(testkit::sim::state::ForkableMemState::default()), tx)
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