//! HIP-23 stress / fuzz-adjacent tests — aggressive edge cases and multi-pattern combos.
//! Run: cargo test hip23_stress_ -- --nocapture

mod common;

use basis::interface::{StateOperat, Transaction, TxExec};
use common::hip23::*;
use field::*;
use mint::action::AssetCreate;
use mint::action::ASSET_ALIVE_HEIGHT;
use mint::genesis;
use protocol::action::*;
use protocol::tex::*;
use sys::Account;

// ---------------------------------------------------------------------------
// Height / guard stress
// ---------------------------------------------------------------------------

#[test]
fn hip23_stress_height_start_zero_end_zero_always_passes() {
    init_setup();
    let main_acc = Account::create_by("hip23-stress-h00-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut guard = HeightScope::new();
    guard.start = BlockHeight::from(0);
    guard.end = BlockHeight::from(0);
    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient.clone());
    transfer.hacash = Amount::mei(1);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(guard), Box::new(transfer)],
        0,
    );
    let mut ctx = make_ctx(1, tx.as_read());
    seed_hac(&mut ctx, &main, 50);
    tx.execute(&mut ctx).unwrap();
    assert_eq!(hac_mei(&mut ctx, &recipient), 1);
}

#[test]
fn hip23_stress_triple_guard_chain_allow_height_floor_transfer() {
    init_setup();
    let main_acc = Account::create_by("hip23-stress-triple-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut allow = ChainAllow::new();
    allow.chains = ChainIDList::from_list(vec![Uint4::from(0)]).unwrap();

    let mut height = HeightScope::new();
    height.start = BlockHeight::from(TEST_HEIGHT);
    height.end = BlockHeight::from(TEST_HEIGHT + 50);

    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(recipient.clone());
    transfer.hacash = Amount::mei(25);

    let mut floor = BalanceFloor::new();
    floor.addr = AddrOrPtr::from_addr(main);
    floor.hacash = Amount::mei(900);

    let tx = build_signed_type3(
        &main_acc,
        vec![
            Box::new(allow),
            Box::new(height),
            Box::new(transfer),
            Box::new(floor),
        ],
        0,
    );

    let mut ctx = make_ctx(TEST_HEIGHT + 10, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000);
    tx.execute(&mut ctx).unwrap();
    assert_eq!(hac_mei(&mut ctx, &recipient), 25);
    assert_eq!(hac_mei(&mut ctx, &main), 975 - TX_FEE_MEI);
}

#[test]
fn hip23_stress_height_far_future_start_reverts_at_present() {
    init_setup();
    let main_acc = Account::create_by("hip23-stress-hfar-main").unwrap();
    let main = addr_of(&main_acc);
    let far = TEST_HEIGHT + 10_000_000;

    let mut guard = HeightScope::new();
    guard.start = BlockHeight::from(far);
    guard.end = BlockHeight::from(0);
    let mut transfer = HacToTrs::new();
    transfer.to = AddrOrPtr::from_addr(field::ADDRESS_TWOX.clone());
    transfer.hacash = Amount::mei(1);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(guard), Box::new(transfer)],
        0,
    );
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 10);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "submitted in height between");
}

// ---------------------------------------------------------------------------
// TEX stress
// ---------------------------------------------------------------------------

#[test]
fn hip23_stress_three_party_hac_tex_zero_sum() {
    init_setup();
    let main_acc = Account::create_by("hip23-stress-3p-main").unwrap();
    let a_acc = Account::create_by("hip23-stress-3p-a").unwrap();
    let b_acc = Account::create_by("hip23-stress-3p-b").unwrap();
    let c_acc = Account::create_by("hip23-stress-3p-c").unwrap();
    let a = addr_of(&a_acc);
    let b = addr_of(&b_acc);
    let c = addr_of(&c_acc);

    // A pays 30M, B pays 70M, C gets 100M — zero-sum across three bundles.
    let mut tex_a = TexCellAct::create_by(a);
    tex_a
        .add_cell(Box::new(CellTrsZhuPay::new(Fold64::from(30_000_000).unwrap())))
        .unwrap();
    tex_a.do_sign(&a_acc).unwrap();

    let mut tex_b = TexCellAct::create_by(b);
    tex_b
        .add_cell(Box::new(CellTrsZhuPay::new(Fold64::from(70_000_000).unwrap())))
        .unwrap();
    tex_b.do_sign(&b_acc).unwrap();

    let mut tex_c = TexCellAct::create_by(c);
    tex_c
        .add_cell(Box::new(CellTrsZhuGet::new(Fold64::from(100_000_000).unwrap())))
        .unwrap();
    tex_c.do_sign(&c_acc).unwrap();

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(tex_a), Box::new(tex_b), Box::new(tex_c)],
        0,
    );
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_hac(&mut ctx, &a, 1);
    seed_hac(&mut ctx, &b, 1);

    tx.execute(&mut ctx).unwrap();
    assert_eq!(hac_mei(&mut ctx, &c), 1);
}

#[test]
fn hip23_stress_tex_asset_serial_mismatch_fails_settlement() {
    init_setup();
    let main_acc = Account::create_by("hip23-stress-serial-main").unwrap();
    let pay_acc = Account::create_by("hip23-stress-serial-pay").unwrap();
    let get_acc = Account::create_by("hip23-stress-serial-get").unwrap();
    let pay = addr_of(&pay_acc);
    const SERIAL_PAY: u64 = 2401;
    const SERIAL_GET: u64 = 2402;

    let mut pay_tex = TexCellAct::create_by(pay);
    pay_tex
        .add_cell(Box::new(CellTrsAssetPay::new(
            AssetAmt::from(SERIAL_PAY, 1).unwrap(),
        )))
        .unwrap();
    pay_tex.do_sign(&pay_acc).unwrap();

    let mut get_tex = TexCellAct::create_by(addr_of(&get_acc));
    get_tex
        .add_cell(Box::new(CellTrsAssetGet::new(
            AssetAmt::from(SERIAL_GET, 1).unwrap(),
        )))
        .unwrap();
    get_tex.do_sign(&get_acc).unwrap();

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        99,
    );
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_asset(&mut ctx, &pay, SERIAL_PAY, 1);
    seed_asset(&mut ctx, &pay, SERIAL_GET, 0);

    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(
        err.contains("settlement check failed") || err.contains("asset"),
        "{err}"
    );
}

#[test]
fn hip23_stress_tex_empty_bundles_rejected_or_noop() {
    init_setup();
    let main_acc = Account::create_by("hip23-stress-empty-main").unwrap();
    let a_acc = Account::create_by("hip23-stress-empty-a").unwrap();
    let b_acc = Account::create_by("hip23-stress-empty-b").unwrap();

    let tex_a = TexCellAct::create_by(addr_of(&a_acc));
    let tex_b = TexCellAct::create_by(addr_of(&b_acc));
    // Unsigned empty bundles — should fail at sign verification or settlement.
    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(tex_a), Box::new(tex_b)],
        0,
    );
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(
        err.contains("signature") || err.contains("settlement") || err.contains("positive"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// P4 stress — issuance at serial floor + chained distributions
// ---------------------------------------------------------------------------

#[test]
fn hip23_stress_p4_minsri_serial_and_double_tex_distribution() {
    init_setup();
    let main_acc = Account::create_by("hip23-stress-p4-main").unwrap();
    let issuer_acc = Account::create_by("hip23-stress-p4-issuer").unwrap();
    let buyer_a = Account::create_by("hip23-stress-p4-buy-a").unwrap();
    let buyer_b = Account::create_by("hip23-stress-p4-buy-b").unwrap();
    let main = addr_of(&main_acc);
    let issuer = addr_of(&issuer_acc);
    let buyer_a_addr = addr_of(&buyer_a);
    let buyer_b_addr = addr_of(&buyer_b);
    const SERIAL: u64 = 1025;
    let height = ASSET_ALIVE_HEIGHT + 100;

    let mut create = AssetCreate::new();
    create.metadata = AssetSmelt {
        serial: Fold64::from(SERIAL).unwrap(),
        supply: Fold64::from(1_000).unwrap(),
        decimal: Uint1::from(0),
        issuer,
        ticket: BytesW1::from_str("STR").unwrap(),
        name: BytesW1::from_str("Stress").unwrap(),
    };
    create.protocol_cost = genesis::block_reward(height);

    let tx_create = build_signed_type3(&main_acc, vec![Box::new(create)], 0);
    let mut ctx = make_ctx(height, tx_create.as_read());
    seed_hac(&mut ctx, &main, 1_000_000);
    tx_create.execute(&mut ctx).unwrap();
    assert_eq!(asset_amt(&mut ctx, &issuer, SERIAL), 1_000);

    let mut persisted = ctx.state().clone_state();

    for (buyer_acc, buyer_addr, amt) in [
        (&buyer_a, buyer_a_addr, 100u64),
        (&buyer_b, buyer_b_addr, 200u64),
    ] {
        let mut issuer_tex = TexCellAct::create_by(issuer);
        issuer_tex
            .add_cell(Box::new(CellTrsAssetPay::new(
                AssetAmt::from(SERIAL, amt).unwrap(),
            )))
            .unwrap();
        issuer_tex.do_sign(&issuer_acc).unwrap();

        let mut buyer_tex = TexCellAct::create_by(buyer_addr);
        buyer_tex
            .add_cell(Box::new(CellTrsAssetGet::new(
                AssetAmt::from(SERIAL, amt).unwrap(),
            )))
            .unwrap();
        buyer_tex.do_sign(buyer_acc).unwrap();

        let tx_tex = build_signed_type3(
            &main_acc,
            vec![Box::new(issuer_tex), Box::new(buyer_tex)],
            99,
        );
        let mut tex_ctx = make_ctx_persisted(height, persisted.clone_state(), tx_tex.as_read());
        tx_tex.execute(&mut tex_ctx).unwrap();
        persisted = tex_ctx.state().clone_state();
    }

    let final_ctx = make_ctx_persisted(height, persisted, tx_create.as_read());
    let mut check = final_ctx;
    assert_eq!(asset_amt(&mut check, &buyer_a_addr, SERIAL), 100);
    assert_eq!(asset_amt(&mut check, &buyer_b_addr, SERIAL), 200);
    assert_eq!(asset_amt(&mut check, &issuer, SERIAL), 700);
}

// ---------------------------------------------------------------------------
// P5 / AST stress
// ---------------------------------------------------------------------------

#[test]
fn hip23_stress_p5_balance_floor_condition_else_branch() {
    init_setup();
    let main_acc = Account::create_by("hip23-stress-p5bf-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut floor_guard = BalanceFloor::new();
    floor_guard.addr = AddrOrPtr::from_addr(main);
    // Balance 1000 < floor 1001 → condition reverts → else branch.
    floor_guard.hacash = Amount::mei(1001);
    let cond = AstSelect::create_by(1, 1, vec![Box::new(floor_guard)]);

    let br_if = AstSelect::create_list(vec![Box::new(HacToTrs::create_by(
        recipient.clone(),
        Amount::mei(100),
    ))]);
    let br_else = AstSelect::create_list(vec![Box::new(HacToTrs::create_by(
        recipient.clone(),
        Amount::mei(2),
    ))]);
    let act = AstIf::create_by(cond, br_if, br_else);

    let tx = build_signed_type3(&main_acc, vec![Box::new(act)], 17);
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000);
    tx.execute(&mut ctx).unwrap();
    assert_eq!(hac_mei(&mut ctx, &recipient), 2);
}

#[test]
fn hip23_stress_p5_ast_low_gas_fails_after_partial_burn() {
    init_setup();
    let main_acc = Account::create_by("hip23-stress-gas-main").unwrap();
    let main = addr_of(&main_acc);
    let recipient = field::ADDRESS_TWOX.clone();

    let mut cond_guard = HeightScope::new();
    cond_guard.start = BlockHeight::from(TEST_HEIGHT);
    cond_guard.end = BlockHeight::from(TEST_HEIGHT + 10);
    let cond = AstSelect::create_by(1, 1, vec![Box::new(cond_guard)]);
    let br_if = AstSelect::create_list(vec![Box::new(HacToTrs::create_by(
        recipient,
        Amount::mei(500),
    ))]);
    let act = AstIf::create_by(cond, br_if, AstSelect::nop());

    // gas_max=1 → tiny budget, should fail during AST execution.
    let tx = build_signed_type3(&main_acc, vec![Box::new(act)], 1);
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(
        err.contains("gas") || err.contains("insufficient") || err.contains("budget"),
        "{err}"
    );
}

// ---------------------------------------------------------------------------
// P3 multi-debit stress
// ---------------------------------------------------------------------------

#[test]
fn hip23_stress_p3_multi_debit_single_floor() {
    init_setup();
    let main_acc = Account::create_by("hip23-stress-mdeb-main").unwrap();
    let main = addr_of(&main_acc);
    let r1 = field::ADDRESS_TWOX.clone();
    let r2_acc = Account::create_by("hip23-stress-mdeb-r2").unwrap();
    let r2 = addr_of(&r2_acc);

    let t1 = HacToTrs::create_by(r1.clone(), Amount::mei(30));
    let t2 = HacToTrs::create_by(r2, Amount::mei(20));
    let mut floor = BalanceFloor::new();
    floor.addr = AddrOrPtr::from_addr(main);
    floor.hacash = Amount::mei(940);

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(t1), Box::new(t2), Box::new(floor)],
        0,
    );
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &main, 1_000);
    tx.execute(&mut ctx).unwrap();
    assert_eq!(hac_mei(&mut ctx, &main), 950 - TX_FEE_MEI);
    assert_eq!(hac_mei(&mut ctx, &r1), 30);
    assert_eq!(hac_mei(&mut ctx, &r2), 20);
}

// ---------------------------------------------------------------------------
// Topology / precheck stress
// ---------------------------------------------------------------------------

#[test]
fn hip23_stress_duplicate_tex_same_addr_two_bundles_unsigned_second() {
    init_setup();
    let main_acc = Account::create_by("hip23-stress-dupaddr-main").unwrap();
    let pay_acc = Account::create_by("hip23-stress-dupaddr-pay").unwrap();
    let get_acc = Account::create_by("hip23-stress-dupaddr-get").unwrap();

    let (pay_tex, get_tex) = build_balanced_tex_swap(&pay_acc, &get_acc, 10_000_000, 0, 0);

    let mut pay_tex2 = TexCellAct::create_by(addr_of(&pay_acc));
    pay_tex2
        .add_cell(Box::new(CellTrsZhuPay::new(Fold64::from(10_000_000).unwrap())))
        .unwrap();
    // Deliberately not signed — stress malformed multi-bundle tx.
    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex), Box::new(pay_tex2)],
        0,
    );
    let mut ctx = make_ctx(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_hac(&mut ctx, &addr_of(&pay_acc), 10);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert!(
        err.contains("signature") || err.contains("settlement"),
        "{err}"
    );
}

#[test]
fn hip23_stress_asset_create_minsri_serial_1025_at_alive_height() {
    init_setup();
    let main_acc = Account::create_by("hip23-stress-devserial-main").unwrap();
    let issuer = addr_of(&Account::create_by("hip23-stress-devserial-iss").unwrap());
    // Minsri floor serial at ASSET_ALIVE_HEIGHT on mainnet chain 0.
    const SERIAL: u64 = 1025;

    let mut create = AssetCreate::new();
    create.metadata = AssetSmelt {
        serial: Fold64::from(SERIAL).unwrap(),
        supply: Fold64::from(100).unwrap(),
        decimal: Uint1::from(0),
        issuer,
        ticket: BytesW1::from_str("MN").unwrap(),
        name: BytesW1::from_str("Min").unwrap(),
    };
    create.protocol_cost = genesis::block_reward(ASSET_ALIVE_HEIGHT);

    let tx = build_signed_type3(&main_acc, vec![Box::new(create)], 0);
    let mut ctx = make_ctx(ASSET_ALIVE_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    tx.execute(&mut ctx).unwrap();
    assert_eq!(asset_amt(&mut ctx, &issuer, SERIAL), 100);
}

#[test]
fn hip23_stress_asset_serial_below_minsri_faults_on_mainnet() {
    init_setup();
    let main_acc = Account::create_by("hip23-stress-badserial-main").unwrap();
    let issuer = addr_of(&Account::create_by("hip23-stress-badserial-iss").unwrap());
    const SERIAL: u64 = 1024;

    let mut create = AssetCreate::new();
    create.metadata = AssetSmelt {
        serial: Fold64::from(SERIAL).unwrap(),
        supply: Fold64::from(100).unwrap(),
        decimal: Uint1::from(0),
        issuer,
        ticket: BytesW1::from_str("BAD").unwrap(),
        name: BytesW1::from_str("Bad").unwrap(),
    };
    create.protocol_cost = genesis::block_reward(ASSET_ALIVE_HEIGHT);

    let tx = build_signed_type3(&main_acc, vec![Box::new(create)], 0);
    let mut ctx = make_ctx(ASSET_ALIVE_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "serial cannot be less than");
}