//! HIP-23 production-path smoke tests (`fast_sync = false`).
//! Validates signature verification, duplicate-tx rejection, and fee-address rules
//! in addition to pattern semantics covered by the fast_sync harness.
//!
//! Run: cargo test hip23_production_ -- --nocapture

mod common;

use basis::interface::{StateOperat, Transaction, TxExec};
use common::hip23::*;
use field::*;
use mint::action::AssetCreate;
use mint::genesis;
use protocol::action::*;
use protocol::tex::*;
use sys::Account;

#[test]
fn hip23_production_p1_tex_swap_succeeds() {
    init_setup();
    let main_acc = Account::create_by("hip23-prod-p1-main").unwrap();
    let pay_acc = Account::create_by("hip23-prod-p1-pay").unwrap();
    let get_acc = Account::create_by("hip23-prod-p1-get").unwrap();
    let main = addr_of(&main_acc);
    let pay = addr_of(&pay_acc);
    let get = addr_of(&get_acc);
    const SERIAL: u64 = 2501;

    let (pay_tex, get_tex) = build_balanced_tex_swap(&pay_acc, &get_acc, 100_000_000, SERIAL, 10);
    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        99,
    );

    let mut ctx = make_ctx_strict(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000_000);
    seed_hac(&mut ctx, &pay, 100);
    seed_asset(&mut ctx, &pay, SERIAL, 10);

    tx.execute(&mut ctx).unwrap();
    assert_eq!(hac_mei(&mut ctx, &get), 1);
    assert_eq!(asset_amt(&mut ctx, &get, SERIAL), 10);
}

#[test]
fn hip23_production_p2_height_guarded_payment() {
    init_setup();
    let main_acc = Account::create_by("hip23-prod-p2-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut guard = HeightScope::new();
    guard.start = BlockHeight::from(TEST_HEIGHT);
    guard.end = BlockHeight::from(TEST_HEIGHT + 500);
    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient.clone());
    transfer.hacash = Amount::mei(8);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(guard), Box::new(transfer)],
        0,
    );

    let mut ctx = make_ctx_strict(TEST_HEIGHT + 10, tx.as_read());
    seed_hac(&mut ctx, &main, 500);
    tx.execute(&mut ctx).unwrap();
    assert_eq!(hac_mei(&mut ctx, &recipient), 8);
}

#[test]
fn hip23_production_p3_balance_floor_transfer() {
    init_setup();
    let main_acc = Account::create_by("hip23-prod-p3-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient.clone());
    transfer.hacash = Amount::mei(50);

    let mut floor = BalanceFloor::new();
    floor.addr = AddrOrPtr::from_addr(main);
    floor.hacash = Amount::mei(900);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(transfer), Box::new(floor)],
        0,
    );

    let mut ctx = make_ctx_strict(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000);
    tx.execute(&mut ctx).unwrap();
    assert_eq!(hac_mei(&mut ctx, &main), 950 - TX_FEE_MEI);
}

#[test]
fn hip23_production_p4_asset_create_and_tex() {
    init_setup();
    let main_acc = Account::create_by("hip23-prod-p4-main").unwrap();
    let issuer_acc = Account::create_by("hip23-prod-p4-issuer").unwrap();
    let buyer_acc = Account::create_by("hip23-prod-p4-buyer").unwrap();
    let main = addr_of(&main_acc);
    let issuer = addr_of(&issuer_acc);
    let buyer = addr_of(&buyer_acc);
    const SERIAL: u64 = 2504;

    let mut create = AssetCreate::new();
    create.metadata = AssetSmelt {
        serial: Fold64::from(SERIAL).unwrap(),
        supply: Fold64::from(500).unwrap(),
        decimal: Uint1::from(0),
        issuer,
        ticket: BytesW1::from_str("PRD").unwrap(),
        name: BytesW1::from_str("Prod").unwrap(),
    };
    create.protocol_cost = genesis::block_reward(TEST_HEIGHT);

    let tx_create = build_signed_type3(&main_acc, vec![Box::new(create)], 0);
    let mut ctx = make_ctx_strict(TEST_HEIGHT, tx_create.as_read());
    seed_hac(&mut ctx, &main, 1_000_000);
    tx_create.execute(&mut ctx).unwrap();

    let mut issuer_tex = TexCellAct::create_by(issuer);
    issuer_tex
        .add_cell(Box::new(CellTrsAssetPay::new(
            AssetAmt::from(SERIAL, 100).unwrap(),
        )))
        .unwrap();
    issuer_tex.do_sign(&issuer_acc).unwrap();

    let mut buyer_tex = TexCellAct::create_by(buyer);
    buyer_tex
        .add_cell(Box::new(CellTrsAssetGet::new(
            AssetAmt::from(SERIAL, 100).unwrap(),
        )))
        .unwrap();
    buyer_tex.do_sign(&buyer_acc).unwrap();

    let tx_tex = build_signed_type3(
        &main_acc,
        vec![Box::new(issuer_tex), Box::new(buyer_tex)],
        99,
    );
    let mut tex_ctx = make_ctx_strict_persisted(
        TEST_HEIGHT,
        ctx.state().clone_state(),
        tx_tex.as_read(),
    );
    tx_tex.execute(&mut tex_ctx).unwrap();
    assert_eq!(asset_amt(&mut tex_ctx, &buyer, SERIAL), 100);
}

#[test]
fn hip23_production_p5_ast_conditional_settlement() {
    init_setup();
    let main_acc = Account::create_by("hip23-prod-p5-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut cond_guard = HeightScope::new();
    cond_guard.start = BlockHeight::from(TEST_HEIGHT);
    cond_guard.end = BlockHeight::from(TEST_HEIGHT + 200);
    let cond = AstSelect::create_by(1, 1, vec![Box::new(cond_guard)]);
    let br_if = AstSelect::create_list(vec![Box::new(HacToTrs::create_by(
        recipient.clone(),
        Amount::mei(4),
    ))]);
    let act = AstIf::create_by(cond, br_if, AstSelect::nop());

    let tx = build_signed_type3(&main_acc, vec![Box::new(act)], 17);
    let mut ctx = make_ctx_strict(TEST_HEIGHT + 5, tx.as_read());
    seed_hac(&mut ctx, &main, 200);
    tx.execute(&mut ctx).unwrap();
    assert_eq!(hac_mei(&mut ctx, &recipient), 4);
}

#[test]
fn hip23_production_duplicate_tx_rejected() {
    init_setup();
    let main_acc = Account::create_by("hip23-prod-dup-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient);
    transfer.hacash = Amount::mei(1);

    let tx = build_signed_type3(&main_acc, vec![Box::new(transfer)], 0);
    let mut ctx = make_ctx_strict(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 100);
    tx.execute(&mut ctx).unwrap();

    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "already exists");
}

#[test]
fn hip23_production_tampered_main_signature_rejected() {
    init_setup();
    let main_acc = Account::create_by("hip23-prod-sig-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient);
    transfer.hacash = Amount::mei(2);

    let mut tx = build_signed_type3(&main_acc, vec![Box::new(transfer)], 0);
    let mut sig_bytes = *tx.signs[0].signature.as_array();
    sig_bytes[0] ^= 0x01;
    tx.signs[0].signature = Fixed64::from(sig_bytes);

    let mut ctx = make_ctx_strict(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 100);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(
        err.contains("signature") || err.contains("verify"),
        "{err}"
    );
}