//! ADVERSARIAL PROBE against the NEW wallet-side unilateral exit driver.
//!
//! The hostile-Hub suite asked "can the user get out at all". This asks the
//! opposite question about the code that was just written to let them: the exit
//! driver signs and submits Type 3 transactions on the user's behalf, which
//! makes it a brand new place to lose money.
//!
//! Every call string below is a string the wallet driver emits or used to. They
//! were copied out of, not paraphrased from:
//!   crates/l2-fast-pay-hub/src/hvm_registry_watchtower.rs
//!     registry_bill_call        :1002-1016   "{fn}({left}, {serial}, {lb}, {hb}, 0x{ls}, 0x{hs})"
//!     registry_finalize_call_source :413-415 "finalize({left})"
//!     registry_renew_channel_call_source :462-475  "renew_channel({left}, {periods})"
//!     registry_renew_registry_call_source :477-487 "renew_registry({periods})"
//!     checked_registry_call     :1026-1031  "lib Registry = 1: {c}\nvar result = Registry.{call}\nassert result == 0\nend"
//!
//! Nothing here is broadcast. testkit::sim::memchain, in process, chain id of a
//! throwaway simulator. No mainnet contact, no real balance.

use field::{AddrOrPtr, Address, Amount, BytesW2, Field, Hash, Serialize, Sign, Uint4};
use protocol::action::{HacFromToTrs, HacToTrs};
use sys::Account;
use testkit::sim::memchain::{MemChain, TxOutput};
use vm::ContractAddress;
use vm::value::Value;

const DOMAIN: &[u8] = b"HPAY/HVM-CHANNEL-REGISTRY/V2";
const CONTRACT_SOURCE: &str = include_str!("../contracts/hpay_channel_registry_v2.fitsh");
const DEPOSIT: u64 = 1_000_000;
const CHALLENGE_BLOCKS: u64 = 6;

/// crates/l2-fast-pay-hub/src/hvm_registry_watchtower.rs:449
/// The size the wallet driver used to ask for. Kept as the thing that must
/// stay refused, not as a description of what the wallet does today.
const OVERSIZED_RENEW_CHANNEL_PERIODS: u64 = 200;
/// crates/l2-fast-pay-hub/src/hvm_registry_watchtower.rs:460
const WALLET_RENEW_REGISTRY_MAX_PERIODS: u64 = 400;

fn addr(account: &Account) -> Address {
    Address::from(account.address().clone())
}

fn channel_key(prefix: &str, left: &Address) -> Value {
    let mut key = prefix.as_bytes().to_vec();
    key.extend_from_slice(left.as_bytes());
    Value::bytes(key)
}

/// `checked_registry_call`, hvm_registry_watchtower.rs:1026-1031, verbatim.
fn call_source(contract: &ContractAddress, call: &str) -> String {
    format!(
        "lib Registry = 1: {}\nvar result = Registry.{}\nassert result == 0\nend",
        contract.to_readable(),
        call
    )
}

struct Fixture {
    chain: MemChain,
    hub: Account,
    user: Account,
    miner: Address,
    network: [u8; 32],
    contract: ContractAddress,
    channel_id: [u8; 16],
}

fn open(seed: &str) -> Fixture {
    let mut chain = MemChain::new();
    chain.set_height(protocol::upgrade::ONLINE_OPEN_HEIGHT);
    let hub = Account::create_by(&format!("attack-hub-{seed}")).unwrap();
    let user = Account::create_by(&format!("attack-user-{seed}")).unwrap();
    let miner = addr(&Account::create_by(&format!("attack-miner-{seed}")).unwrap());
    let hub_a = addr(&hub);
    let user_a = addr(&user);
    for address in [hub_a, user_a] {
        chain.mint_hac(&address, 30_000_000_000_000);
    }
    let network = [0x77_u8; 32];
    let contract = ContractAddress::calculate(&hub_a, &Uint4::from(0));
    let mut deploy = vm::action::ContractDeploy::new();
    deploy.nonce = Uint4::from(0);
    deploy.construct_argv = BytesW2::from(network.to_vec()).unwrap();
    deploy.contract = vm::fitshc::compile(CONTRACT_SOURCE).unwrap().0.into_sto();
    deploy.protocol_cost = Amount::unit238(20_000_000_000_000);
    let hash = chain
        .submit_formal_actions(
            &hub,
            vec![hub_a],
            vec![Box::new(deploy)],
            u8::MAX,
            TxOutput::ContractAddress(contract.clone()),
        )
        .unwrap();
    chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&hash);

    let channel_id = [0x5C_u8; 16];
    let init = format!(
        "init(0x{}, 0, {}, {}, {}, 100)",
        hex::encode(channel_id),
        user_a.to_readable(),
        DEPOSIT,
        CHALLENGE_BLOCKS,
    );
    let init_hash = chain
        .submit_formal_main_call_fitsh_with_signers(
            &user,
            &[&hub],
            vec![user_a, contract.to_addr(), hub_a],
            &call_source(&contract, &init),
            u8::MAX,
        )
        .unwrap();
    chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&init_hash);

    let mut fund = HacToTrs::new();
    fund.to = AddrOrPtr::from_addr(contract.to_addr());
    fund.hacash = Amount::zhu(DEPOSIT);
    let fund_hash = chain
        .submit_formal_actions(
            &user,
            vec![user_a, contract.to_addr()],
            vec![Box::new(fund)],
            u8::MAX,
            TxOutput::None,
        )
        .unwrap();
    chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&fund_hash);

    Fixture {
        chain,
        hub,
        user,
        miner,
        network,
        contract,
        channel_id,
    }
}

struct Bill {
    serial: u64,
    left_balance: u64,
    hub_balance: u64,
    left_sign: Sign,
    hub_sign: Sign,
}

impl Fixture {
    fn cosigned(&self, serial: u64, left_balance: u64, hub_balance: u64) -> Bill {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(DOMAIN);
        bytes.extend_from_slice(&self.network);
        bytes.extend_from_slice(self.contract.to_addr().as_bytes());
        bytes.extend_from_slice(&self.channel_id);
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(addr(&self.user).as_bytes());
        bytes.extend_from_slice(addr(&self.hub).as_bytes());
        bytes.extend_from_slice(&DEPOSIT.to_be_bytes());
        bytes.extend_from_slice(&CHALLENGE_BLOCKS.to_be_bytes());
        bytes.extend_from_slice(&serial.to_be_bytes());
        bytes.extend_from_slice(&left_balance.to_be_bytes());
        bytes.extend_from_slice(&hub_balance.to_be_bytes());
        let commitment = Hash::from(sys::sha3(bytes));
        Bill {
            serial,
            left_balance,
            hub_balance,
            left_sign: Sign::create_by(&self.user, &commitment),
            hub_sign: Sign::create_by(&self.hub, &commitment),
        }
    }

    /// `registry_bill_call`, hvm_registry_watchtower.rs:1002-1016, verbatim.
    fn bill_call(&self, name: &str, bill: &Bill) -> String {
        format!(
            "{}({}, {}, {}, {}, 0x{}, 0x{})",
            name,
            addr(&self.user).to_readable(),
            bill.serial,
            bill.left_balance,
            bill.hub_balance,
            hex::encode(bill.left_sign.serialize()),
            hex::encode(bill.hub_sign.serialize()),
        )
    }

    fn call_by(&mut self, payer: &Account, call: &str) -> Result<(), String> {
        let hash = self
            .chain
            .submit_formal_main_call_fitsh_with_signers(
                payer,
                &[],
                vec![addr(payer), self.contract.to_addr()],
                &call_source(&self.contract, call),
                u8::MAX,
            )
            .map_err(|error| format!("build failed: {error:?}"))?;
        let block = self
            .chain
            .confirm_formal_block_observing_failures(self.miner)
            .map_err(|error| format!("execute failed: {error:?}"))?;
        let receipt = block.receipt(&hash).expect("receipt");
        if receipt.is_error() {
            Err(format!("{:?}", receipt.error))
        } else {
            Ok(())
        }
    }

    fn payout_by(
        &mut self,
        payer: &Account,
        recipient: Address,
        amount: u64,
    ) -> Result<(), String> {
        let raw = self.payout_bytes(payer, recipient, amount);
        self.submit_raw(&raw)
    }

    /// The exact wire bytes of an Action 14 payout, so they can be replayed.
    fn payout_bytes(&mut self, payer: &Account, recipient: Address, amount: u64) -> Vec<u8> {
        let mut action = HacFromToTrs::new();
        action.from = AddrOrPtr::from_addr(self.contract.to_addr());
        action.to = AddrOrPtr::from_addr(recipient);
        action.hacash = Amount::zhu(amount);
        self.chain
            .build_formal_actions_raw(
                payer,
                &[],
                vec![addr(payer), self.contract.to_addr(), recipient],
                vec![Box::new(action)],
                u8::MAX,
            )
            .expect("payout bytes")
    }

    fn submit_raw(&mut self, raw: &[u8]) -> Result<(), String> {
        let hash = self
            .chain
            .submit_signed_transaction_raw(raw, TxOutput::None)
            .map_err(|error| format!("submit failed: {error:?}"))?;
        let block = self
            .chain
            .confirm_formal_block_observing_failures(self.miner)
            .map_err(|error| format!("execute failed: {error:?}"))?;
        let Some(receipt) = block.receipt(&hash) else {
            return Err("transaction was not included in the block".into());
        };
        if receipt.is_error() {
            Err(format!("{:?}", receipt.error))
        } else {
            Ok(())
        }
    }

    fn u64_at(&self, prefix: &str) -> u64 {
        match self
            .chain
            .storage(&self.contract, &channel_key(prefix, &addr(&self.user)))
        {
            Value::U64(v) => v,
            other => panic!("{prefix} must be u64, got {other:?}"),
        }
    }

    fn status(&self) -> String {
        format!(
            "{:?}",
            self.chain
                .storage(&self.contract, &channel_key("c_status_", &addr(&self.user)))
        )
    }

    fn zhu(&self, address: &Address) -> u64 {
        self.chain.balance(address).to_zhu_u64().expect("balance")
    }

    fn live_blocks(&self, prefix: &str) -> u64 {
        let h = self.chain.height();
        vm::VMStateRead::wrap(self.chain.state())
            .debug_storage_get(
                &vm::rt::GasExtra::new(h),
                &vm::rt::SpaceCap::new(h),
                h,
                &self.contract.to_addr(),
                &channel_key(prefix, &addr(&self.user)),
            )
            .unwrap()
            .expect("entry")
            .live_blocks
    }

    fn skip(&mut self, blocks: u64) {
        for _ in 0..blocks {
            self.chain.confirm_empty_formal_block(self.miner).unwrap();
        }
    }
}

// ---------------------------------------------------------------------------
// ATTACK 1 - CLOSED IN THE WALLET, still measured here at the contract.
//
// plan_user_exit_step answers a short lease by renewing first, and it used to
// ask for HVM_REGISTRY_RENEW_CHANNEL_MAX_PERIODS = 200 against a contract that
// asserts `periods <= MAX_RENT_STEP` with MAX_RENT_STEP = 150. The one escape
// from the one irreversible outcome in this system aborted on execution.
//
// The wallet constant is now the contract's own figure, and a test in
// crates/l2-fast-pay-hub/src/hvm_registry_pilot.rs re-reads MAX_RENT_STEP out
// of the contract source so the two cannot drift again. This file cannot see
// the wallet - different repository, no dependency - so what it keeps is the
// CHAIN half: 200 and 400 really are refused, 150 really does work. That is the
// evidence the wallet-side cap has to stay under, and it is worth keeping
// exactly because a future contract revision could move it again.
// ---------------------------------------------------------------------------
#[test]
fn attack_one_the_drivers_lease_rescue_is_refused_by_the_contract() {
    let mut f = open("renewcap");
    let user = f.user.clone();
    let user_a = addr(&user);
    let before_life = f.live_blocks("c_status_");
    let before_hac = f.zhu(&user_a);
    println!("ATTACK 1: lease before = {before_life} live blocks, user holds {before_hac} zhu");

    let channel_call = format!(
        "renew_channel({}, {OVERSIZED_RENEW_CHANNEL_PERIODS})",
        user_a.to_readable()
    );
    println!("ATTACK 1: the driver emits  {channel_call}");
    let channel_result = f.call_by(&user, &channel_call);
    let after_life = f.live_blocks("c_status_");
    let after_hac = f.zhu(&user_a);
    println!(
        "ATTACK 1: renew_channel(...,{OVERSIZED_RENEW_CHANNEL_PERIODS}) -> {channel_result:?}"
    );
    println!(
        "ATTACK 1: lease after = {after_life} live blocks (moved {}), user paid {} zhu in fees",
        after_life as i64 - before_life as i64,
        before_hac - after_hac
    );

    let registry_call = format!("renew_registry({WALLET_RENEW_REGISTRY_MAX_PERIODS})");
    println!("ATTACK 1: the driver emits  {registry_call}");
    let registry_result = f.call_by(&user, &registry_call);
    println!(
        "ATTACK 1: renew_registry({WALLET_RENEW_REGISTRY_MAX_PERIODS}) -> {registry_result:?}"
    );

    // Contract line: `assert periods <= MAX_RENT_STEP`, MAX_RENT_STEP = 150.
    let ok = f.call_by(
        &user,
        &format!("renew_channel({}, 150)", user_a.to_readable()),
    );
    println!(
        "ATTACK 1: renew_channel(...,150) -> {ok:?}, lease now {} live blocks",
        f.live_blocks("c_status_")
    );

    assert!(
        channel_result.is_err(),
        "MAX_RENT_STEP is 150 so a 200-period renewal must abort; if this ever passes the contract has moved and the wallet cap must move with it"
    );
    assert!(registry_result.is_err(), "the driver's 400 must abort too");
    // An aborted renewal buys nothing: the lease only keeps counting down with
    // the block that carried the failed transaction. A successful 150 would
    // have added 15_000 blocks, as the last call below shows.
    assert!(
        after_life < before_life,
        "an aborted renewal buys no lease at all; the clock only kept running"
    );
    assert!(ok.is_ok(), "150 is inside MAX_RENT_STEP and must work");
    assert!(
        f.live_blocks("c_status_") > before_life,
        "the call the driver never builds is the one that works"
    );
}

// ---------------------------------------------------------------------------
// ATTACK 2 - pay to exit a channel that has nothing in it.
// ---------------------------------------------------------------------------
#[test]
fn attack_two_the_user_pays_to_exit_an_empty_channel() {
    let mut f = open("empty");
    let user = f.user.clone();
    let user_a = addr(&user);

    // The user has spent the whole channel. This is the ordinary end state of a
    // one-directional rail, not an exotic one.
    let spent = f.cosigned(9, 0, DEPOSIT);
    let before = f.zhu(&user_a);
    println!("ATTACK 2: head bill is serial 9, left_balance 0. User holds {before} zhu");

    let challenge = f.bill_call("challenge", &spent);
    let r1 = f.call_by(&user, &challenge);
    println!("ATTACK 2: challenge with a zero-balance bill -> {r1:?}");
    f.skip(CHALLENGE_BLOCKS + 1);
    let r2 = f.call_by(&user, &format!("finalize({})", user_a.to_readable()));
    println!("ATTACK 2: finalize -> {r2:?}, status now {}", f.status());
    println!(
        "ATTACK 2: c_left_balance_ = {}, c_left_claimed_ = {:?}",
        f.u64_at("c_left_balance_"),
        f.chain
            .storage(&f.contract, &channel_key("c_left_claimed_", &user_a))
    );

    let claim = f.payout_by(&user, user_a, 0);
    println!("ATTACK 2: Action 14 claim for 0 zhu -> {claim:?}");

    let after = f.zhu(&user_a);
    println!(
        "ATTACK 2: user started this exit with {before} zhu and ended with {after} zhu. NET {} zhu",
        after as i64 - before as i64
    );

    assert!(
        r1.is_ok(),
        "the contract happily accepts a zero-value close"
    );
    assert!(r2.is_ok(), "and happily finalizes it");
    assert!(
        after < before,
        "the user spent network fees and recovered nothing"
    );
}

// ---------------------------------------------------------------------------
// ATTACK 3 - race the exit against a cooperative close.
// ---------------------------------------------------------------------------
#[test]
fn attack_three_a_cooperative_close_races_the_users_challenge() {
    let mut f = open("race");
    let user = f.user.clone();
    let hub = f.hub.clone();
    let user_a = addr(&user);

    // The user's own head, and one the Hub also holds that is newer.
    let user_head = f.cosigned(3, 900_000, 100_000);
    let newer = f.cosigned(4, 850_000, 150_000);

    let r1 = f.call_by(&user, &f.bill_call("challenge", &user_head).clone());
    println!(
        "ATTACK 3: user challenge at serial 3 -> {r1:?}; status {}, deadline {}, left_balance {}",
        f.status(),
        f.u64_at("c_deadline_"),
        f.u64_at("c_left_balance_")
    );

    // The plan the wallet made at this instant: claim 900_000 once final.
    let planned_amount = f.u64_at("c_left_balance_");

    // The Hub, mid-window, does not respond. It cooperatively closes with a
    // newer bill: `assert c_status_ != FINAL` lets this run from CHALLENGING.
    let coop = f.bill_call("cooperative_close", &newer);
    let r2 = f.call_by(&hub, &coop);
    println!(
        "ATTACK 3: hub cooperative_close at serial 4 mid-window -> {r2:?}; status {}, left_balance {}, height {} vs deadline {}",
        f.status(),
        f.u64_at("c_left_balance_"),
        f.chain.height(),
        f.u64_at("c_deadline_")
    );

    // The user's already-planned claim carries the pre-race amount.
    let stale_claim = f.payout_by(&user, user_a, planned_amount);
    println!("ATTACK 3: the user's pre-race claim for {planned_amount} zhu -> {stale_claim:?}");
    let fresh_claim = f.payout_by(&user, user_a, f.u64_at("c_left_balance_"));
    println!(
        "ATTACK 3: re-planned claim for {} zhu -> {fresh_claim:?}",
        f.u64_at("c_left_balance_")
    );

    // What bounds this race is in the contract, not in the driver: every entry
    // point runs `verify_bill`, whose second line is
    //   assert verify_signature(commitment, left, left_sign)
    // so the Hub can only ever settle on a split the USER already signed. It
    // can pick which of the user's own bills wins the race; it cannot invent
    // one. (hpay_channel_registry_v2.fitsh, verify_bill.)
    let _ = &hub;

    assert!(r2.is_ok(), "cooperative_close runs from CHALLENGING");
    assert!(
        stale_claim.is_err(),
        "a claim built against the pre-race snapshot must abort, not overpay"
    );
    assert!(fresh_claim.is_ok(), "re-planning recovers the real balance");
}

// ---------------------------------------------------------------------------
// ATTACK 4 - replay a user exit transaction.
// ---------------------------------------------------------------------------
#[test]
fn attack_four_replaying_the_users_exit_transactions() {
    let mut f = open("replay");
    let user = f.user.clone();
    let user_a = addr(&user);
    let head = f.cosigned(3, 900_000, 100_000);

    // (a) the same challenge, submitted twice, as a driver with no durable
    //     per-step record would do after a crash between signing and submitting.
    let challenge = f.bill_call("challenge", &head);
    let first = f.call_by(&user, &challenge);
    let second = f.call_by(&user, &challenge);
    println!("ATTACK 4a: challenge #1 -> {first:?}");
    println!("ATTACK 4a: the SAME challenge re-signed and re-submitted -> {second:?}");

    f.skip(CHALLENGE_BLOCKS + 1);
    let fin1 = f.call_by(&user, &format!("finalize({})", user_a.to_readable()));
    let fin2 = f.call_by(&user, &format!("finalize({})", user_a.to_readable()));
    println!("ATTACK 4b: finalize #1 -> {fin1:?}");
    println!("ATTACK 4b: finalize #2 -> {fin2:?}");

    // (c) the Action 14 payout, replayed byte for byte.
    let amount = f.u64_at("c_left_balance_");
    let raw = f.payout_bytes(&user, user_a, amount);
    let before = f.zhu(&user_a);
    let paid = f.submit_raw(&raw);
    let mid = f.zhu(&user_a);
    println!("ATTACK 4c: claim for {amount} zhu -> {paid:?}, balance {before} -> {mid}");
    let replayed = f.submit_raw(&raw);
    let after = f.zhu(&user_a);
    println!(
        "ATTACK 4c: the IDENTICAL signed bytes resubmitted -> {replayed:?}, balance now {after}"
    );

    // (d) a freshly signed second claim: different timestamp, different hash,
    //     valid signature. This is what a driver with no durable record does.
    let resigned = f.payout_bytes(&user, user_a, amount);
    assert_ne!(raw, resigned, "a re-signed claim is different bytes");
    let resigned_result = f.submit_raw(&resigned);
    let final_balance = f.zhu(&user_a);
    println!(
        "ATTACK 4d: a re-signed second claim -> {resigned_result:?}, balance now {final_balance}"
    );
    println!(
        "ATTACK 4: contract still holds {} zhu; g_left_claimable = {:?}",
        f.zhu(&f.contract.to_addr()),
        f.chain
            .storage(&f.contract, &Value::bytes(b"g_left_claimable".to_vec()))
    );

    assert!(first.is_ok());
    assert!(
        second.is_err(),
        "a replayed challenge must not restart the window"
    );
    assert!(fin1.is_ok());
    assert!(fin2.is_err(), "a replayed finalize must not re-settle");
    assert!(paid.is_ok());
    assert!(
        replayed.is_err(),
        "identical claim bytes must never pay twice"
    );
    assert!(
        resigned_result.is_err(),
        "a re-signed claim must never pay twice"
    );
    assert_eq!(
        final_balance, mid,
        "no replay moved a second zhu to the user"
    );
}

// ---------------------------------------------------------------------------
// ATTACK 6 - the responder that exists to protect a sleeping user takes money
// off them.
//
// hvm_registry_response_watch.rs says a sleeping user "answers nothing" and
// that "the worst an absent [operator] can do is nothing at all". Its decision
// (decide_response_watch_action :207-211) is RespondWithLatestBill whenever
// chain_serial < kit.latest.serial, with no test of which way that moves the
// user. On a one-directional rail a higher serial is always a LOWER left
// balance, so the only thing this can ever do to a stale challenge is take
// money off the user it was hired to defend.
// ---------------------------------------------------------------------------
#[test]
fn attack_six_the_sleeping_users_watcher_answers_against_them() {
    let mut f = open("watcher");
    let hub = f.hub.clone();
    let user = f.user.clone();
    let user_a = addr(&user);

    // A hostile Hub challenges with a STALE bill while the user is asleep.
    let stale = f.cosigned(2, 950_000, 50_000);
    // The kit the user handed their watchtower is their true latest.
    let latest = f.cosigned(6, 300_000, 700_000);

    f.call_by(&hub, &f.bill_call("challenge", &stale).clone())
        .unwrap();
    println!(
        "ATTACK 6: hostile Hub challenged with STALE serial 2. Chain owes the user {}",
        f.u64_at("c_left_balance_")
    );
    let if_nobody_answers = f.u64_at("c_left_balance_");

    // The watchtower wakes up and does exactly what the module decides.
    let responded = f.call_by(&user, &f.bill_call("respond", &latest).clone());
    println!(
        "ATTACK 6: the watcher responded with serial 6 -> {responded:?}. Chain now owes the user {}",
        f.u64_at("c_left_balance_")
    );
    let after_watcher = f.u64_at("c_left_balance_");

    f.skip(CHALLENGE_BLOCKS + 1);
    f.call_by(&user, &format!("finalize({})", user_a.to_readable()))
        .unwrap();
    let paid = f.u64_at("c_left_balance_");
    f.payout_by(&user, user_a, paid).unwrap();
    println!("ATTACK 6: user was paid {paid} zhu");
    println!(
        "ATTACK 6: had the watcher stayed OFFLINE the user would have been paid {if_nobody_answers} zhu. \
         The watcher cost the user {} zhu plus its own fee.",
        if_nobody_answers - after_watcher
    );

    assert!(responded.is_ok());
    assert!(
        after_watcher < if_nobody_answers,
        "on this rail responding to a stale challenge can only move money away from the user"
    );
    assert_eq!(paid, 300_000);
}

// ---------------------------------------------------------------------------
// ATTACK 5 - exit on a bill that is not the user's latest.
// ---------------------------------------------------------------------------
#[test]
fn attack_five_exiting_on_a_bill_that_is_not_the_latest() {
    let mut f = open("stale");
    let user = f.user.clone();
    let hub = f.hub.clone();
    let user_a = addr(&user);

    let stale = f.cosigned(1, 990_000, 10_000);
    let latest = f.cosigned(7, 400_000, 600_000);
    println!(
        "ATTACK 5: stale bill serial 1 pays the user {}, latest serial 7 pays the user {}",
        stale.left_balance, latest.left_balance
    );

    // The user exits on the stale one. The wallet driver would build exactly
    // this if it were handed an old exported kit: plan_user_exit_step validates
    // the kit's signatures and never compares it to the durable head.
    let r1 = f.call_by(&user, &f.bill_call("challenge", &stale).clone());
    println!(
        "ATTACK 5: challenge with the STALE bill -> {r1:?}; chain now carries left_balance {}",
        f.u64_at("c_left_balance_")
    );

    // Correcting downwards is impossible: verify_bill asserts serial > c_serial_.
    let correct = f.call_by(&user, &f.bill_call("respond", &stale).clone());
    println!("ATTACK 5: the user tries to re-submit the same serial -> {correct:?}");

    // The Hub's watchtower answers with the truth, and the user is settled at
    // what they actually hold.
    let r2 = f.call_by(&hub, &f.bill_call("respond", &latest).clone());
    println!(
        "ATTACK 5: hub responds with serial 7 -> {r2:?}; chain now carries left_balance {}",
        f.u64_at("c_left_balance_")
    );

    f.skip(CHALLENGE_BLOCKS + 1);
    f.call_by(&user, &format!("finalize({})", user_a.to_readable()))
        .unwrap();
    let settled = f.u64_at("c_left_balance_");
    f.payout_by(&user, user_a, settled).unwrap();
    println!("ATTACK 5: user was paid {settled} zhu, which is the LATEST bill's figure");

    assert!(r1.is_ok());
    assert!(
        correct.is_err(),
        "the chain refuses a bill at or below the serial it already holds"
    );
    assert_eq!(
        settled, latest.left_balance,
        "a stale exit is corrected by the Hub, downward, to the truth"
    );
    assert!(
        stale.left_balance > latest.left_balance,
        "on this one-directional rail an older bill always pays the user MORE, never less"
    );
}
