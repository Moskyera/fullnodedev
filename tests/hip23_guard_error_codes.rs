//! Locks HIP-23 error string → stable code mapping for indexers (F-007 mitigation).
//! Run: cargo test hip23_guard_error_ -- --nocapture

mod common;

use basis::interface::{Transaction, TxExec};
use common::hip23::*;
use common::hip23_errors::*;
use field::*;
use mint::action::AssetCreate;
use protocol::action::*;
use protocol::tex::*;
use sys::Account;

#[test]
fn hip23_guard_error_height_outside_maps_to_code() {
    init_setup();
    let main_acc = Account::create_by("hip23-err-h-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut guard = HeightScope::new();
    guard.start = BlockHeight::from(TEST_HEIGHT);
    guard.end = BlockHeight::from(TEST_HEIGHT + 1);
    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient);
    transfer.hacash = Amount::mei(1);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(guard), Box::new(transfer)],
        0,
    );
    let mut ctx = make_ctx(TEST_HEIGHT + 99, tx.as_read());
    seed_hac(&mut ctx, &main, 50);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_eq!(
        classify_error(&err),
        Hip23ErrorCode::HeightOutsideWindow
    );
    assert_eq!(
        error_code_name(classify_error(&err)),
        "height_outside_window"
    );
}

#[test]
fn hip23_guard_error_floor_below_maps_to_code() {
    init_setup();
    let main_acc = Account::create_by("hip23-err-f-main").unwrap();
    let main = addr_of(&main_acc);

    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(field::ADDRESS_TWOX.clone());
    transfer.hacash = Amount::mei(900);
    let mut floor = BalanceFloor::new();
    floor.addr = AddrOrPtr::from_addr(main);
    floor.hacash = Amount::mei(500);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(transfer), Box::new(floor)],
        0,
    );
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_eq!(classify_error(&err), Hip23ErrorCode::BalanceBelowFloor);
}

#[test]
fn hip23_guard_error_duplicate_tx_maps_to_code() {
    init_setup();
    let main_acc = Account::create_by("hip23-err-dup-main").unwrap();
    let main = addr_of(&main_acc);
    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(field::ADDRESS_TWOX.clone());
    transfer.hacash = Amount::mei(1);

    let tx = build_signed_type3(&main_acc, vec![Box::new(transfer)], 0);
    let mut ctx = make_ctx_strict(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 100);
    tx.execute(&mut ctx).unwrap();
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_eq!(classify_error(&err), Hip23ErrorCode::DuplicateTx);
}

#[test]
fn hip23_guard_error_protocol_fee_maps_to_code() {
    init_setup();
    let main_acc = Account::create_by("hip23-err-fee-main").unwrap();
    let issuer = addr_of(&Account::create_by("hip23-err-fee-iss").unwrap());

    let mut create = AssetCreate::new();
    create.metadata = AssetSmelt {
        serial: Fold64::from(9910).unwrap(),
        supply: Fold64::from(10).unwrap(),
        decimal: Uint1::from(0),
        issuer,
        ticket: BytesW1::from_str("ERR").unwrap(),
        name: BytesW1::from_str("Err").unwrap(),
    };
    create.protocol_cost = Amount::mei(1);

    let tx = build_signed_type3(&main_acc, vec![Box::new(create)], 0);
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_eq!(classify_error(&err), Hip23ErrorCode::ProtocolFeeMismatch);
}

#[test]
fn hip23_guard_error_tex_settlement_maps_to_code() {
    init_setup();
    let main_acc = Account::create_by("hip23-err-tex-main").unwrap();
    let pay_acc = Account::create_by("hip23-err-tex-pay").unwrap();
    let get_acc = Account::create_by("hip23-err-tex-get").unwrap();
    let pay = addr_of(&pay_acc);

    let (pay_tex, mut get_tex) =
        build_balanced_tex_swap(&pay_acc, &get_acc, 100_000_000, 0, 0);
    get_tex
        .add_cell(Box::new(CellTrsZhuGet::new(Fold64::from(1).unwrap())))
        .unwrap();
    get_tex.do_sign(&get_acc).unwrap();

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        0,
    );
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_hac(&mut ctx, &pay, 10);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_eq!(classify_error(&err), Hip23ErrorCode::TexSettlementFail);
}