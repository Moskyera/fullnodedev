//! HIP-23 adversarial / edge-case tests for bug hunting.
//! Complements hip23_pattern_regression.rs.

mod common;

use basis::interface::{Action, StateOperat, Transaction, TxExec};
use common::hip23::*;
use field::*;

use mint::action::AssetCreate;
use mint::genesis;
use protocol::action::*;
use protocol::tex::*;
use protocol::transaction::TransactionType3;
use sys::Account;

// ---------------------------------------------------------------------------
// P1 — TEX adversarial
// ---------------------------------------------------------------------------

#[test]
fn hip23_p1_tex_imbalanced_hac_amount_fails() {
    init_setup();
    let main_acc = Account::create_by("hip23-p1a-main").unwrap();
    let pay_acc = Account::create_by("hip23-p1a-pay").unwrap();
    let get_acc = Account::create_by("hip23-p1a-get").unwrap();
    let pay = addr_of(&pay_acc);
    let get = addr_of(&get_acc);

    let (pay_tex, _) = build_balanced_tex_swap(&pay_acc, &get_acc, 100_000_000, 0, 0);
    let mut get_tex = TexCellAct::create_by(get);
    get_tex
        .add_cell(Box::new(CellTrsZhuGet::new(Fold64::from(50_000_000).unwrap())))
        .unwrap();
    get_tex.do_sign(&get_acc).unwrap();

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        0,
    );
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_hac(&mut ctx, &pay, 1_000);

    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "coin settlement check failed");
    assert_eq!(hac_mei(&mut ctx, &get), 0);
}

#[test]
fn hip23_p1_tex_tampered_signature_fails() {
    init_setup();
    let main_acc = Account::create_by("hip23-p1b-main").unwrap();
    let pay_acc = Account::create_by("hip23-p1b-pay").unwrap();
    let get_acc = Account::create_by("hip23-p1b-get").unwrap();
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
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_hac(&mut ctx, &pay, 1_000);

    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "signature verification failed");
}

#[test]
fn hip23_p1_tex_insufficient_hac_balance_fails() {
    init_setup();
    let main_acc = Account::create_by("hip23-p1c-main").unwrap();
    let pay_acc = Account::create_by("hip23-p1c-pay").unwrap();
    let get_acc = Account::create_by("hip23-p1c-get").unwrap();
    let pay = addr_of(&pay_acc);

    let (pay_tex, get_tex) = build_balanced_tex_swap(&pay_acc, &get_acc, 100_000_000, 0, 0);
    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        0,
    );
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_hac(&mut ctx, &pay, 0);

    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(err.contains("insufficient") || err.contains("overflow"), "{err}");
}

#[test]
fn hip23_p1_tex_asset_cells_require_gas() {
    init_setup();
    let main_acc = Account::create_by("hip23-p1d-main").unwrap();
    let pay_acc = Account::create_by("hip23-p1d-pay").unwrap();
    let get_acc = Account::create_by("hip23-p1d-get").unwrap();
    let pay = addr_of(&pay_acc);
    const SERIAL: u64 = 2310;

    let (pay_tex, get_tex) = build_balanced_tex_swap(&pay_acc, &get_acc, 0, SERIAL, 10);
    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        0,
    );
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_asset(&mut ctx, &pay, SERIAL, 10);

    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "gas not initialized");
}

#[test]
fn hip23_p1_tex_hac_and_sat_dual_swap_succeeds() {
    init_setup();
    let main_acc = Account::create_by("hip23-p1e-main").unwrap();
    let pay_acc = Account::create_by("hip23-p1e-pay").unwrap();
    let get_acc = Account::create_by("hip23-p1e-get").unwrap();
    let pay = addr_of(&pay_acc);
    let get = addr_of(&get_acc);

    let mut pay_tex = TexCellAct::create_by(pay);
    pay_tex
        .add_cell(Box::new(CellTrsZhuPay::new(Fold64::from(100_000_000).unwrap())))
        .unwrap();
    pay_tex
        .add_cell(Box::new(CellTrsSatPay::new(Fold64::from(3).unwrap())))
        .unwrap();
    pay_tex.do_sign(&pay_acc).unwrap();

    let mut get_tex = TexCellAct::create_by(get);
    get_tex
        .add_cell(Box::new(CellTrsZhuGet::new(Fold64::from(100_000_000).unwrap())))
        .unwrap();
    get_tex
        .add_cell(Box::new(CellTrsSatGet::new(Fold64::from(3).unwrap())))
        .unwrap();
    get_tex.do_sign(&get_acc).unwrap();

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        0,
    );
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_hac(&mut ctx, &pay, 500);
    seed_sat(&mut ctx, &pay, 3);

    tx.execute(&mut ctx).unwrap();
    assert_eq!(hac_mei(&mut ctx, &get), 1);
    assert_eq!(sat_amount(&mut ctx, &get), 3);
}

#[test]
fn hip23_p1_tex_imbalanced_sat_fails() {
    init_setup();
    let main_acc = Account::create_by("hip23-p1g-main").unwrap();
    let pay_acc = Account::create_by("hip23-p1g-pay").unwrap();
    let get_acc = Account::create_by("hip23-p1g-get").unwrap();
    let pay = addr_of(&pay_acc);

    let mut pay_tex = TexCellAct::create_by(pay);
    pay_tex
        .add_cell(Box::new(CellTrsSatPay::new(Fold64::from(5).unwrap())))
        .unwrap();
    pay_tex.do_sign(&pay_acc).unwrap();

    let mut get_tex = TexCellAct::create_by(addr_of(&get_acc));
    get_tex
        .add_cell(Box::new(CellTrsSatGet::new(Fold64::from(2).unwrap())))
        .unwrap();
    get_tex.do_sign(&get_acc).unwrap();

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        0,
    );
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_sat(&mut ctx, &pay, 5);

    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "coin settlement check failed");
}

#[test]
fn hip23_p1_tex_with_hac_to_trs_prelude_succeeds() {
    init_setup();
    let main_acc = Account::create_by("hip23-p1h-main").unwrap();
    let pay_acc = Account::create_by("hip23-p1h-pay").unwrap();
    let get_acc = Account::create_by("hip23-p1h-get").unwrap();
    let pay = addr_of(&pay_acc);
    let get = addr_of(&get_acc);

    let fund = HacToTrs::create_by(pay, Amount::mei(5));
    let (pay_tex, get_tex) = build_balanced_tex_swap(&pay_acc, &get_acc, 100_000_000, 0, 0);
    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(fund), Box::new(pay_tex), Box::new(get_tex)],
        0,
    );

    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_hac(&mut ctx, &pay, 0);

    tx.execute(&mut ctx).unwrap();
    assert_eq!(hac_mei(&mut ctx, &get), 1);
    assert_eq!(hac_mei(&mut ctx, &pay), 4);
}

#[test]
fn hip23_p1_tex_diamond_count_swap_succeeds() {
    init_setup();
    let main_acc = Account::create_by("hip23-p1i-main").unwrap();
    let pay_acc = Account::create_by("hip23-p1i-pay").unwrap();
    let get_acc = Account::create_by("hip23-p1i-get").unwrap();
    let pay = addr_of(&pay_acc);
    let get = addr_of(&get_acc);

    let dia_a = DiamondName::from_readable(b"KKKKVA").unwrap();
    let dia_b = DiamondName::from_readable(b"HYXYHY").unwrap();
    let (pay_tex, get_tex) = {
        let mut pay_tex = TexCellAct::create_by(pay);
        pay_tex
            .add_cell(Box::new(CellTrsDiaPay::new(
                DiamondNameListMax200::from_list_checked(vec![dia_a.clone(), dia_b.clone()]).unwrap(),
            )))
            .unwrap();
        pay_tex.do_sign(&pay_acc).unwrap();

        let mut get_tex = TexCellAct::create_by(get);
        get_tex
            .add_cell(Box::new(CellTrsDiaGet::new(DiamondNumber::from(2))))
            .unwrap();
        get_tex.do_sign(&get_acc).unwrap();
        (pay_tex, get_tex)
    };

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        0,
    );
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_diamond_owned(&mut ctx, &dia_a, &pay);
    seed_diamond_owned(&mut ctx, &dia_b, &pay);

    tx.execute(&mut ctx).unwrap();
    assert_eq!(diamond_count(&mut ctx, &get), 2);
}

#[test]
fn hip23_p1_tex_height_condition_in_bundle() {
    init_setup();
    let main_acc = Account::create_by("hip23-p1f-main").unwrap();
    let pay_acc = Account::create_by("hip23-p1f-pay").unwrap();
    let get_acc = Account::create_by("hip23-p1f-get").unwrap();
    let pay = addr_of(&pay_acc);

    let mut pay_tex = TexCellAct::create_by(pay);
    pay_tex
        .add_cell(Box::new(CellCondHeightAtMost::new(TEST_HEIGHT + 100)))
        .unwrap();
    pay_tex
        .add_cell(Box::new(CellTrsZhuPay::new(Fold64::from(10_000_000).unwrap())))
        .unwrap();
    pay_tex.do_sign(&pay_acc).unwrap();

    let mut get_tex = TexCellAct::create_by(addr_of(&get_acc));
    get_tex
        .add_cell(Box::new(CellTrsZhuGet::new(Fold64::from(10_000_000).unwrap())))
        .unwrap();
    get_tex.do_sign(&get_acc).unwrap();

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        0,
    );

    let mut fail_ctx = make_ctx(TEST_HEIGHT + 200, tx.as_read());
    seed_hac(&mut fail_ctx, &addr_of(&main_acc), 1_000_000);
    seed_hac(&mut fail_ctx, &pay, 100);
    let err = tx.execute(&mut fail_ctx).unwrap_err();
    assert_err_contains(&err, "cell condition check failed");

    let mut ok_ctx = make_ctx(TEST_HEIGHT + 50, tx.as_read());
    seed_hac(&mut ok_ctx, &addr_of(&main_acc), 1_000_000);
    seed_hac(&mut ok_ctx, &pay, 100);
    tx.execute(&mut ok_ctx).unwrap();
}

// ---------------------------------------------------------------------------
// P2 — Guard adversarial
// ---------------------------------------------------------------------------

#[test]
fn hip23_p2_height_guard_boundary_inclusive() {
    init_setup();
    let main_acc = Account::create_by("hip23-p2a-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();
    let start = TEST_HEIGHT;
    let end = TEST_HEIGHT + 10;

    let mut guard = HeightScope::new();
    guard.start = BlockHeight::from(start);
    guard.end = BlockHeight::from(end);
    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient.clone());
    transfer.hacash = Amount::mei(1);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(guard), Box::new(transfer)],
        0,
    );

    let mut ctx_start = make_ctx(start, tx.as_read());
    seed_hac(&mut ctx_start, &main, 100);
    tx.execute(&mut ctx_start).unwrap();
    assert_eq!(hac_mei(&mut ctx_start, &recipient), 1);

    let tx_end = build_signed_type3(
        &main_acc,
        vec![
            Box::new({
                let mut g = HeightScope::new();
                g.start = BlockHeight::from(start);
                g.end = BlockHeight::from(end);
                g
            }),
            Box::new({
                let mut t = HacToTrs::new();
                t.to = AddrOrPtr::from_addr(recipient.clone());
                t.hacash = Amount::mei(1);
                t
            }),
        ],
        0,
    );
    let mut ctx_end = make_ctx(end, tx_end.as_read());
    seed_hac(&mut ctx_end, &main, 100);
    tx_end.execute(&mut ctx_end).unwrap();
    assert_eq!(hac_mei(&mut ctx_end, &recipient), 1);
}

#[test]
fn hip23_p2_height_guard_above_end_reverts() {
    init_setup();
    let main_acc = Account::create_by("hip23-p2e-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();
    let start = TEST_HEIGHT;
    let end = TEST_HEIGHT + 10;

    let mut guard = HeightScope::new();
    guard.start = BlockHeight::from(start);
    guard.end = BlockHeight::from(end);
    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient.clone());
    transfer.hacash = Amount::mei(4);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(guard), Box::new(transfer)],
        0,
    );

    let mut ctx = make_ctx(end + 1, tx.as_read());
    seed_hac(&mut ctx, &main, 100);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "submitted in height between");
    assert_eq!(hac_mei(&mut ctx, &recipient), 0);
    assert_eq!(hac_mei(&mut ctx, &main), 100);
}

#[test]
fn hip23_p2_height_guard_unlimited_end_zero() {
    init_setup();
    let main_acc = Account::create_by("hip23-p2b-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut guard = HeightScope::new();
    guard.start = BlockHeight::from(TEST_HEIGHT);
    guard.end = BlockHeight::from(0);
    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient.clone());
    transfer.hacash = Amount::mei(2);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(guard), Box::new(transfer)],
        0,
    );

    let mut ctx = make_ctx(TEST_HEIGHT + 1_000_000, tx.as_read());
    seed_hac(&mut ctx, &main, 100);
    tx.execute(&mut ctx).unwrap();
    assert_eq!(hac_mei(&mut ctx, &recipient), 2);
}

#[test]
fn hip23_p2_chain_allow_rejects_wrong_chain() {
    init_setup();
    let main_acc = Account::create_by("hip23-p2c-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut allow = ChainAllow::new();
    allow.chains = ChainIDList::from_list(vec![Uint4::from(1), Uint4::from(2)]).unwrap();
    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient);
    transfer.hacash = Amount::mei(3);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(allow), Box::new(transfer)],
        0,
    );

    let mut ctx = make_ctx_chain(TEST_HEIGHT, 9, tx.as_read());
    seed_hac(&mut ctx, &main, 100);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "must belong to chains");
}

#[test]
fn hip23_p2_transfer_before_guard_still_reverts_outside_window() {
    init_setup();
    let main_acc = Account::create_by("hip23-p2d-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient.clone());
    transfer.hacash = Amount::mei(7);

    let mut guard = HeightScope::new();
    guard.start = BlockHeight::from(TEST_HEIGHT + 100);
    guard.end = BlockHeight::from(TEST_HEIGHT + 200);

    // Anti-pattern: debit listed before guard — entire tx still reverts when guard fails.
    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(transfer), Box::new(guard)],
        0,
    );

    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 100);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "submitted in height between");
    // Tx is rejected, but earlier actions already mutated in-tx state in this harness.
    // Wallets MUST NOT treat step-by-step simulation as final until the whole tx succeeds.
    assert_eq!(hac_mei(&mut ctx, &recipient), 7);
    assert_eq!(hac_mei(&mut ctx, &main), 93);
}

// ---------------------------------------------------------------------------
// P3 — BalanceFloor adversarial
// ---------------------------------------------------------------------------

#[test]
fn hip23_p3_floor_asset_dimension_blocks_overspend() {
    init_setup();
    let main_acc = Account::create_by("hip23-p3a-main").unwrap();
    let cp_acc = Account::create_by("hip23-p3a-cp").unwrap();
    let main = addr_of(&main_acc);
    let counterparty = addr_of(&cp_acc);
    const SERIAL: u64 = 2330;

    let mut pay_tex = TexCellAct::create_by(main);
    pay_tex
        .add_cell(Box::new(CellTrsAssetPay::new(
            AssetAmt::from(SERIAL, 5).unwrap(),
        )))
        .unwrap();
    pay_tex.do_sign(&main_acc).unwrap();

    let mut get_tex = TexCellAct::create_by(counterparty);
    get_tex
        .add_cell(Box::new(CellTrsAssetGet::new(
            AssetAmt::from(SERIAL, 5).unwrap(),
        )))
        .unwrap();
    get_tex.do_sign(&cp_acc).unwrap();

    let mut floor = BalanceFloor::new();
    floor.addr = AddrOrPtr::from_addr(main);
    floor.assets
        .push(AssetAmt::from(SERIAL, 8).unwrap())
        .unwrap();

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex), Box::new(floor)],
        99,
    );

    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000_000);
    seed_asset(&mut ctx, &main, SERIAL, 10);

    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "lower than floor");
}

#[test]
fn hip23_p3_floor_before_transfer_checks_pre_debit_state() {
    init_setup();
    let main_acc = Account::create_by("hip23-p3b-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut floor = BalanceFloor::new();
    floor.addr = AddrOrPtr::from_addr(main);
    floor.hacash = Amount::mei(950);

    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient);
    transfer.hacash = Amount::mei(100);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(floor), Box::new(transfer)],
        0,
    );

    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000);
    tx.execute(&mut ctx).unwrap();
    assert_eq!(hac_mei(&mut ctx, &main), 900 - TX_FEE_MEI);
}

#[test]
fn hip23_p3_floor_satoshi_dimension_blocks_overspend() {
    init_setup();
    let main_acc = Account::create_by("hip23-p3c-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut pay_tex = TexCellAct::create_by(main);
    pay_tex
        .add_cell(Box::new(CellTrsSatPay::new(Fold64::from(4).unwrap())))
        .unwrap();
    pay_tex.do_sign(&main_acc).unwrap();

    let cp_acc = Account::create_by("hip23-p3c-cp").unwrap();
    let counterparty = addr_of(&cp_acc);
    let mut get_tex = TexCellAct::create_by(counterparty);
    get_tex
        .add_cell(Box::new(CellTrsSatGet::new(Fold64::from(4).unwrap())))
        .unwrap();
    get_tex.do_sign(&cp_acc);

    let mut floor = BalanceFloor::new();
    floor.addr = AddrOrPtr::from_addr(main);
    floor.satoshi = Satoshi::from(8);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex), Box::new(floor)],
        0,
    );

    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000_000);
    seed_sat(&mut ctx, &main, 10);

    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "lower than floor");
}

// ---------------------------------------------------------------------------
// P4 — HIP20 + TEX adversarial
// ---------------------------------------------------------------------------

#[test]
fn hip23_p4_asset_create_with_tex_same_tx_rejected() {
    init_setup();
    let main_acc = Account::create_by("hip23-p4-topo-main").unwrap();
    let issuer = addr_of(&Account::create_by("hip23-p4-topo-issuer").unwrap());
    const SERIAL: u64 = 2339;

    let mut create = AssetCreate::new();
    create.metadata = AssetSmelt {
        serial: Fold64::from(SERIAL).unwrap(),
        supply: Fold64::from(1000).unwrap(),
        decimal: Uint1::from(0),
        issuer,
        ticket: BytesW1::from_str("TOPO").unwrap(),
        name: BytesW1::from_str("Topo").unwrap(),
    };
    create.protocol_cost = genesis::block_reward(TEST_HEIGHT);

    let issuer_acc = Account::create_by("hip23-p4-topo-issuer").unwrap();
    let buyer_acc = Account::create_by("hip23-p4-topo-buyer").unwrap();
    let (pay_tex, get_tex) = build_balanced_tex_swap(&issuer_acc, &buyer_acc, 0, SERIAL, 1);

    let actions: Vec<Box<dyn Action>> = vec![
        Box::new(create),
        Box::new(pay_tex),
        Box::new(get_tex),
    ];
    let err =
        protocol::action::precheck_tx_actions(TransactionType3::TYPE, &actions).unwrap_err();
    assert_err_contains(&err, "TOP_ONLY");

    let tx = build_signed_type3(&main_acc, actions, 99);
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    let exec_err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&exec_err, "TOP_ONLY");
}

#[test]
fn hip23_p4_duplicate_serial_rejected() {
    init_setup();
    let main_acc = Account::create_by("hip23-p4a-main").unwrap();
    let issuer = addr_of(&Account::create_by("hip23-p4a-issuer").unwrap());
    const SERIAL: u64 = 2340;

    let mut create = AssetCreate::new();
    create.metadata = AssetSmelt {
        serial: Fold64::from(SERIAL).unwrap(),
        supply: Fold64::from(1000).unwrap(),
        decimal: Uint1::from(0),
        issuer,
        ticket: BytesW1::from_str("DUP").unwrap(),
        name: BytesW1::from_str("Dup").unwrap(),
    };
    create.protocol_cost = genesis::block_reward(TEST_HEIGHT);

    let tx = build_signed_type3(&main_acc, vec![Box::new(create)], 0);
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    tx.execute(&mut ctx).unwrap();

    let mut create_dup = AssetCreate::new();
    create_dup.metadata = AssetSmelt {
        serial: Fold64::from(SERIAL).unwrap(),
        supply: Fold64::from(1000).unwrap(),
        decimal: Uint1::from(0),
        issuer,
        ticket: BytesW1::from_str("DUP").unwrap(),
        name: BytesW1::from_str("Dup").unwrap(),
    };
    create_dup.protocol_cost = genesis::block_reward(TEST_HEIGHT + 1);

    let persisted = ctx.state().clone_state();
    let tx_dup = build_signed_type3(&main_acc, vec![Box::new(create_dup)], 0);
    let mut ctx2 = make_ctx_persisted(TEST_HEIGHT + 1, persisted, tx_dup.as_read());
    seed_hac(&mut ctx2, &addr_of(&main_acc), 1_000_000);
    let err = tx_dup.execute(&mut ctx2).unwrap_err();
    assert_err_contains(&err, "already exists");
}

#[test]
fn hip23_p4_tex_on_missing_asset_fails() {
    init_setup();
    let main_acc = Account::create_by("hip23-p4b-main").unwrap();
    let pay_acc = Account::create_by("hip23-p4b-pay").unwrap();
    let get_acc = Account::create_by("hip23-p4b-get").unwrap();
    const SERIAL: u64 = 2341;

    let (pay_tex, get_tex) = build_balanced_tex_swap(&pay_acc, &get_acc, 0, SERIAL, 1);
    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        99,
    );

    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "does not exist");
}

#[test]
fn hip23_p4_issuer_insufficient_asset_for_tex_pay() {
    init_setup();
    let main_acc = Account::create_by("hip23-p4c-main").unwrap();
    let pay_acc = Account::create_by("hip23-p4c-pay").unwrap();
    let get_acc = Account::create_by("hip23-p4c-get").unwrap();
    let pay = addr_of(&pay_acc);
    const SERIAL: u64 = 2342;

    let (pay_tex, get_tex) = build_balanced_tex_swap(&pay_acc, &get_acc, 0, SERIAL, 100);
    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        99,
    );

    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_asset(&mut ctx, &pay, SERIAL, 50);

    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(err.contains("insufficient") || err.contains("overflow"), "{err}");
}

#[test]
fn hip23_p4_wrong_protocol_cost_rejected() {
    init_setup();
    let main_acc = Account::create_by("hip23-p4d-main").unwrap();
    let issuer = addr_of(&Account::create_by("hip23-p4d-issuer").unwrap());
    const SERIAL: u64 = 2343;

    let mut create = AssetCreate::new();
    create.metadata = AssetSmelt {
        serial: Fold64::from(SERIAL).unwrap(),
        supply: Fold64::from(1000).unwrap(),
        decimal: Uint1::from(0),
        issuer,
        ticket: BytesW1::from_str("FEE").unwrap(),
        name: BytesW1::from_str("Fee").unwrap(),
    };
    create.protocol_cost = Amount::mei(1);

    let tx = build_signed_type3(&main_acc, vec![Box::new(create)], 0);
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "Protocol fee must be");
}

// ---------------------------------------------------------------------------
// P5 — AST adversarial
// ---------------------------------------------------------------------------

#[test]
fn hip23_p5_ast_if_condition_fault_aborts_whole_node() {
    init_setup();
    let main_acc = Account::create_by("hip23-p5a-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut bad_guard = HeightScope::new();
    bad_guard.start = BlockHeight::from(20);
    bad_guard.end = BlockHeight::from(10);
    let cond = AstSelect::create_by(1, 1, vec![Box::new(bad_guard)]);
    let br_if = AstSelect::create_list(vec![Box::new(HacToTrs::create_by(
        recipient.clone(),
        Amount::mei(5),
    ))]);
    let br_else = AstSelect::create_list(vec![Box::new(HacToTrs::create_by(
        recipient.clone(),
        Amount::mei(1),
    ))]);
    let act = AstIf::create_by(cond, br_if, br_else);

    let tx = build_signed_type3(&main_acc, vec![Box::new(act)], 17);
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000);

    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "cannot exceed");
    assert_eq!(hac_mei(&mut ctx, &recipient), 0);
}

#[test]
fn hip23_p5_ast_else_branch_executes_transfer() {
    init_setup();
    let main_acc = Account::create_by("hip23-p5b-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut cond_guard = HeightScope::new();
    cond_guard.start = BlockHeight::from(TEST_HEIGHT + 500);
    cond_guard.end = BlockHeight::from(TEST_HEIGHT + 600);
    let cond = AstSelect::create_by(1, 1, vec![Box::new(cond_guard)]);

    let br_if = AstSelect::create_list(vec![Box::new(HacToTrs::create_by(
        recipient.clone(),
        Amount::mei(50),
    ))]);
    let br_else = AstSelect::create_list(vec![Box::new(HacToTrs::create_by(
        recipient.clone(),
        Amount::mei(3),
    ))]);
    let act = AstIf::create_by(cond, br_if, br_else);

    let tx = build_signed_type3(&main_acc, vec![Box::new(act)], 17);
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000);
    tx.execute(&mut ctx).unwrap();
    assert_eq!(hac_mei(&mut ctx, &recipient), 3);
}

#[test]
fn hip23_p5_ast_requires_nonzero_gas() {
    init_setup();
    let main_acc = Account::create_by("hip23-p5c-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut cond_guard = HeightScope::new();
    cond_guard.start = BlockHeight::from(TEST_HEIGHT);
    cond_guard.end = BlockHeight::from(TEST_HEIGHT + 100);
    let cond = AstSelect::create_by(1, 1, vec![Box::new(cond_guard)]);
    let br_if = AstSelect::create_list(vec![Box::new(HacToTrs::create_by(
        recipient,
        Amount::mei(1),
    ))]);
    let act = AstIf::create_by(cond, br_if, AstSelect::nop());

    let tx = build_signed_type3(&main_acc, vec![Box::new(act)], 0);
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "gas not initialized");
}

// ---------------------------------------------------------------------------
// Topology / composition
// ---------------------------------------------------------------------------

#[test]
fn hip23_topology_guard_only_tx_rejected() {
    init_setup();
    let actions: Vec<Box<dyn Action>> = vec![Box::new(HeightScope::new())];
    let err = protocol::action::precheck_tx_actions(TransactionType3::TYPE, &actions).unwrap_err();
    assert_err_contains(&err, "all GUARD");
}

#[test]
fn hip23_topology_guard_only_execute_rejected() {
    init_setup();
    let main_acc = Account::create_by("hip23-topo-exec-main").unwrap();
    let tx = build_signed_type3(&main_acc, vec![Box::new(HeightScope::new())], 0);
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 100);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "all GUARD");
}

#[test]
fn hip23_combined_height_scope_balance_floor_and_transfer() {
    init_setup();
    let main_acc = Account::create_by("hip23-combo-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut height = HeightScope::new();
    height.start = BlockHeight::from(TEST_HEIGHT);
    height.end = BlockHeight::from(TEST_HEIGHT + 100);

    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient.clone());
    transfer.hacash = Amount::mei(40);

    let mut floor = BalanceFloor::new();
    floor.addr = AddrOrPtr::from_addr(main);
    floor.hacash = Amount::mei(900);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(height), Box::new(transfer), Box::new(floor)],
        0,
    );

    let mut ctx = make_ctx(TEST_HEIGHT + 50, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000);
    tx.execute(&mut ctx).unwrap();
    assert_eq!(hac_mei(&mut ctx, &main), 960 - TX_FEE_MEI);
    assert_eq!(hac_mei(&mut ctx, &recipient), 40);
}

#[test]
fn hip23_combined_height_floor_transfer_outside_height_fails() {
    init_setup();
    let main_acc = Account::create_by("hip23-combo-fail-h-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut height = HeightScope::new();
    height.start = BlockHeight::from(TEST_HEIGHT);
    height.end = BlockHeight::from(TEST_HEIGHT + 100);

    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient.clone());
    transfer.hacash = Amount::mei(40);

    let mut floor = BalanceFloor::new();
    floor.addr = AddrOrPtr::from_addr(main);
    floor.hacash = Amount::mei(900);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(height), Box::new(transfer), Box::new(floor)],
        0,
    );

    let mut ctx = make_ctx(TEST_HEIGHT - 1, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "submitted in height between");
    assert_eq!(hac_mei(&mut ctx, &recipient), 0);
}

#[test]
fn hip23_combined_height_floor_transfer_below_floor_fails() {
    init_setup();
    let main_acc = Account::create_by("hip23-combo-fail-f-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut height = HeightScope::new();
    height.start = BlockHeight::from(TEST_HEIGHT);
    height.end = BlockHeight::from(TEST_HEIGHT + 100);

    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient.clone());
    transfer.hacash = Amount::mei(150);

    let mut floor = BalanceFloor::new();
    floor.addr = AddrOrPtr::from_addr(main);
    floor.hacash = Amount::mei(900);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(height), Box::new(transfer), Box::new(floor)],
        0,
    );

    let mut ctx = make_ctx(TEST_HEIGHT + 50, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "lower than floor");
    // Debit ran before failing floor; in-tx simulator shows partial progress (see HIP23.md §6.4).
    assert_eq!(hac_mei(&mut ctx, &recipient), 150);
    assert_eq!(hac_mei(&mut ctx, &main), 850);
}

#[test]
fn hip23_height_guard_plus_tex_swap_in_one_tx() {
    init_setup();
    let main_acc = Account::create_by("hip23-combo-tex-main").unwrap();
    let pay_acc = Account::create_by("hip23-combo-tex-pay").unwrap();
    let get_acc = Account::create_by("hip23-combo-tex-get").unwrap();
    let pay = addr_of(&pay_acc);

    let mut guard = HeightScope::new();
    guard.start = BlockHeight::from(TEST_HEIGHT);
    guard.end = BlockHeight::from(TEST_HEIGHT + 500);

    let (pay_tex, get_tex) = build_balanced_tex_swap(&pay_acc, &get_acc, 100_000_000, 0, 0);
    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(guard), Box::new(pay_tex), Box::new(get_tex)],
        0,
    );

    let mut ctx = make_ctx(TEST_HEIGHT + 10, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_hac(&mut ctx, &pay, 100);
    tx.execute(&mut ctx).unwrap();
    assert_eq!(hac_mei(&mut ctx, &addr_of(&get_acc)), 1);
}

#[test]
fn hip23_height_guard_plus_tex_outside_window_fails() {
    init_setup();
    let main_acc = Account::create_by("hip23-combo-tex-fail-main").unwrap();
    let pay_acc = Account::create_by("hip23-combo-tex-fail-pay").unwrap();
    let get_acc = Account::create_by("hip23-combo-tex-fail-get").unwrap();
    let pay = addr_of(&pay_acc);
    let get = addr_of(&get_acc);

    let mut guard = HeightScope::new();
    guard.start = BlockHeight::from(TEST_HEIGHT);
    guard.end = BlockHeight::from(TEST_HEIGHT + 500);

    let (pay_tex, get_tex) = build_balanced_tex_swap(&pay_acc, &get_acc, 100_000_000, 0, 0);
    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(guard), Box::new(pay_tex), Box::new(get_tex)],
        0,
    );

    let mut ctx = make_ctx(TEST_HEIGHT + 501, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_hac(&mut ctx, &pay, 100);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "submitted in height between");
    assert_eq!(hac_mei(&mut ctx, &get), 0);
}