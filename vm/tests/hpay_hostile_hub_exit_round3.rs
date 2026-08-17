//! ROUND 3 - adversarial re-audit of the two "closed" traps.
//!
//! Everything here is driven against `testkit::sim::memchain::MemChain`: real
//! `TransactionType3` bytes, real Type3 signature verification, real `BlockV1`
//! execution, real HVM contract state, real Action 14 `HacFromToTrs` payouts.
//!
//! Default assumption for every probe: THE USER IS TRAPPED. A probe only
//! reports "user got out" when a real Action 14 lands coin on the user's own
//! address and the balance delta is asserted exactly.
//!
//! NOTE ON THE HARNESS: `MemChain::new()` takes a process-global mutex and
//! holds it for the chain's whole lifetime (`testkit/src/sim/integration.rs:11`,
//! `memchain.rs:403`), so only ONE rig may be alive at a time. Every probe that
//! needs several chains builds them in sequence and drops each before the next.

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

fn addr(account: &Account) -> Address {
    Address::from(account.address().clone())
}

fn channel_key(prefix: &str, left: &Address) -> Value {
    let mut key = prefix.as_bytes().to_vec();
    key.extend_from_slice(left.as_bytes());
    Value::bytes(key)
}

#[derive(Clone)]
struct Bill {
    network: [u8; 32],
    contract: Address,
    channel_id: [u8; 16],
    reuse: u32,
    left: Address,
    hub: Address,
    total: u64,
    challenge_blocks: u64,
    serial: u64,
    left_balance: u64,
    hub_balance: u64,
    left_sign: Sign,
    hub_sign: Sign,
}

impl Bill {
    fn commitment(&self) -> Hash {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(DOMAIN);
        bytes.extend_from_slice(&self.network);
        bytes.extend_from_slice(self.contract.as_bytes());
        bytes.extend_from_slice(&self.channel_id);
        bytes.extend_from_slice(&self.reuse.to_be_bytes());
        bytes.extend_from_slice(self.left.as_bytes());
        bytes.extend_from_slice(self.hub.as_bytes());
        bytes.extend_from_slice(&self.total.to_be_bytes());
        bytes.extend_from_slice(&self.challenge_blocks.to_be_bytes());
        bytes.extend_from_slice(&self.serial.to_be_bytes());
        bytes.extend_from_slice(&self.left_balance.to_be_bytes());
        bytes.extend_from_slice(&self.hub_balance.to_be_bytes());
        Hash::from(sys::sha3(bytes))
    }

    fn cosigned(mut self, left: &Account, hub: &Account) -> Self {
        let commitment = self.commitment();
        self.left_sign = Sign::create_by(left, &commitment);
        self.hub_sign = Sign::create_by(hub, &commitment);
        self
    }

    fn self_signed_only(mut self, left: &Account) -> Self {
        let commitment = self.commitment();
        self.left_sign = Sign::create_by(left, &commitment);
        self.hub_sign = Sign::create_by(left, &commitment);
        self
    }
}

fn call_source(contract: &ContractAddress, call: &str) -> String {
    format!(
        "lib Registry = 1: {}\nvar result = Registry.{}\nassert result == 0\nend",
        contract.to_readable(),
        call
    )
}

fn bill_call(name: &str, bill: &Bill) -> String {
    format!(
        "{}({}, {}, {}, {}, 0x{}, 0x{})",
        name,
        bill.left.to_readable(),
        bill.serial,
        bill.left_balance,
        bill.hub_balance,
        hex::encode(bill.left_sign.serialize()),
        hex::encode(bill.hub_sign.serialize()),
    )
}

fn submit_solo_call(
    chain: &mut MemChain,
    payer: &Account,
    contract: &ContractAddress,
    call: &str,
) -> Hash {
    chain
        .submit_formal_main_call_fitsh_with_signers(
            payer,
            &[],
            vec![addr(payer), contract.to_addr()],
            &call_source(contract, call),
            u8::MAX,
        )
        .expect("build solo registry call")
}

fn confirm_solo_call(
    chain: &mut MemChain,
    payer: &Account,
    contract: &ContractAddress,
    call: &str,
    miner: Address,
) {
    let hash = submit_solo_call(chain, payer, contract, call);
    chain
        .confirm_formal_block(miner)
        .expect("execute solo registry call")
        .expect_success(&hash);
}

fn confirm_solo_call_rejected(
    chain: &mut MemChain,
    payer: &Account,
    contract: &ContractAddress,
    call: &str,
    miner: Address,
) -> String {
    let hash = submit_solo_call(chain, payer, contract, call);
    let block = chain
        .confirm_formal_block_observing_failures(miner)
        .expect("execute expected-rejection call");
    let receipt = block.receipt(&hash).expect("expected-rejection receipt");
    assert!(
        receipt.is_error(),
        "HOSTILE ESCAPE UNEXPECTEDLY SUCCEEDED: {call}"
    );
    format!("{:?}", receipt.error)
}

/// Run a call and report whether it succeeded, without asserting either way.
fn try_solo_call(
    chain: &mut MemChain,
    payer: &Account,
    contract: &ContractAddress,
    call: &str,
    miner: Address,
) -> (bool, String) {
    let hash = submit_solo_call(chain, payer, contract, call);
    let block = chain
        .confirm_formal_block_observing_failures(miner)
        .expect("execute probe call");
    let receipt = block.receipt(&hash).expect("probe receipt");
    (!receipt.is_error(), format!("{:?}", receipt.error))
}

fn submit_payout(
    chain: &mut MemChain,
    payer: &Account,
    contract: &ContractAddress,
    recipient: Address,
    amount: u64,
) -> Hash {
    let mut action = HacFromToTrs::new();
    action.from = AddrOrPtr::from_addr(contract.to_addr());
    action.to = AddrOrPtr::from_addr(recipient);
    action.hacash = Amount::zhu(amount);
    chain
        .submit_formal_actions(
            payer,
            vec![addr(payer), contract.to_addr(), recipient],
            vec![Box::new(action)],
            u8::MAX,
            TxOutput::None,
        )
        .expect("build registry payout")
}

fn try_payout(
    chain: &mut MemChain,
    payer: &Account,
    contract: &ContractAddress,
    recipient: Address,
    amount: u64,
    miner: Address,
) -> (bool, String) {
    let hash = submit_payout(chain, payer, contract, recipient, amount);
    let block = chain
        .confirm_formal_block_observing_failures(miner)
        .expect("execute payout probe");
    let receipt = block.receipt(&hash).expect("payout probe receipt");
    (!receipt.is_error(), format!("{:?}", receipt.error))
}

// ---------------------------------------------------------------------------

struct Rig {
    chain: MemChain,
    hub: Account,
    user: Account,
    stranger: Account,
    miner: Address,
    network: [u8; 32],
    contract: ContractAddress,
    channel_id: [u8; 16],
    challenge_blocks: u64,
    deposit: u64,
}

impl Rig {
    /// Deploy the registry only. Nothing is opened and nothing is funded.
    fn deploy(seed: &str) -> Self {
        let mut chain = MemChain::new();
        chain.set_height(protocol::upgrade::ONLINE_OPEN_HEIGHT);
        let hub = Account::create_by(&format!("r3-hub-{seed}")).unwrap();
        let user = Account::create_by(&format!("r3-user-{seed}")).unwrap();
        let stranger = Account::create_by(&format!("r3-stranger-{seed}")).unwrap();
        let miner = addr(&Account::create_by(&format!("r3-miner-{seed}")).unwrap());
        let hub_a = addr(&hub);
        for address in [hub_a, addr(&user), addr(&stranger)] {
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

        Self {
            chain,
            hub,
            user,
            stranger,
            miner,
            network,
            contract,
            channel_id: [0x5C_u8; 16],
            challenge_blocks: CHALLENGE_BLOCKS,
            deposit: DEPOSIT,
        }
    }

    /// Co-signed `init`. Both parties sign; this is the one cooperative step.
    fn init(&mut self) {
        let call = format!(
            "init(0x{}, 0, {}, {}, {}, 100)",
            hex::encode(self.channel_id),
            addr(&self.user).to_readable(),
            self.deposit,
            self.challenge_blocks,
        );
        let hash = self
            .chain
            .submit_formal_main_call_fitsh_with_signers(
                &self.user,
                &[&self.hub],
                vec![addr(&self.user), self.contract.to_addr(), addr(&self.hub)],
                &call_source(&self.contract, &call),
                u8::MAX,
            )
            .expect("build channel init");
        self.chain
            .confirm_formal_block(self.miner)
            .unwrap()
            .expect_success(&hash);
    }

    /// The user pays the deposit in with a plain Action 1 `HacToTrs`. This is
    /// the ONLY thing the chain requires in order to take custody: no wallet,
    /// no countersignature, no gate.
    fn fund_raw(&mut self, amount: u64) -> (bool, String) {
        let mut fund = HacToTrs::new();
        fund.to = AddrOrPtr::from_addr(self.contract.to_addr());
        fund.hacash = Amount::zhu(amount);
        let hash = self
            .chain
            .submit_formal_actions(
                &self.user,
                vec![addr(&self.user), self.contract.to_addr()],
                vec![Box::new(fund)],
                u8::MAX,
                TxOutput::None,
            )
            .expect("build channel funding");
        let block = self
            .chain
            .confirm_formal_block_observing_failures(self.miner)
            .expect("execute funding");
        let receipt = block.receipt(&hash).expect("funding receipt");
        (!receipt.is_error(), format!("{:?}", receipt.error))
    }

    fn bill(&self, serial: u64, left_balance: u64, hub_balance: u64) -> Bill {
        Bill {
            network: self.network,
            contract: self.contract.to_addr(),
            channel_id: self.channel_id,
            reuse: 0,
            left: addr(&self.user),
            hub: addr(&self.hub),
            total: self.deposit,
            challenge_blocks: self.challenge_blocks,
            serial,
            left_balance,
            hub_balance,
            left_sign: Sign::new(),
            hub_sign: Sign::new(),
        }
    }

    fn status(&self) -> Value {
        self.chain
            .storage(&self.contract, &channel_key("c_status_", &addr(&self.user)))
    }

    fn deadline(&self) -> u64 {
        match self.chain.storage(
            &self.contract,
            &channel_key("c_deadline_", &addr(&self.user)),
        ) {
            Value::U64(v) => v,
            other => panic!("deadline must be u64, got {other:?}"),
        }
    }

    fn contract_balance(&self) -> u64 {
        self.chain
            .balance(&self.contract.to_addr())
            .to_zhu_u64()
            .expect("contract balance")
    }

    fn user_balance(&self) -> u64 {
        self.chain
            .balance(&addr(&self.user))
            .to_zhu_u64()
            .expect("user balance")
    }

    fn lease(&self, key: &Value) -> Option<(u64, u64)> {
        let h = self.chain.height();
        vm::VMStateRead::wrap(self.chain.state())
            .debug_storage_get(
                &vm::rt::GasExtra::new(h),
                &vm::rt::SpaceCap::new(h),
                h,
                &self.contract.to_addr(),
                key,
            )
            .unwrap()
            .map(|d| (d.live_blocks, d.recover_blocks))
    }

    fn channel_lease(&self, prefix: &str) -> Option<(u64, u64)> {
        self.lease(&channel_key(prefix, &addr(&self.user)))
    }

    fn global_lease(&self, name: &str) -> Option<(u64, u64)> {
        self.lease(&Value::bytes(name.as_bytes().to_vec()))
    }

    /// Every door out of the contract, tried in turn by the user alone with
    /// whatever bill they hold. Returns whether ANY of them put coin on the
    /// user's address, plus a transcript.
    fn try_every_door(&mut self, ticket: &Bill) -> (bool, String) {
        let miner = self.miner;
        let contract = self.contract.clone();
        let user = self.user.clone();
        let stranger = self.stranger.clone();
        let user_a = addr(&user);
        let before = self.user_balance();
        let mut log = String::new();

        let (ok_ch, e_ch) = try_solo_call(
            &mut self.chain,
            &user,
            &contract,
            &bill_call("challenge", ticket),
            miner,
        );
        log.push_str(&format!("  challenge           -> ok={ok_ch} {e_ch}\n"));

        if ok_ch {
            let deadline = self.deadline();
            let now = self.chain.height();
            if deadline > now && deadline - now <= 100_000 {
                self.chain
                    .confirm_empty_formal_blocks_to_height(miner, deadline)
                    .unwrap();
            } else {
                log.push_str(&format!(
                    "  (deadline {deadline} is {} blocks away - not waitable)\n",
                    deadline.saturating_sub(now)
                ));
            }
        }

        let (ok_fin, e_fin) = try_solo_call(
            &mut self.chain,
            &user,
            &contract,
            &format!("finalize({})", user_a.to_readable()),
            miner,
        );
        log.push_str(&format!("  finalize            -> ok={ok_fin} {e_fin}\n"));

        let (ok_close, e_close) = try_solo_call(
            &mut self.chain,
            &user,
            &contract,
            &bill_call("cooperative_close", ticket),
            miner,
        );
        log.push_str(&format!(
            "  cooperative_close   -> ok={ok_close} {e_close}\n"
        ));

        let (ok_rc, e_rc) = try_solo_call(
            &mut self.chain,
            &user,
            &contract,
            &format!("renew_channel({}, 100)", user_a.to_readable()),
            miner,
        );
        log.push_str(&format!("  renew_channel(100)  -> ok={ok_rc} {e_rc}\n"));

        let (ok_rr, e_rr) = try_solo_call(
            &mut self.chain,
            &user,
            &contract,
            "renew_registry(100)",
            miner,
        );
        log.push_str(&format!("  renew_registry(100) -> ok={ok_rr} {e_rr}\n"));

        let held = self.contract_balance();
        let (ok_pay, e_pay) = try_payout(
            &mut self.chain,
            &stranger,
            &contract,
            user_a,
            held.max(1),
            miner,
        );
        log.push_str(&format!("  action14 {held} zhu -> ok={ok_pay} {e_pay}\n"));

        let after = self.user_balance();
        log.push_str(&format!(
            "  contract still holds = {}\n",
            self.contract_balance()
        ));
        (after > before, log)
    }
}

// ===========================================================================
// PROBE 1 - "fund through a route that skips the new gate".
//
// The new gate lives in `build_hvm_registry_pilot_exact_funding` in the wallet.
// The chain has never heard of it. A plain Action 1 transfer - the thing every
// wallet, exchange, hardware signer and shell script on earth can build - takes
// custody with no countersigned refund anywhere in sight.
// ===========================================================================
#[test]
fn probe_1_a_plain_transfer_still_funds_a_channel_with_no_countersigned_refund() {
    let mut f = Rig::deploy("p1");
    f.init();

    let (ok, err) = f.fund_raw(DEPOSIT);
    assert!(ok, "the chain enforces no gate at all: {err}");
    assert_eq!(f.status(), Value::U8(2), "channel is OPEN");
    assert_eq!(f.contract_balance(), DEPOSIT);
    assert_eq!(
        f.chain
            .storage(&f.contract, &channel_key("c_serial_", &addr(&f.user))),
        Value::U64(0),
        "no bill has ever been agreed - the Hub countersigned nothing"
    );

    let forged = f.bill(1, DEPOSIT, 0).self_signed_only(&f.user.clone());
    let (got_out, log) = f.try_every_door(&forged);
    println!("PROBE 1 - funded by a raw Action 1, no countersignature:\n{log}");
    assert!(!got_out, "unexpected escape");
    assert_eq!(
        f.contract_balance(),
        DEPOSIT,
        "TRAPPED: the wallet-side gate is not a chain-side gate"
    );
}

// ===========================================================================
// PROBE 2a - the countersigned refund names one channel; the channel actually
// opened is a different one.
//
// `PayableHAC` checks exactly three things: status == FUNDING, c_paid_ == 0,
// amount == c_deposit_. It never looks at c_id_, c_reuse_ or c_challenge_.
// `bill_hash` binds all of them. So the funding transaction cannot tell whether
// the record it is paying into is the record its refund was signed against.
// ===========================================================================
#[test]
fn probe_2a_refund_bound_to_a_different_channel_id_is_worthless() {
    let mut f = Rig::deploy("p2a");
    // Binding agreed off chain: channel_id 0x5C.., and the Hub countersigns the
    // serial-1 full refund for exactly that binding.
    let refund = f
        .bill(1, DEPOSIT, 0)
        .cosigned(&f.user.clone(), &f.hub.clone());

    // The `init` that actually reaches the chain carries a different id. Same
    // deposit, same addresses, same everything `PayableHAC` inspects.
    f.channel_id = [0xA1_u8; 16];
    f.init();
    let (ok, err) = f.fund_raw(DEPOSIT);
    assert!(
        ok,
        "funding must succeed - PayableHAC cannot see the swap: {err}"
    );
    assert_eq!(f.contract_balance(), DEPOSIT);

    let (got_out, log) = f.try_every_door(&refund);
    println!("PROBE 2a - refund bound to channel_id 5c.., chain opened a1..:\n{log}");
    assert!(!got_out);
    assert_eq!(
        f.contract_balance(),
        DEPOSIT,
        "TRAPPED by a channel_id swap"
    );
}

// ===========================================================================
// PROBE 2b - the same attack through `challenge_blocks`, the binding field the
// funding gate ignores most conspicuously.
// ===========================================================================
#[test]
fn probe_2b_refund_bound_to_a_different_challenge_window_is_worthless() {
    let mut f = Rig::deploy("p2b");
    let refund = f
        .bill(1, DEPOSIT, 0)
        .cosigned(&f.user.clone(), &f.hub.clone());
    f.challenge_blocks = 7;
    f.init();
    let (ok, err) = f.fund_raw(DEPOSIT);
    assert!(ok, "funding must succeed: {err}");
    let (got_out, log) = f.try_every_door(&refund);
    println!("PROBE 2b - refund says challenge_blocks=6, chain says 7:\n{log}");
    assert!(!got_out);
    assert_eq!(
        f.contract_balance(),
        DEPOSIT,
        "TRAPPED by a challenge_blocks swap"
    );
}

// ===========================================================================
// PROBE 2c - the one binding field `PayableHAC` DOES check, checked against the
// wrong thing.
//
// `amount != this.load("c_deposit_", from_addr)` compares the payment against
// the ON-CHAIN record, not against the signed binding. A record opened for a
// different deposit is funded exactly as happily, and the refund - whose
// commitment binds `total` - is void.
// ===========================================================================
#[test]
fn probe_2c_refund_bound_to_a_different_deposit_is_worthless() {
    let mut f = Rig::deploy("p2c");
    let refund = f
        .bill(1, DEPOSIT, 0)
        .cosigned(&f.user.clone(), &f.hub.clone());
    f.deposit = DEPOSIT - 1;
    f.init();
    let (ok, err) = f.fund_raw(DEPOSIT - 1);
    assert!(
        ok,
        "funding must succeed against the record's own figure: {err}"
    );
    assert_eq!(f.contract_balance(), DEPOSIT - 1);
    let (got_out, log) = f.try_every_door(&refund);
    println!("PROBE 2c - refund says total=1000000, chain says 999999:\n{log}");
    assert!(!got_out);
    assert_eq!(
        f.contract_balance(),
        DEPOSIT - 1,
        "TRAPPED by a one-zhu deposit swap"
    );
}

// ===========================================================================
// PROBE 3 - `init` puts NO upper bound on challenge_blocks.
//
// The refund here is perfect and matches the record exactly. The user's
// challenge is accepted. `finalize` then asserts
// `block_height() >= c_deadline_`, and `challenge` set c_deadline_ to
// `block_height() + c_challenge_`. Nothing anywhere bounds c_challenge_, and
// `PayableHAC` does not look at it, so the deposit lands behind an arbitration
// window that outlives the chain.
// ===========================================================================
#[test]
fn probe_3_unbounded_challenge_blocks_makes_finalize_unreachable_forever() {
    let mut f = Rig::deploy("p3");
    f.challenge_blocks = 1_000_000_000_000_000_000; // 1e18 blocks
    f.init();
    let (ok, err) = f.fund_raw(DEPOSIT);
    assert!(ok, "PayableHAC never looks at challenge_blocks: {err}");
    assert_eq!(f.contract_balance(), DEPOSIT);

    let refund = f
        .bill(1, DEPOSIT, 0)
        .cosigned(&f.user.clone(), &f.hub.clone());
    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();

    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("challenge", &refund),
        miner,
    );
    assert_eq!(f.status(), Value::U8(3), "the challenge itself is accepted");
    let deadline = f.deadline();
    let now = f.chain.height();
    println!(
        "PROBE 3: challenge accepted at height {now}; c_deadline_ = {deadline} \
         ({} blocks away)",
        deadline - now
    );

    let early = confirm_solo_call_rejected(
        &mut f.chain,
        &user,
        &contract,
        &format!("finalize({})", addr(&user).to_readable()),
        miner,
    );
    println!("PROBE 3: finalize refused: {early}");

    // A key's live lease tops out at storage_live_max_periods (30000) *
    // storage_period (100) = 3_000_000 blocks, plus a 300_000-block dormant
    // window (vm/src/rt/cap.rs:59-61). The deadline is 1e18.
    let max_reach: u128 = 3_000_000 + 300_000;
    assert!(
        (deadline - now) as u128 > max_reach,
        "the deadline is beyond anything the record can survive to see"
    );

    // Wait as long as this harness can and confirm nothing changes.
    f.chain.set_height(now + 150_000);
    let (out, log) = f.try_every_door(&refund);
    println!("PROBE 3 - 150000 blocks later:\n{log}");
    assert!(!out);
    assert_eq!(
        f.contract_balance(),
        DEPOSIT,
        "TRAPPED: one unbounded `init` parameter locks the deposit permanently"
    );
}

// ===========================================================================
// PROBE 4 - a countersigned refund that is valid, but for less than was funded.
//
// The wallet's `validate_shape` pins left_balance == deposit and
// hub_balance == 0. The contract pins nothing of the sort: `verify_bill` only
// requires left_balance + hub_balance == c_total_. A Hub that countersigns a
// 40/60 split at serial 1 has taken 600_000 for providing nothing, and the
// user's own challenge + finalize is what hands it over.
// ===========================================================================
#[test]
fn probe_4_a_smaller_refund_is_accepted_by_the_chain_and_the_hub_keeps_the_rest() {
    let mut f = Rig::deploy("p4");
    f.init();
    let (ok, err) = f.fund_raw(DEPOSIT);
    assert!(ok, "funding: {err}");

    let short = f
        .bill(1, 400_000, 600_000)
        .cosigned(&f.user.clone(), &f.hub.clone());

    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();
    let hub_a = addr(&f.hub);
    let stranger = f.stranger.clone();

    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("challenge", &short),
        miner,
    );
    let deadline = f.deadline();
    f.chain
        .confirm_empty_formal_blocks_to_height(miner, deadline)
        .unwrap();
    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &format!("finalize({})", addr(&user).to_readable()),
        miner,
    );

    assert_eq!(
        f.chain
            .storage(&contract, &Value::bytes(b"g_left_claimable".to_vec())),
        Value::U64(400_000)
    );
    assert_eq!(
        f.chain
            .storage(&contract, &Value::bytes(b"g_hub_claimable".to_vec())),
        Value::U64(600_000)
    );

    let (over, over_err) = try_payout(&mut f.chain, &user, &contract, addr(&user), DEPOSIT, miner);
    assert!(!over, "over-claim must be refused");
    println!("PROBE 4: user claiming the full deposit refused: {over_err}");

    let before = f.user_balance();
    let claim = submit_payout(&mut f.chain, &stranger, &contract, addr(&user), 400_000);
    f.chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&claim);
    assert_eq!(f.user_balance(), before + 400_000);

    let hub_before = f.chain.balance(&hub_a).to_zhu_u64().unwrap();
    let hub_claim = submit_payout(&mut f.chain, &stranger, &contract, hub_a, 600_000);
    f.chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&hub_claim);
    assert_eq!(
        f.chain.balance(&hub_a).to_zhu_u64().unwrap(),
        hub_before + 600_000
    );
    assert_eq!(f.contract_balance(), 0);
    println!(
        "PROBE 4: user funded {DEPOSIT} and recovered 400000. \
         The Hub took 600000 for one off-chain signature."
    );
}

// ===========================================================================
// PROBE 5 - the refund never expires, but the RECORD it points at does, before
// the funding transaction lands.
//
// `init` creates every channel key with `storage_new(..., 100)`: 10000 live
// blocks and - because `snew` hardcodes the recovery credit to zero
// (vm/src/field/state.rs:243) - no dormant window at all. Nothing fortifies
// those keys until `PayableHAC` runs. The fix extended the lease of a FUNDED
// record and left the PRE-FUNDING record exactly as it was.
// ===========================================================================
#[test]
fn probe_5_the_pre_funding_record_still_has_no_recovery_buffer_and_dies() {
    let mut f = Rig::deploy("p5");
    f.init();
    let refund = f
        .bill(1, DEPOSIT, 0)
        .cosigned(&f.user.clone(), &f.hub.clone());

    let (live, recover) = f
        .channel_lease("c_status_")
        .expect("record exists right after init");
    println!("PROBE 5: straight after init, c_status_ live={live} recover={recover}");
    assert_eq!(
        recover, 0,
        "an UNFUNDED record still has zero dormant window: `init` was never fortified"
    );

    // The payment is delayed past the live lease. Nothing malicious is needed:
    // a fee-market backlog, an offline signer, or a human weekend.
    f.chain.set_height(f.chain.height() + live + 1);
    assert!(
        f.channel_lease("c_status_").is_none(),
        "the pre-funding record is DELETED outright, not made dormant"
    );

    let (ok, err) = f.fund_raw(DEPOSIT);
    println!("PROBE 5: funding after the pre-funding lease lapsed -> ok={ok} {err}");
    assert!(!ok, "funding a dead record must not swallow the deposit");
    assert_eq!(f.contract_balance(), 0, "the deposit stayed with the user");

    // Not out of pocket, but the channel is unrecoverable without the Hub:
    // `init` asserts BOTH signatures and the Hub is gone.
    let solo_reinit = format!(
        "init(0x{}, 0, {}, {}, {}, 100)",
        hex::encode(f.channel_id),
        addr(&f.user).to_readable(),
        DEPOSIT,
        f.challenge_blocks,
    );
    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();
    let solo = confirm_solo_call_rejected(&mut f.chain, &user, &contract, &solo_reinit, miner);
    println!("PROBE 5: user re-opening alone refused: {solo}");
    let _ = refund;
}

// ===========================================================================
// PROBE 6 - the diligent user who renews their CHANNEL and not the REGISTRY.
//
// This is the expired-arbitration state the contract fix did not consider.
// `fortify_channel_storage` fortifies the twelve channel keys at funding. It
// does NOT fortify the six registry globals - `PayableHAC` only *reads* their
// reach. The globals were minted at `Construct`, so they are always OLDER than
// any channel and they die first. `renew_channel` reads no global, so it keeps
// succeeding, forever, while the registry underneath it is destroyed.
// ===========================================================================
#[test]
fn probe_6_renewing_the_channel_forever_does_not_save_it_from_the_dead_registry() {
    let mut f = Rig::deploy("p6");
    f.init();
    let (ok, err) = f.fund_raw(DEPOSIT);
    assert!(ok, "funding: {err}");
    let refund = f
        .bill(1, DEPOSIT, 0)
        .cosigned(&f.user.clone(), &f.hub.clone());

    let (cl, cr) = f.channel_lease("c_status_").unwrap();
    let (gl, gr) = f.global_lease("g_hub").unwrap();
    println!(
        "PROBE 6: at funding, channel c_status_ live={cl} recover={cr} (reach {})",
        cl + cr
    );
    println!(
        "PROBE 6: at funding, global  g_hub     live={gl} recover={gr} (reach {})",
        gl + gr
    );

    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();
    let renew = format!("renew_channel({}, 150)", addr(&user).to_readable());

    // A model citizen: they renew their channel record on schedule for as long
    // as it takes. Nobody told them the registry globals are a separate lease
    // with a separate, EARLIER deadline.
    let mut round = 0;
    while f.global_lease("g_hub").is_some() {
        round += 1;
        assert!(round < 40, "g_hub should have died long before now");
        confirm_solo_call(&mut f.chain, &user, &contract, &renew, miner);
        f.chain.set_height(f.chain.height() + 14_000);
    }
    let (cl2, cr2) = f
        .channel_lease("c_status_")
        .expect("the diligent user's own record is in perfect health");
    println!(
        "PROBE 6: after {round} channel renewals the user's record is live={cl2} \
         recover={cr2}, and g_hub is GONE"
    );
    assert!(cl2 > 0, "the channel record is still active");

    let (out, log) = f.try_every_door(&refund);
    println!("PROBE 6 - channel record perfect, registry destroyed:\n{log}");
    assert!(!out, "unexpected escape");
    assert_eq!(
        f.contract_balance(),
        DEPOSIT,
        "TRAPPED: the registry globals expire first and renew_registry cannot \
         resurrect a deleted key"
    );
}

// ===========================================================================
// PROBE 7 - where exactly is the new cliff, and is it still a cliff?
//
// Nobody renews anything. The fix buys 55000 recovery blocks at funding on top
// of the live lease. Past the END of that dormant window every key is deleted
// and `renew_channel` answers StorageKeyNotFind - the same total destruction as
// before the fix, just later.
// ===========================================================================
#[test]
fn probe_7_the_recovery_window_is_finite_and_past_it_the_money_is_destroyed() {
    let mut f = Rig::deploy("p7");
    f.init();
    let (ok, err) = f.fund_raw(DEPOSIT);
    assert!(ok, "funding: {err}");
    let refund = f
        .bill(1, DEPOSIT, 0)
        .cosigned(&f.user.clone(), &f.hub.clone());

    let (live, recover) = f.channel_lease("c_status_").unwrap();
    let reach = live + recover;
    println!("PROBE 7: at funding, reach = {live} live + {recover} recover = {reach} blocks");

    f.chain.set_height(f.chain.height() + reach + 1);
    assert!(
        f.channel_lease("c_status_").is_none(),
        "past the dormant window the record is deleted, not dormant"
    );
    for g in [
        "g_network",
        "g_hub",
        "g_locked",
        "g_left_claimable",
        "g_hub_claimable",
        "g_open_count",
    ] {
        assert!(f.global_lease(g).is_none(), "{g} is gone too");
    }

    let (out, log) = f.try_every_door(&refund);
    println!("PROBE 7 - {reach} blocks after custody began, nobody renewed:\n{log}");
    assert!(!out);
    assert_eq!(
        f.contract_balance(),
        DEPOSIT,
        "MONEY DESTROYED: the deposit sits in the contract account and no key describes it"
    );
    assert_eq!(
        f.status(),
        Value::Nil,
        "the arbitration record does not exist"
    );
}

// ===========================================================================
// PROBE 8 - how late can a deposit arrive, and do the two new custody refusals
// ever actually fire?
//
// `PayableHAC` now throws HPAY_LEASE_TOO_SHORT / HPAY_REGISTRY_LEASE_TOO_SHORT
// below MIN_FUNDED_REACH_BLOCKS (50000). Measure whether either is reachable.
// ===========================================================================
#[test]
fn probe_8_the_new_custody_refusals_never_fire() {
    // Sequential rigs: one MemChain at a time (global test mutex).
    fn run(delay: u64) -> (bool, String, Option<(u64, u64)>) {
        let mut f = Rig::deploy(&format!("p8-{delay}"));
        f.init();
        if delay > 0 {
            f.chain.set_height(f.chain.height() + delay);
        }
        let g = f.global_lease("g_hub");
        let (ok, err) = f.fund_raw(DEPOSIT);
        (ok, err, g)
    }

    for delay in [0_u64, 5_000, 9_000, 9_999, 10_001, 20_000, 70_000] {
        let (ok, err, g_hub) = run(delay);
        println!("PROBE 8: fund {delay} blocks after init -> ok={ok} g_hub_lease={g_hub:?} {err}");
        assert!(
            !err.contains("HPAY_LEASE_TOO_SHORT") && !err.contains("HPAY_REGISTRY_LEASE_TOO_SHORT"),
            "a reach refusal fired at delay {delay}: {err}"
        );
    }
    println!(
        "PROBE 8: neither reach refusal is reachable. While g_hub is readable its \
         recovery buffer is still untouched (>= 55000), so reach can never be under \
         50000; and once it is not readable, `init` cannot run at all. The two new \
         throws in PayableHAC are dead code."
    );
}

// ===========================================================================
// PROBE 9 - the record lapses while the channel is CHALLENGING.
//
// The user did everything right and on time: countersigned bill, challenge
// filed, waiting out the window. `init` lets the Hub co-sign an arbitration
// window longer than any lease the contract will ever buy, so the record can be
// destroyed mid-arbitration with the coin inside and the status stuck at
// CHALLENGING.
// ===========================================================================
#[test]
fn probe_9_a_lease_that_lapses_mid_arbitration() {
    let mut f = Rig::deploy("p9");
    f.challenge_blocks = 500_000; // long, but perfectly legal
    f.init();
    let (ok, err) = f.fund_raw(DEPOSIT);
    assert!(ok, "funding: {err}");
    let refund = f
        .bill(1, DEPOSIT, 0)
        .cosigned(&f.user.clone(), &f.hub.clone());

    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();
    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("challenge", &refund),
        miner,
    );
    assert_eq!(f.status(), Value::U8(3));
    let deadline = f.deadline();
    let (live, recover) = f.channel_lease("c_status_").unwrap();
    println!(
        "PROBE 9: CHALLENGING, deadline {} blocks away, record reach = {} blocks",
        deadline - f.chain.height(),
        live + recover
    );
    assert!(
        deadline - f.chain.height() > live + recover,
        "the arbitration window outlives the record that records it"
    );

    f.chain.set_height(f.chain.height() + live + recover + 1);
    assert!(
        f.channel_lease("c_status_").is_none(),
        "the record was destroyed mid-arbitration"
    );
    let (out, log) = f.try_every_door(&refund);
    println!("PROBE 9 - lease lapsed mid-arbitration:\n{log}");
    assert!(!out);
    assert_eq!(
        f.contract_balance(),
        DEPOSIT,
        "MONEY DESTROYED mid-arbitration"
    );
}
