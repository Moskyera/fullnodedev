//! HIP-23 audit suite: adversarial cases under production path (`fast_sync=false`).
//! Run: cargo test hip23_audit_ -- --nocapture

mod common;

use basis::interface::{Transaction, TxExec};
use common::hip23::*;
use field::*;
use mint::action::AssetCreate;
use protocol::action::*;
use protocol::tex::*;
use sys::Account;

#[test]
fn hip23_audit_strict_tex_imbalanced_rejected() {
    init_setup();
    let main_acc = Account::create_by("hip23-audit-imb-main").unwrap();
    let pay_acc = Account::create_by("hip23-audit-imb-pay").unwrap();
    let get_acc = Account::create_by("hip23-audit-imb-get").unwrap();
    let pay = addr_of(&pay_acc);

    let (pay_tex, _) = build_balanced_tex_swap(&pay_acc, &get_acc, 100_000_000, 0, 0);
    let mut get_tex = TexCellAct::create_by(addr_of(&get_acc));
    get_tex
        .add_cell(Box::new(CellTrsZhuGet::new(Fold64::from(1).unwrap())))
        .unwrap();
    get_tex.do_sign(&get_acc).unwrap();

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        0,
    );
    let mut ctx = make_ctx_strict(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_hac(&mut ctx, &pay, 10);

    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "settlement check failed");
}

#[test]
fn hip23_audit_strict_tex_tamper_rejected() {
    init_setup();
    let main_acc = Account::create_by("hip23-audit-sig-main").unwrap();
    let pay_acc = Account::create_by("hip23-audit-sig-pay").unwrap();
    let get_acc = Account::create_by("hip23-audit-sig-get").unwrap();
    let pay = addr_of(&pay_acc);

    let (mut pay_tex, get_tex) = build_balanced_tex_swap(&pay_acc, &get_acc, 100_000_000, 0, 0);
    pay_tex
        .add_cell(Box::new(CellTrsZhuPay::new(Fold64::from(1).unwrap())))
        .unwrap();

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        0,
    );
    let mut ctx = make_ctx_strict(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_hac(&mut ctx, &pay, 10);

    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "signature verification failed");
}

#[test]
fn hip23_audit_strict_height_outside_window_rejected() {
    init_setup();
    let main_acc = Account::create_by("hip23-audit-h-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut guard = HeightScope::new();
    guard.start = BlockHeight::from(TEST_HEIGHT);
    guard.end = BlockHeight::from(TEST_HEIGHT + 5);
    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient);
    transfer.hacash = Amount::mei(1);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(guard), Box::new(transfer)],
        0,
    );
    let mut ctx = make_ctx_strict(TEST_HEIGHT + 99, tx.as_read());
    seed_hac(&mut ctx, &main, 100);

    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "submitted in height between");
}

#[test]
fn hip23_audit_strict_wrong_protocol_cost_rejected() {
    init_setup();
    let main_acc = Account::create_by("hip23-audit-p4-main").unwrap();
    let issuer_acc = Account::create_by("hip23-audit-p4-issuer").unwrap();
    let main = addr_of(&main_acc);
    let issuer = addr_of(&issuer_acc);

    let mut create = AssetCreate::new();
    create.metadata = AssetSmelt {
        serial: Fold64::from(9901).unwrap(),
        supply: Fold64::from(100).unwrap(),
        decimal: Uint1::from(0),
        issuer,
        ticket: BytesW1::from_str("AUD").unwrap(),
        name: BytesW1::from_str("Audit").unwrap(),
    };
    create.protocol_cost = Amount::mei(1);

    let tx = build_signed_type3(&main_acc, vec![Box::new(create)], 0);
    let mut ctx = make_ctx_strict(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 2_000_000);

    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "Protocol fee must be");
}

#[test]
fn hip23_audit_strict_main_sig_tamper_rejected() {
    init_setup();
    let main_acc = Account::create_by("hip23-audit-main-sig").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient);
    transfer.hacash = Amount::mei(1);

    let mut tx = build_signed_type3(&main_acc, vec![Box::new(transfer)], 0);
    let mut sig_bytes = *tx.signs[0].signature.as_array();
    sig_bytes[0] ^= 0x01;
    tx.signs[0].signature = Fixed64::from(sig_bytes);

    let mut ctx = make_ctx_strict(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 100);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(err.contains("signature") || err.contains("verify"), "{err}");
}

#[test]
fn hip23_audit_strict_guard_only_precheck_rejected() {
    init_setup();
    let mut guard = HeightScope::new();
    guard.start = BlockHeight::from(TEST_HEIGHT);
    guard.end = BlockHeight::from(TEST_HEIGHT + 10);
    let actions: Vec<Box<dyn basis::interface::Action>> = vec![Box::new(guard)];
    let err = protocol::action::precheck_tx_actions(
        protocol::transaction::TransactionType3::TYPE,
        &actions,
    )
    .unwrap_err();
    assert_err_contains(&err, "all GUARD");
}