//! HIP-23 happy-path regression tests.
//! See doc/HIP23.md. Adversarial cases: hip23_pattern_adversarial.rs

mod common;

use basis::interface::{StateOperat, Transaction, TxExec};
use common::hip23::*;
use field::*;
use mint::action::AssetCreate;
use mint::genesis;
use protocol::action::*;
use protocol::tex::*;
use protocol::transaction::create_tx_info;
use sys::Account;
use testkit::sim::context::make_ctx_with_state;

#[test]
fn hip23_pattern_1_atomic_tex_swap() {
    init_setup_once();
    let main_acc = Account::create_by("hip23-p1-main").unwrap();
    let pay_acc = Account::create_by("hip23-p1-pay").unwrap();
    let get_acc = Account::create_by("hip23-p1-get").unwrap();
    let main = addr_of(&main_acc);
    let pay = addr_of(&pay_acc);
    let get = addr_of(&get_acc);
    const SERIAL: u64 = 2301;

    let (pay_tex, get_tex) = build_balanced_tex_swap(&pay_acc, &get_acc, 100_000_000, SERIAL, 50);
    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        99,
    );

    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000_000);
    seed_hac(&mut ctx, &pay, 1_000);
    seed_asset(&mut ctx, &pay, SERIAL, 50);

    tx.execute(&mut ctx).unwrap();

    assert_eq!(hac_mei(&mut ctx, &get), 1);
    assert_eq!(asset_amt(&mut ctx, &get, SERIAL), 50);
    assert_eq!(asset_amt(&mut ctx, &pay, SERIAL), 0);
}

#[test]
fn hip23_pattern_2_height_guarded_payment() {
    init_setup_once();
    let main_acc = Account::create_by("hip23-p2-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let window_start = TEST_HEIGHT;
    let window_end = TEST_HEIGHT + 1_000;

    let mut guard = HeightScope::new();
    guard.start = BlockHeight::from(window_start);
    guard.end = BlockHeight::from(window_end);

    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient.clone());
    transfer.hacash = Amount::mei(10);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(guard), Box::new(transfer)],
        0,
    );

    let mut fail_ctx = make_ctx(window_start - 1, tx.as_read());
    seed_hac(&mut fail_ctx, &main, 1_000);
    let err = tx.execute(&mut fail_ctx).unwrap_err();
    assert_err_contains(&err, "submitted in height between");

    let mut ok_ctx = make_ctx(window_start + 100, tx.as_read());
    seed_hac(&mut ok_ctx, &main, 1_000);
    tx.execute(&mut ok_ctx).unwrap();
    assert_eq!(hac_mei(&mut ok_ctx, &main), 1_000 - 10 - TX_FEE_MEI);
    assert_eq!(hac_mei(&mut ok_ctx, &recipient), 10);
}

#[test]
fn hip23_pattern_3_balance_floor_protected_transfer() {
    init_setup_once();
    let main_acc = Account::create_by("hip23-p3-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient.clone());
    transfer.hacash = Amount::mei(150);

    let mut floor = BalanceFloor::new();
    floor.addr = AddrOrPtr::from_addr(main);
    floor.hacash = Amount::mei(900);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(transfer), Box::new(floor)],
        0,
    );

    let mut fail_ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut fail_ctx, &main, 1_000);
    let err = tx.execute(&mut fail_ctx).unwrap_err();
    assert_err_contains(&err, "lower than floor");

    let mut ok_transfer = HacToTrs::new();
    ok_transfer.to = AddrOrPtr::from_addr(recipient);
    ok_transfer.hacash = Amount::mei(100);

    let mut ok_floor = BalanceFloor::new();
    ok_floor.addr = AddrOrPtr::from_addr(main);
    ok_floor.hacash = Amount::mei(900);

    let tx_ok = build_signed_type3(
        &main_acc,
        vec![Box::new(ok_transfer), Box::new(ok_floor)],
        0,
    );

    let mut ok_ctx = make_ctx(TEST_HEIGHT, tx_ok.as_read());
    seed_hac(&mut ok_ctx, &main, 1_000);
    tx_ok.execute(&mut ok_ctx).unwrap();
    assert_eq!(hac_mei(&mut ok_ctx, &main), 900 - TX_FEE_MEI);
}

#[test]
fn hip23_pattern_4_asset_create_plus_tex() {
    init_setup_once();
    let main_acc = Account::create_by("hip23-p4-main").unwrap();
    let issuer_acc = Account::create_by("hip23-p4-issuer").unwrap();
    let buyer_acc = Account::create_by("hip23-p4-buyer").unwrap();
    let main = addr_of(&main_acc);
    let issuer = addr_of(&issuer_acc);
    let buyer = addr_of(&buyer_acc);
    const SERIAL: u64 = 2304;

    let mut create = AssetCreate::new();
    create.metadata = AssetSmelt {
        serial: Fold64::from(SERIAL).unwrap(),
        supply: Fold64::from(10_000).unwrap(),
        decimal: Uint1::from(2),
        issuer,
        ticket: BytesW1::from_str("USDT").unwrap(),
        name: BytesW1::from_str("Tether").unwrap(),
    };
    create.protocol_cost = genesis::block_reward(TEST_HEIGHT);

    let mut issuer_tex = TexCellAct::create_by(issuer);
    issuer_tex
        .add_cell(Box::new(CellTrsAssetPay::new(
            AssetAmt::from(SERIAL, 500).unwrap(),
        )))
        .unwrap();
    issuer_tex.do_sign(&issuer_acc).unwrap();

    let mut buyer_tex = TexCellAct::create_by(buyer);
    buyer_tex
        .add_cell(Box::new(CellTrsAssetGet::new(
            AssetAmt::from(SERIAL, 500).unwrap(),
        )))
        .unwrap();
    buyer_tex.do_sign(&buyer_acc).unwrap();

    let tx_create = build_signed_type3(&main_acc, vec![Box::new(create)], 0);
    let tx_tex = build_signed_type3(
        &main_acc,
        vec![Box::new(issuer_tex), Box::new(buyer_tex)],
        99,
    );

    let mut ctx = make_ctx(TEST_HEIGHT, tx_create.as_read());
    seed_hac(&mut ctx, &main, 1_000_000);
    tx_create.execute(&mut ctx).unwrap();
    assert_eq!(asset_amt(&mut ctx, &issuer, SERIAL), 10_000);

    let persisted = ctx.state().clone_state();
    let mut tex_ctx = make_ctx_with_state(
        {
            let mut env = basis::component::Env::default();
            env.chain.fast_sync = true;
            env.block.height = TEST_HEIGHT;
            env.tx = create_tx_info(tx_tex.as_read());
            env
        },
        persisted,
        tx_tex.as_read(),
    );
    tx_tex.execute(&mut tex_ctx).unwrap();

    assert_eq!(asset_amt(&mut tex_ctx, &buyer, SERIAL), 500);
    assert_eq!(asset_amt(&mut tex_ctx, &issuer, SERIAL), 10_000 - 500);
}

#[test]
fn hip23_pattern_5_ast_conditional_settlement() {
    init_setup_once();
    let main_acc = Account::create_by("hip23-p5-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let window_start = TEST_HEIGHT;
    let window_end = TEST_HEIGHT + 500;

    let mut cond_guard = HeightScope::new();
    cond_guard.start = BlockHeight::from(window_start);
    cond_guard.end = BlockHeight::from(window_end);
    let cond = AstSelect::create_by(1, 1, vec![Box::new(cond_guard)]);

    let br_if = AstSelect::create_list(vec![Box::new(HacToTrs::create_by(
        recipient.clone(),
        Amount::mei(5),
    ))]);
    let act = AstIf::create_by(cond, br_if, AstSelect::nop());

    let tx = build_signed_type3(&main_acc, vec![Box::new(act)], 17);

    let mut else_ctx = make_ctx(window_start - 1, tx.as_read());
    seed_hac(&mut else_ctx, &main, 1_000);
    tx.execute(&mut else_ctx).unwrap();
    assert_eq!(hac_mei(&mut else_ctx, &main), 1_000 - TX_FEE_MEI);
    assert_eq!(hac_mei(&mut else_ctx, &recipient), 0);

    let mut if_ctx = make_ctx(window_start + 10, tx.as_read());
    seed_hac(&mut if_ctx, &main, 1_000);
    tx.execute(&mut if_ctx).unwrap();
    assert_eq!(hac_mei(&mut if_ctx, &main), 1_000 - 5 - TX_FEE_MEI);
    assert_eq!(hac_mei(&mut if_ctx, &recipient), 5);
}