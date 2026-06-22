//! HIP-23 TEX replay and composition attacks.
//! TEX signatures bind addr+cells only — not the enclosing Type3 tx.
//! Run: cargo test hip23_tex_replay_ -- --nocapture

mod common;

use basis::interface::{Transaction, TxExec};
use common::hip23::*;
use field::*;
use protocol::action::*;
use protocol::tex::*;
use sys::Account;
use basis::interface::StateOperat;
use testkit::sim::state::ForkableMemState;

#[test]
fn hip23_tex_replay_same_bundle_different_main_succeeds() {
    init_setup();
    let main_a = Account::create_by("hip23-replay-main-a").unwrap();
    let main_b = Account::create_by("hip23-replay-main-b").unwrap();
    let pay_acc = Account::create_by("hip23-replay-pay").unwrap();
    let get_acc = Account::create_by("hip23-replay-get").unwrap();
    let pay = addr_of(&pay_acc);
    let get = addr_of(&get_acc);

    let (pay_tex, get_tex) = build_balanced_tex_swap(&pay_acc, &get_acc, 100_000_000, 0, 0);
    // Reuse wire-identical signed bundles (not re-signed) in a second composed tx.
    let pay_replay = clone_tex_wire(&pay_tex);
    let get_replay = clone_tex_wire(&get_tex);

    let tx_a = build_signed_type3(
        &main_a,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        0,
    );
    let tx_b = build_signed_type3(
        &main_b,
        vec![Box::new(pay_replay), Box::new(get_replay)],
        0,
    );

    let mut ctx_a = make_ctx_strict(TEST_HEIGHT, tx_a.as_read());
    seed_hac(&mut ctx_a, &addr_of(&main_a), 1_000_000);
    seed_hac(&mut ctx_a, &pay, 10);
    tx_a.execute(&mut ctx_a).unwrap();
    assert_eq!(hac_mei(&mut ctx_a, &get), 1);

    let mut ctx_b = make_ctx_strict(TEST_HEIGHT, tx_b.as_read());
    seed_hac(&mut ctx_b, &addr_of(&main_b), 1_000_000);
    seed_hac(&mut ctx_b, &pay, 10);
    tx_b.execute(&mut ctx_b).unwrap();
    assert_eq!(hac_mei(&mut ctx_b, &get), 1);
}

#[test]
fn hip23_tex_replay_tampered_cell_after_sign_fails() {
    init_setup();
    let main_acc = Account::create_by("hip23-replay-tamper-main").unwrap();
    let pay_acc = Account::create_by("hip23-replay-tamper-pay").unwrap();
    let get_acc = Account::create_by("hip23-replay-tamper-get").unwrap();
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
fn hip23_tex_replay_extra_unbalanced_party_fails_settlement() {
    init_setup();
    let main_acc = Account::create_by("hip23-replay-3p-main").unwrap();
    let pay_acc = Account::create_by("hip23-replay-3p-pay").unwrap();
    let get_acc = Account::create_by("hip23-replay-3p-get").unwrap();
    let rogue_acc = Account::create_by("hip23-replay-3p-rogue").unwrap();
    let pay = addr_of(&pay_acc);
    let rogue = addr_of(&rogue_acc);

    let (pay_tex, get_tex) = build_balanced_tex_swap(&pay_acc, &get_acc, 100_000_000, 0, 0);

    let mut rogue_tex = TexCellAct::create_by(rogue);
    rogue_tex
        .add_cell(Box::new(CellTrsZhuPay::new(Fold64::from(50_000_000).unwrap())))
        .unwrap();
    rogue_tex.do_sign(&rogue_acc).unwrap();

    let tx = build_signed_type3(
        &main_acc,
        vec![Box::new(pay_tex), Box::new(get_tex), Box::new(rogue_tex)],
        0,
    );
    let mut ctx = make_ctx_strict(TEST_HEIGHT, tx.as_read());
    seed_hac(&mut ctx, &addr_of(&main_acc), 1_000_000);
    seed_hac(&mut ctx, &pay, 10);
    seed_hac(&mut ctx, &rogue, 10);

    let err = tx.execute(&mut ctx).unwrap_err();
    assert_err_contains(&err, "settlement check failed");
}

#[test]
fn hip23_tex_replay_same_wire_twice_on_persisted_chain() {
    init_setup();
    let main_a = Account::create_by("hip23-replay-ch-main-a").unwrap();
    let main_b = Account::create_by("hip23-replay-ch-main-b").unwrap();
    let pay_acc = Account::create_by("hip23-replay-ch-pay").unwrap();
    let get_acc = Account::create_by("hip23-replay-ch-get").unwrap();
    let pay = addr_of(&pay_acc);
    let get = addr_of(&get_acc);

    let (pay_tex, get_tex) = build_balanced_tex_swap(&pay_acc, &get_acc, 100_000_000, 0, 0);
    let pay_replay = clone_tex_wire(&pay_tex);
    let get_replay = clone_tex_wire(&get_tex);

    let tx_a = build_signed_type3(
        &main_a,
        vec![Box::new(pay_tex), Box::new(get_tex)],
        0,
    );
    let tx_b = build_signed_type3(
        &main_b,
        vec![Box::new(pay_replay), Box::new(get_replay)],
        0,
    );

    let mut state: Box<dyn basis::interface::State> =
        Box::new(ForkableMemState::default());
    seed_hac_chain(&mut state, &addr_of(&main_a), 1_000_000);
    seed_hac_chain(&mut state, &addr_of(&main_b), 1_000_000);
    seed_hac_chain(&mut state, &pay, 5);

    try_execute_tx_fork(TEST_HEIGHT, false, tx_a.as_read(), &mut state).unwrap();
    try_execute_tx_fork(TEST_HEIGHT, false, tx_b.as_read(), &mut state).unwrap();
    assert_eq!(hac_mei_chain(&state, &get), 2);
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