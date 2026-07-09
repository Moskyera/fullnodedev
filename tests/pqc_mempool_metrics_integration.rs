use basis::interface::Transaction;
use field::*;
use protocol::action::HacToTrs;
use protocol::transaction::TransactionType4;
use protocol::upgrade::{check_gated_tx, DEV_OPEN_MAX_HEIGHT, MAINNET_CHAIN_ID, PQC_TYPE4_OPEN_HEIGHT};
use sys::*;

use testkit::sim::integration::ensure_standard_protocol_setup_for_tests;

fn init_setup() {
    ensure_standard_protocol_setup_for_tests(|_, stuff| sys::calculate_hash(stuff), false);
}

fn random_fill(buf: &mut [u8]) -> Rerr {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i as u8).wrapping_add(3);
    }
    Ok(())
}

#[test]
fn type4_mempool_gate_respects_dev_and_activation_window() {
    assert!(check_gated_tx(MAINNET_CHAIN_ID, 0, 4).is_ok());
    assert!(check_gated_tx(MAINNET_CHAIN_ID, DEV_OPEN_MAX_HEIGHT, 4).is_ok());
    let mid = DEV_OPEN_MAX_HEIGHT.saturating_add(1);
    assert!(check_gated_tx(MAINNET_CHAIN_ID, mid, 4).is_err());
    assert!(check_gated_tx(MAINNET_CHAIN_ID, PQC_TYPE4_OPEN_HEIGHT, 4).is_ok());
}

#[test]
fn type4_wire_size_within_mempool_cap() {
    init_setup();
    let acc = HybridAccount::create_hybrid_randomly(&random_fill).unwrap();
    let main = Address::from(*acc.address());
    let to = Address::from_readable(Account::create_by("recv").unwrap().readable()).unwrap();
    let mut tx = TransactionType4::new_by(main, Amount::unit238(500), 1_730_100_000);
    tx.push_action(Box::new(HacToTrs::create_by(to, Amount::unit238(1))))
        .unwrap();
    tx.fill_hybrid_sign(&acc).unwrap();
    let wire = tx.serialize();
    assert!(wire.len() >= 5_000, "type4 wire should be ~5-6KB, got {}", wire.len());
    assert!(wire.len() <= 16 * 1024);
    let cap = protocol::transaction::effective_max_tx_wire_size(16 * 1024, 4);
    assert!(wire.len() <= cap);
}

#[test]
fn mldsa_verify_observer_records_timing() {
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEEN_US: AtomicU64 = AtomicU64::new(0);
    sys::set_mldsa_verify_observer(|us| {
        SEEN_US.fetch_add(us, Ordering::Relaxed);
    });

    let acc = HybridAccount::create_pqc_randomly(&random_fill).unwrap();
    let msg = sha2(b"metrics-observer-test");
    let body = acc.sign_hash(&msg).unwrap();
    let pk = &body[..1952];
    let sig = &body[1952..];
    assert!(verify_mldsa65_detached(&msg, pk, sig));
    assert!(SEEN_US.load(Ordering::Relaxed) > 0);
}