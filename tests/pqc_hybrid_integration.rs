use basis::interface::{StateOperat, Transaction, TransactionRead, TxExec};
use field::*;
use protocol::action::HacToTrs;
use protocol::transaction::TransactionType4;
use protocol::upgrade::{
    DEV_OPEN_MAX_HEIGHT, MAINNET_CHAIN_ID, PQC_TYPE4_OPEN_HEIGHT, check_gated_tx,
};
use sys::*;

use testkit::sim::integration::ensure_standard_protocol_setup_for_tests;

fn init_setup() {
    ensure_standard_protocol_setup_for_tests(|_, stuff| sys::calculate_hash(stuff), false);
}

fn random_fill(buf: &mut [u8]) -> Rerr {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i as u8).wrapping_add(11);
    }
    Ok(())
}

fn fund_main(ctx: &mut protocol::context::ContextInst<'_>, main: &Address) {
    let mut st = protocol::state::CoreState::wrap(ctx.state());
    let mut bls = st.balance(main).unwrap_or_default();
    bls.hacash = Amount::unit238(10_000_000_000_000);
    st.balance_set(main, &bls);
}

#[test]
fn pqc_type4_wire_roundtrip_and_verify() {
    init_setup();
    let acc = HybridAccount::create_pqc_randomly(&random_fill).unwrap();
    let main = Address::from(*acc.address());
    let to = Address::from_readable(Account::create_by("recipient-1").unwrap().readable()).unwrap();
    let mut tx = TransactionType4::new_by(main, Amount::unit238(1000), 1_730_000_000);
    tx.push_action(Box::new(HacToTrs::create_by(to, Amount::unit238(10))))
        .unwrap();
    tx.fill_hybrid_sign(&acc).unwrap();
    tx.verify_signature().unwrap();

    let wire = tx.serialize();
    let (decoded, _) = TransactionType4::create(&wire).unwrap();
    decoded.verify_signature().unwrap();
    assert_eq!(decoded.size(), tx.size());
    assert!(decoded.size() < TransactionType4::MAX_WIRE_SIZE);
}

#[test]
fn hybrid_type4_dual_alg_verify() {
    init_setup();
    let acc = HybridAccount::create_hybrid_randomly(&random_fill).unwrap();
    let main = Address::from(*acc.address());
    let mut tx = TransactionType4::new_by(main, Amount::unit238(500), 1_730_000_001);
    tx.fill_hybrid_sign(&acc).unwrap();
    tx.verify_signature().unwrap();
    assert!(acc.is_hybrid());
    assert!(main.is_hybrid());
}

#[test]
fn type4_is_neutralized_on_mainnet_but_allowed_off_mainnet() {
    // PQC type 4 is neutralized on mainnet at ALL heights to match the official
    // Istanbul node (which has no type 4)...
    assert!(check_gated_tx(MAINNET_CHAIN_ID, DEV_OPEN_MAX_HEIGHT.saturating_add(1), 4).is_err());
    assert!(check_gated_tx(MAINNET_CHAIN_ID, PQC_TYPE4_OPEN_HEIGHT, 4).is_err());
    assert!(check_gated_tx(MAINNET_CHAIN_ID, 0, 4).is_err());
    // ...but stays available off mainnet (testnet / sidechain / future rollout).
    assert!(check_gated_tx(1u32, PQC_TYPE4_OPEN_HEIGHT, 4).is_ok());
    assert!(check_gated_tx(1u32, 0, 4).is_ok());
}

#[test]
fn type4_execute_simple_transfer_off_mainnet() {
    init_setup();
    let acc = HybridAccount::create_pqc_randomly(&random_fill).unwrap();
    let main = Address::from(*acc.address());
    let to = Address::from_readable(Account::create_by("recipient-2").unwrap().readable()).unwrap();

    let mut tx = TransactionType4::new_by(main, Amount::unit238(1000), 1_730_000_002);
    tx.push_action(Box::new(HacToTrs::create_by(to, Amount::unit238(50))))
        .unwrap();
    tx.fill_hybrid_sign(&acc).unwrap();

    let mut env = basis::component::Env::default();
    env.block.height = 100;
    // PQC type 4 is neutralized on mainnet, so exercise its execution on a
    // non-mainnet chain_id (where PQC remains available).
    env.chain.id = 1;
    env.tx = protocol::transaction::create_tx_info(&tx);

    let state: Box<dyn basis::interface::State> =
        Box::new(testkit::sim::state::FlatMemState::default());
    let logs: Box<dyn basis::interface::Logs> = Box::new(testkit::sim::logs::MemLogs::new());
    let mut ctx = testkit::sim::context::make_ctx_with_logs(env, state, logs, &tx);
    fund_main(&mut ctx, &main);

    tx.execute(&mut ctx).unwrap();

    let st = protocol::state::CoreState::wrap(ctx.state());
    let to_bal = st.balance(&to).unwrap_or_default().hacash;
    assert_eq!(to_bal, Amount::unit238(50));
}

#[test]
fn type4_rejects_wrong_alg_for_pqckey_main() {
    init_setup();
    let pqc = HybridAccount::create_pqc_randomly(&random_fill).unwrap();
    let hybrid = HybridAccount::create_hybrid_randomly(&random_fill).unwrap();
    let main = Address::from(*pqc.address());
    let mut tx = TransactionType4::new_by(main, Amount::unit238(100), 1);
    let sign = tx
        .create_hybrid_sign_by(&hybrid, &tx.hash_with_fee())
        .unwrap_err();
    assert!(
        sign.contains("PQCKEY") || sign.contains("ML-DSA"),
        "{}",
        sign
    );
}
