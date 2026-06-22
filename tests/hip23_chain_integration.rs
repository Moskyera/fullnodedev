//! HIP-23 chain-level integration: per-tx fork/merge semantics (`chain/src/check.rs`).
//! Run: cargo test hip23_chain_ -- --nocapture

mod common;

use basis::interface::{StateOperat, Transaction, TxExec};
use common::hip23::*;
use field::*;
use mint::action::AssetCreate;
use mint::genesis;
use protocol::action::*;
use protocol::tex::*;
use sys::Account;
use testkit::sim::state::ForkableMemState;

#[test]
fn hip23_chain_failed_tx_does_not_commit() {
    init_setup();
    let main_acc = Account::create_by("hip23-chain-fail-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient.clone());
    transfer.hacash = Amount::mei(50);

    let mut guard = HeightScope::new();
    guard.start = BlockHeight::from(TEST_HEIGHT);
    guard.end = BlockHeight::from(TEST_HEIGHT + 10);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(transfer), Box::new(guard)],
        0,
    );

    let mut state: Box<dyn basis::interface::State> =
        Box::new(ForkableMemState::default());
    seed_hac_chain(&mut state, &main, 1_000);

    let err = try_execute_tx_fork(TEST_HEIGHT + 100, false, tx.as_read(), &mut state).unwrap_err();
    assert_err_contains(&err, "submitted in height between");
    assert_eq!(hac_mei_chain(&state, &main), 1_000);
    assert_eq!(hac_mei_chain(&state, &recipient), 0);
}

#[test]
fn hip23_chain_successful_tx_commits() {
    init_setup();
    let main_acc = Account::create_by("hip23-chain-ok-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut guard = HeightScope::new();
    guard.start = BlockHeight::from(TEST_HEIGHT);
    guard.end = BlockHeight::from(TEST_HEIGHT + 500);
    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient.clone());
    transfer.hacash = Amount::mei(3);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(guard), Box::new(transfer)],
        0,
    );

    let mut state: Box<dyn basis::interface::State> =
        Box::new(ForkableMemState::default());
    seed_hac_chain(&mut state, &main, 200);
    try_execute_tx_fork(TEST_HEIGHT + 5, false, tx.as_read(), &mut state).unwrap();
    assert_eq!(hac_mei_chain(&state, &recipient), 3);
}

#[test]
fn hip23_chain_p4_tx_a_then_b_commits() {
    init_setup();
    let main_acc = Account::create_by("hip23-chain-p4-main").unwrap();
    let issuer_acc = Account::create_by("hip23-chain-p4-issuer").unwrap();
    let buyer_acc = Account::create_by("hip23-chain-p4-buyer").unwrap();
    let main = addr_of(&main_acc);
    let issuer = addr_of(&issuer_acc);
    let buyer = addr_of(&buyer_acc);
    const SERIAL: u64 = 8801;

    let mut create = AssetCreate::new();
    create.metadata = AssetSmelt {
        serial: Fold64::from(SERIAL).unwrap(),
        supply: Fold64::from(1_000).unwrap(),
        decimal: Uint1::from(0),
        issuer,
        ticket: BytesW1::from_str("CHN").unwrap(),
        name: BytesW1::from_str("Chain").unwrap(),
    };
    create.protocol_cost = genesis::block_reward(TEST_HEIGHT);

    let tx_a = build_signed_type3(&main_acc, vec![Box::new(create)], 0);
    let mut state: Box<dyn basis::interface::State> =
        Box::new(ForkableMemState::default());
    seed_hac_chain(&mut state, &main, 2_000_000);
    try_execute_tx_fork(TEST_HEIGHT, false, tx_a.as_read(), &mut state).unwrap();

    let mut issuer_tex = TexCellAct::create_by(issuer);
    issuer_tex
        .add_cell(Box::new(CellTrsAssetPay::new(
            AssetAmt::from(SERIAL, 50).unwrap(),
        )))
        .unwrap();
    issuer_tex.do_sign(&issuer_acc).unwrap();

    let mut buyer_tex = TexCellAct::create_by(buyer);
    buyer_tex
        .add_cell(Box::new(CellTrsAssetGet::new(
            AssetAmt::from(SERIAL, 50).unwrap(),
        )))
        .unwrap();
    buyer_tex.do_sign(&buyer_acc).unwrap();

    let tx_b = build_signed_type3(
        &main_acc,
        vec![Box::new(issuer_tex), Box::new(buyer_tex)],
        99,
    );
    try_execute_tx_fork(TEST_HEIGHT, false, tx_b.as_read(), &mut state).unwrap();
    assert_eq!(asset_amt_chain(&state, &buyer, SERIAL), 50);
}

#[test]
fn hip23_chain_sequential_fail_then_success_isolated() {
    init_setup();
    let main_acc = Account::create_by("hip23-chain-seq-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut bad_transfer = HacToTrs::new();
    bad_transfer.to = AddrOrPtr::from_addr(recipient.clone());
    bad_transfer.hacash = Amount::mei(100);
    let mut bad_guard = HeightScope::new();
    bad_guard.start = BlockHeight::from(TEST_HEIGHT);
    bad_guard.end = BlockHeight::from(TEST_HEIGHT + 1);
    let bad_tx = build_signed_type3(
        &main_acc,
        vec![Box::new(bad_transfer), Box::new(bad_guard)],
        0,
    );

    let mut good_guard = HeightScope::new();
    good_guard.start = BlockHeight::from(TEST_HEIGHT);
    good_guard.end = BlockHeight::from(TEST_HEIGHT + 100);
    let mut good_transfer = HacToTrs::new();
    good_transfer.to = AddrOrPtr::from_addr(recipient.clone());
    good_transfer.hacash = Amount::mei(2);
    let good_tx = build_signed_type3(
        &main_acc,
        vec![Box::new(good_guard), Box::new(good_transfer)],
        0,
    );

    let mut state: Box<dyn basis::interface::State> =
        Box::new(ForkableMemState::default());
    seed_hac_chain(&mut state, &main, 500);

    let bad_err =
        try_execute_tx_fork(TEST_HEIGHT + 50, false, bad_tx.as_read(), &mut state).unwrap_err();
    assert_err_contains(&bad_err, "submitted in height between");
    try_execute_tx_fork(TEST_HEIGHT + 5, false, good_tx.as_read(), &mut state).unwrap();
    assert_eq!(hac_mei_chain(&state, &recipient), 2);
    // Failed tx does not commit (no fee); successful tx commits once.
    assert_eq!(hac_mei_chain(&state, &main), 500 - 2 - TX_FEE_MEI);
}

fn seed_hac_chain(state: &mut Box<dyn basis::interface::State>, addr: &Address, mei: u64) {
    let taken = std::mem::replace(state, Box::new(ForkableMemState::default()));
    *state = with_persisted_state(TEST_HEIGHT, taken, |ctx| seed_hac(ctx, addr, mei));
}

fn hac_mei_chain(state: &Box<dyn basis::interface::State>, addr: &Address) -> u64 {
    let mut mei = 0u64;
    let _ = with_persisted_state(TEST_HEIGHT, state.clone_state(), |ctx| {
        mei = hac_mei(ctx, addr);
    });
    mei
}

fn asset_amt_chain(state: &Box<dyn basis::interface::State>, addr: &Address, serial: u64) -> u64 {
    let mut amt = 0u64;
    let _ = with_persisted_state(TEST_HEIGHT, state.clone_state(), |ctx| {
        amt = asset_amt(ctx, addr, serial);
    });
    amt
}