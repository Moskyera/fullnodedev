//! HOSTILE HUB. Can the user get their money out with ZERO Hub cooperation?
//!
//! Every transaction in these tests after the channel is funded is paid for and
//! signed by the USER alone (or by an unrelated third party). The Hub account
//! signs nothing after funding. Anything the user recovers here, they recovered
//! unaided.
//!
//! Driven against `testkit::sim::memchain::MemChain`: real `TransactionType3`
//! bytes, real Type3 signature verification, real `BlockV1` execution, real HVM
//! contract state, real Action 14 `HacFromToTrs` payouts. Not a mock.

use field::{AddrOrPtr, Address, Amount, BytesW2, Field, Hash, Serialize, Sign, Uint4};
use protocol::action::{HacFromToTrs, HacToTrs};
use sys::Account;
use testkit::sim::memchain::{MemChain, TxOutput};
use vm::ContractAddress;
use vm::value::Value;

const DOMAIN: &[u8] = b"HPAY/HVM-CHANNEL-REGISTRY/V2";
const CHALLENGE_BLOCKS: u64 = 6;
const CONTRACT_SOURCE: &str = include_str!("../contracts/hpay_channel_registry_v2.fitsh");
const DEPOSIT: u64 = 1_000_000;

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
    #[allow(clippy::too_many_arguments)]
    fn unsigned(
        network: [u8; 32],
        contract: Address,
        channel_id: [u8; 16],
        reuse: u32,
        left: Address,
        hub: Address,
        total: u64,
        serial: u64,
        left_balance: u64,
        hub_balance: u64,
    ) -> Self {
        Self {
            network,
            contract,
            channel_id,
            reuse,
            left,
            hub,
            total,
            challenge_blocks: CHALLENGE_BLOCKS,
            serial,
            left_balance,
            hub_balance,
            left_sign: Sign::new(),
            hub_sign: Sign::new(),
        }
    }

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

    /// A bill both parties signed - the normal co-signed channel state.
    fn cosigned(mut self, left: &Account, hub: &Account) -> Self {
        let commitment = self.commitment();
        self.left_sign = Sign::create_by(left, &commitment);
        self.hub_sign = Sign::create_by(hub, &commitment);
        self
    }

    /// A bill the USER signed twice: what a stranded user can actually
    /// manufacture on their own when the Hub never countersigned anything.
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

/// Submit a registry call signed and paid for by ONE account only.
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

/// Run a call that MUST be rejected, and return the recorded error text.
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
        "HOSTILE-HUB ESCAPE UNEXPECTEDLY SUCCEEDED: {call}"
    );
    format!("{receipt:?}")
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

struct Hostile {
    chain: MemChain,
    hub: Account,
    user: Account,
    stranger: Account,
    miner: Address,
    network: [u8; 32],
    contract: ContractAddress,
    channel_id: [u8; 16],
}

impl Hostile {
    /// Deploy the registry and open + fund one channel. Opening a channel is
    /// cooperative by nature (`init` asserts both signatures), so the Hub does
    /// cooperate here and NOWHERE AFTER THIS POINT.
    fn open(seed: &str) -> Self {
        Self::open_funding_after(seed, 0)
    }

    /// Same, but the user lets `delay` blocks pass between `init` and paying
    /// the deposit in - so the arbitration record is already part-worn at the
    /// moment the contract takes custody.
    fn open_funding_after(seed: &str, delay: u64) -> Self {
        let mut chain = MemChain::new();
        chain.set_height(protocol::upgrade::ONLINE_OPEN_HEIGHT);
        let hub = Account::create_by(&format!("hostile-hub-{seed}")).unwrap();
        let user = Account::create_by(&format!("hostile-user-{seed}")).unwrap();
        let stranger = Account::create_by(&format!("hostile-stranger-{seed}")).unwrap();
        let miner = addr(&Account::create_by(&format!("hostile-miner-{seed}")).unwrap());
        let hub_a = addr(&hub);
        let user_a = addr(&user);
        for address in [hub_a, user_a, addr(&stranger)] {
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
            .expect("build channel init");
        chain
            .confirm_formal_block(miner)
            .unwrap()
            .expect_success(&init_hash);

        if delay > 0 {
            chain.set_height(chain.height() + delay);
        }

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
            .expect("build channel funding");
        chain
            .confirm_formal_block(miner)
            .unwrap()
            .expect_success(&fund_hash);

        assert_eq!(
            chain.storage(&contract, &channel_key("c_status_", &user_a)),
            Value::U8(2),
            "channel must be OPEN after funding"
        );
        assert_eq!(
            chain.balance(&contract.to_addr()).to_zhu_u64(),
            Ok(DEPOSIT),
            "the coin must be inside the contract"
        );

        Self {
            chain,
            hub,
            user,
            stranger,
            miner,
            network,
            contract,
            channel_id,
        }
    }

    fn bill(&self, serial: u64, left_balance: u64, hub_balance: u64) -> Bill {
        Bill::unsigned(
            self.network,
            self.contract.to_addr(),
            self.channel_id,
            0,
            addr(&self.user),
            addr(&self.hub),
            DEPOSIT,
            serial,
            left_balance,
            hub_balance,
        )
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

    /// `(live_blocks, recover_blocks)` left on one storage key, or `None` once
    /// the entry has been destroyed outright.
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
}

// ---------------------------------------------------------------------------
// SCENARIO A - the Hub goes silent the instant the channel is funded, having
// never countersigned a single bill. The user holds nothing but their deposit
// receipt.
// ---------------------------------------------------------------------------
#[test]
fn scenario_a_hub_silent_with_no_countersigned_bill_traps_the_user() {
    let mut f = Hostile::open("a");
    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();

    assert_eq!(
        f.chain
            .storage(&contract, &channel_key("c_serial_", &addr(&user))),
        Value::U64(0),
        "no bill has ever been agreed"
    );

    // Escape 1: finalize straight away.
    let e1 = confirm_solo_call_rejected(
        &mut f.chain,
        &user,
        &contract,
        &format!("finalize({})", addr(&user).to_readable()),
        miner,
    );

    // Escape 2: challenge with a bill the user signed on both lines.
    let forged = f.bill(1, DEPOSIT, 0).self_signed_only(&user);
    let e2 = confirm_solo_call_rejected(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("challenge", &forged),
        miner,
    );

    // Escape 3: cooperative_close with the same self-made bill.
    let e3 = confirm_solo_call_rejected(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("cooperative_close", &forged),
        miner,
    );

    // Escape 4: reach past the contract and pull the coin out with Action 14.
    let raw = submit_payout(&mut f.chain, &user, &contract, addr(&user), DEPOSIT);
    let block = f
        .chain
        .confirm_formal_block_observing_failures(miner)
        .expect("execute raw payout attempt");
    let e4 = block.receipt(&raw).expect("raw payout receipt");
    assert!(
        e4.is_error(),
        "RAW ACTION 14 DRAINED A NON-FINAL CHANNEL - the contract is not the only door"
    );

    // Escape 5: re-open with reuse to reset the channel and get the coin back.
    let reinit = format!(
        "init(0x{}, 1, {}, {}, {}, 100)",
        hex::encode(f.channel_id),
        addr(&user).to_readable(),
        DEPOSIT,
        CHALLENGE_BLOCKS,
    );
    let e5 = confirm_solo_call_rejected(&mut f.chain, &user, &contract, &reinit, miner);

    assert_eq!(f.status(), Value::U8(2), "channel is still OPEN");
    assert_eq!(
        f.contract_balance(),
        DEPOSIT,
        "TRAPPED: the whole deposit is still inside the contract"
    );
    println!(
        "SCENARIO A: user is TRAPPED. finalize={e1}\nchallenge={e2}\nclose={e3}\naction14={e4:?}\nreinit={e5}"
    );
}

// ---------------------------------------------------------------------------
// SCENARIO B - identical hostility, EXCEPT the user made the Hub countersign a
// serial-1 full-refund bill before parting with the money. Hub then vanishes
// forever and signs nothing.
// ---------------------------------------------------------------------------
#[test]
fn scenario_b_refund_bill_lets_the_user_exit_with_zero_hub_cooperation() {
    let mut f = Hostile::open("b");
    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();
    let hub = f.hub.clone();
    let stranger = f.stranger.clone();

    // The ONLY Hub signature in this whole scenario: an off-chain signature on
    // a refund bill. The Hub broadcasts nothing and signs no transaction.
    let refund = f.bill(1, DEPOSIT, 0).cosigned(&user, &hub);

    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("challenge", &refund),
        miner,
    );
    assert_eq!(f.status(), Value::U8(3), "channel must be CHALLENGING");

    // Finalize is refused until the arbitration window has actually elapsed.
    let early = confirm_solo_call_rejected(
        &mut f.chain,
        &user,
        &contract,
        &format!("finalize({})", addr(&user).to_readable()),
        miner,
    );
    println!("SCENARIO B: early finalize correctly refused: {early}");

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
    assert_eq!(f.status(), Value::U8(4), "channel must be FINAL");
    assert_eq!(
        f.chain
            .storage(&contract, &Value::bytes(b"g_left_claimable".to_vec())),
        Value::U64(DEPOSIT)
    );

    // Fee-neutral measurement: a stranger pays the claim fee so the user's
    // delta is exactly the coin the contract released.
    let before = f.user_balance();
    let claim = submit_payout(&mut f.chain, &stranger, &contract, addr(&user), DEPOSIT);
    f.chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&claim);

    assert_eq!(f.contract_balance(), 0, "the contract is empty");
    assert_eq!(
        f.user_balance(),
        before + DEPOSIT,
        "user must recover the entire deposit"
    );
    println!("SCENARIO B: user GOT OUT unaided with the full {DEPOSIT} zhu deposit.");
}

// ---------------------------------------------------------------------------
// SCENARIO C - the Hub challenges with a STALE bill (one where it keeps more)
// and then refuses to respond. The user is online.
// ---------------------------------------------------------------------------
#[test]
fn scenario_c_stale_challenge_then_hub_refuses_to_respond() {
    let mut f = Hostile::open("c");
    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();
    let hub = f.hub.clone();

    // Channel history: user pays 400_000, Hub then refunds 300_000.
    let _s1 = f.bill(1, DEPOSIT, 0).cosigned(&user, &hub);
    let s2 = f.bill(2, 600_000, 400_000).cosigned(&user, &hub);
    let s3 = f.bill(3, 900_000, 100_000).cosigned(&user, &hub);

    // Hostile Hub publishes serial 2, the state where it keeps 400_000.
    // The Hub signs and pays for this one transaction; everything after is the
    // user alone.
    let hostile = submit_solo_call(&mut f.chain, &hub, &contract, &bill_call("challenge", &s2));
    f.chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&hostile);
    assert_eq!(f.status(), Value::U8(3));

    // User responds with the true latest state. Hub does nothing.
    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("respond", &s3),
        miner,
    );
    assert_eq!(
        f.chain
            .storage(&contract, &channel_key("c_left_balance_", &addr(&user))),
        Value::U64(900_000),
        "the user's later bill must overwrite the Hub's stale one"
    );

    // Hub tries to re-publish the stale bill to undo the response.
    let replay = confirm_solo_call_rejected(
        &mut f.chain,
        &hub,
        &contract,
        &bill_call("respond", &s2),
        miner,
    );
    println!("SCENARIO C: stale replay refused: {replay}");

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

    let before = f.user_balance();
    let claim = submit_payout(&mut f.chain, &user, &contract, addr(&user), 900_000);
    f.chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&claim);
    assert!(f.user_balance() > before);
    println!("SCENARIO C: user GOT OUT with the full 900_000.");
}

// ---------------------------------------------------------------------------
// SCENARIO D - the Hub challenges with a stale bill at the exact moment the
// user is offline, and the arbitration window expires unanswered.
// ---------------------------------------------------------------------------
#[test]
fn scenario_d_stale_challenge_while_user_offline_wins_for_the_hub() {
    let mut f = Hostile::open("d");
    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();
    let hub = f.hub.clone();
    let stranger = f.stranger.clone();

    let s2 = f.bill(2, 600_000, 400_000).cosigned(&user, &hub);
    let s3 = f.bill(3, 900_000, 100_000).cosigned(&user, &hub);

    let hostile = submit_solo_call(&mut f.chain, &hub, &contract, &bill_call("challenge", &s2));
    f.chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&hostile);

    // The user is offline. Nobody responds. The window runs out.
    let deadline = f.deadline();
    f.chain
        .confirm_empty_formal_blocks_to_height(miner, deadline)
        .unwrap();

    // The user comes back one block too late holding the true latest bill.
    let too_late = confirm_solo_call_rejected(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("respond", &s3),
        miner,
    );
    println!("SCENARIO D: late response refused: {too_late}");

    confirm_solo_call(
        &mut f.chain,
        &stranger,
        &contract,
        &format!("finalize({})", addr(&user).to_readable()),
        miner,
    );
    assert_eq!(
        f.chain
            .storage(&contract, &Value::bytes(b"g_left_claimable".to_vec())),
        Value::U64(600_000),
        "the stale bill settled: the user lost 300_000 to the Hub"
    );
    assert_eq!(
        f.chain
            .storage(&contract, &Value::bytes(b"g_hub_claimable".to_vec())),
        Value::U64(400_000)
    );

    // The user can still take the 600_000 the stale bill left them.
    let before = f.user_balance();
    let claim = submit_payout(&mut f.chain, &stranger, &contract, addr(&user), 600_000);
    f.chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&claim);
    assert_eq!(f.user_balance(), before + 600_000);
    println!("SCENARIO D: user recovered only 600_000 of 900_000. 300_000 STOLEN.");
}

// ---------------------------------------------------------------------------
// SCENARIO E - the Hub claims first, and tries to redirect the user's payout.
// ---------------------------------------------------------------------------
#[test]
fn scenario_e_hub_cannot_redirect_or_front_run_the_user_payout() {
    let mut f = Hostile::open("e");
    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();
    let hub = f.hub.clone();
    let stranger = f.stranger.clone();

    let s3 = f.bill(3, 900_000, 100_000).cosigned(&user, &hub);
    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("challenge", &s3),
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

    // Hub tries to draw the user's 900_000 to its own address.
    let steal = submit_payout(&mut f.chain, &hub, &contract, addr(&hub), 900_000);
    let block = f
        .chain
        .confirm_formal_block_observing_failures(miner)
        .expect("execute hub steal attempt");
    assert!(
        block.receipt(&steal).expect("steal receipt").is_error(),
        "HUB DRAINED THE USER'S BALANCE"
    );

    // Hub tries to over-claim its own 100_000 slice.
    let over = submit_payout(&mut f.chain, &hub, &contract, addr(&hub), 900_000 + 100_000);
    let block = f
        .chain
        .confirm_formal_block_observing_failures(miner)
        .expect("execute hub overclaim attempt");
    assert!(
        block.receipt(&over).expect("overclaim receipt").is_error(),
        "HUB OVER-CLAIMED THE AGGREGATE"
    );

    // A stranger pays the fee and the coin still lands on the USER.
    let user_before = f.user_balance();
    let claim = submit_payout(&mut f.chain, &stranger, &contract, addr(&user), 900_000);
    f.chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&claim);
    assert_eq!(
        f.user_balance(),
        user_before + 900_000,
        "the payout must land on the user, whoever pays the fee"
    );
    println!("SCENARIO E: Hub could not redirect or over-claim; a stranger completed the exit.");
}

// ---------------------------------------------------------------------------
// SCENARIO F - the Hub abandons the channel and stops paying storage rent.
// Can the user keep the arbitration record alive on their own?
// ---------------------------------------------------------------------------
#[test]
fn scenario_f_user_can_renew_the_lease_alone() {
    let mut f = Hostile::open("f");
    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();
    let hub = f.hub.clone();
    let stranger = f.stranger.clone();

    // MAX_RENT_STEP used to advertise 5000, which no transaction could pay the
    // storage gas for, so the ceiling was a lie and asking for it produced
    // OutOfGas. It now advertises a step that fits; see scenario L.
    let over = confirm_solo_call_rejected(
        &mut f.chain,
        &user,
        &contract,
        &format!("renew_channel({}, 500)", addr(&user).to_readable()),
        miner,
    );
    println!("SCENARIO F: renew_channel(500) is refused above the honest ceiling: {over}");

    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &format!("renew_channel({}, 100)", addr(&user).to_readable()),
        miner,
    );
    confirm_solo_call(&mut f.chain, &user, &contract, "renew_registry(100)", miner);

    // And the exit still works afterwards.
    let s3 = f.bill(3, 900_000, 100_000).cosigned(&user, &hub);
    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("challenge", &s3),
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
    let before = f.user_balance();
    let claim = submit_payout(&mut f.chain, &stranger, &contract, addr(&user), 900_000);
    f.chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&claim);
    assert_eq!(f.user_balance(), before + 900_000);
    println!("SCENARIO F: user renewed both leases alone and still exited in full.");
}

// ---------------------------------------------------------------------------
// SCENARIO I - the Hub abandons the channel and NOBODY renews. The arbitration
// record is allowed to lapse. What happens to the user's coin?
// ---------------------------------------------------------------------------
#[test]
fn scenario_i_abandoned_lease_decides_who_keeps_the_coin() {
    let mut f = Hostile::open("i");
    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();
    let hub = f.hub.clone();

    let s3 = f.bill(3, 900_000, 100_000).cosigned(&user, &hub);

    // How long does the user actually have before the record dies?
    let h = f.chain.height();
    let debug = vm::VMStateRead::wrap(f.chain.state())
        .debug_storage_get(
            &vm::rt::GasExtra::new(h),
            &vm::rt::SpaceCap::new(h),
            h,
            &contract.to_addr(),
            &channel_key("c_status_", &addr(&user)),
        )
        .unwrap()
        .expect("c_status_ must exist at open");
    println!(
        "SCENARIO I: at open, c_status_ live_blocks={} recover_blocks={} active={} recoverable={}",
        debug.live_blocks, debug.recover_blocks, debug.active, debug.recoverable
    );

    // Nobody renews anything for a very long time.
    f.chain.set_height(f.chain.height().saturating_add(200_000));

    let lapsed = submit_solo_call(&mut f.chain, &user, &contract, &bill_call("challenge", &s3));
    let block = f
        .chain
        .confirm_formal_block_observing_failures(miner)
        .expect("execute post-lapse challenge");
    let receipt = block.receipt(&lapsed).expect("post-lapse receipt");
    println!("SCENARIO I: challenge after a fully lapsed lease -> {receipt:?}");
    println!(
        "SCENARIO I: contract still holds {} zhu; channel status = {:?}",
        f.contract_balance(),
        f.status()
    );
}

// ---------------------------------------------------------------------------
// SCENARIO J - TRAP 2. The user holds a perfectly good Hub-countersigned bill.
// They simply do not come back for a while. Nobody renews the storage lease.
//
// This is the money-destruction test: it asserts that after the lease runs out
// the deposit is still sitting in the contract account and that EVERY door to
// it is nailed shut - challenge, finalize, renew, and the raw Action 14 claim.
// ---------------------------------------------------------------------------
#[test]
fn scenario_j_lapsed_lease_must_not_destroy_the_users_money() {
    let mut f = Hostile::open("j");
    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();
    let hub = f.hub.clone();
    let stranger = f.stranger.clone();

    // A real, fully countersigned exit ticket. Nothing is wrong with the user's
    // paperwork; the only thing they did was go away.
    let s3 = f.bill(3, 900_000, 100_000).cosigned(&user, &hub);

    let (live, recover) = f
        .channel_lease("c_status_")
        .expect("c_status_ must exist at open");
    println!("SCENARIO J: at open c_status_ live={live} recover={recover}");
    assert!(
        recover > 0,
        "MONEY DESTRUCTION ON A TIMER: the funded channel record has no recovery \
         buffer at all, so when the live lease runs out it is deleted outright \
         (live={live}, recover={recover})"
    );

    // The user sleeps clean past the end of the live lease.
    f.chain.set_height(f.chain.height() + live + 1);
    assert_eq!(
        f.contract_balance(),
        DEPOSIT,
        "the deposit is still inside the contract after the live lease ended"
    );
    let (live_after, recover_after) = f
        .channel_lease("c_status_")
        .expect("a funded record must go DORMANT, never be deleted with coin inside");
    assert_eq!(live_after, 0, "the live lease really has run out");
    assert!(
        recover_after > 0,
        "the record must still be restorable, not destroyed"
    );
    println!(
        "SCENARIO J: past the live cliff, c_status_ is dormant with {recover_after} recover blocks left"
    );

    // A dormant record is not readable, so the ordinary exit is refused...
    let dormant = confirm_solo_call_rejected(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("challenge", &s3),
        miner,
    );
    println!("SCENARIO J: challenge on a dormant record refused: {dormant}");
    for prefix in [
        "c_status_",
        "c_id_",
        "c_reuse_",
        "c_deposit_",
        "c_paid_",
        "c_total_",
        "c_serial_",
        "c_left_balance_",
        "c_hub_balance_",
        "c_challenge_",
        "c_deadline_",
        "c_left_claimed_",
    ] {
        let (l, r) = f
            .channel_lease(prefix)
            .unwrap_or_else(|| panic!("{prefix} was DESTROYED with the deposit inside"));
        assert!(r > 0, "{prefix} must be dormant, not destroyed (live={l})");
    }
    // The registry's own keys matter just as much: PermitHAC reads `g_hub`, so
    // if the globals are destroyed every channel's payout dies with them.
    for global in [
        "g_network",
        "g_hub",
        "g_locked",
        "g_left_claimable",
        "g_hub_claimable",
        "g_open_count",
    ] {
        let (l, r) = f
            .lease(&Value::bytes(global.as_bytes().to_vec()))
            .unwrap_or_else(|| panic!("{global} was DESTROYED while the registry held coin"));
        assert!(r > 0, "{global} must be dormant, not destroyed (live={l})");
    }

    // A dormant record must be restorable by the USER, alone, with no Hub and
    // no bill. Both calls are permissionless and neither reads a lapsed key, so
    // neither depends on anything the user cannot do for themselves.
    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &format!("renew_channel({}, 100)", addr(&user).to_readable()),
        miner,
    );
    confirm_solo_call(&mut f.chain, &user, &contract, "renew_registry(100)", miner);
    let (live_back, _) = f.channel_lease("c_status_").expect("record is back");
    assert!(live_back > 0, "the record must be active again");

    // ...and once restored the ordinary exit must work end to end.
    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("challenge", &s3),
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
    let before = f.user_balance();
    let claim = submit_payout(&mut f.chain, &stranger, &contract, addr(&user), 900_000);
    f.chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&claim);
    assert_eq!(
        f.user_balance(),
        before + 900_000,
        "the user must still be able to reach their money after the lease lapsed"
    );
    println!("SCENARIO J: lapsed lease was dormant, not fatal; user exited in full.");
}

// ---------------------------------------------------------------------------
// SCENARIO K - the diligent user. They renew on schedule, forever. The renewal
// path must not quietly saturate and then start refusing.
//
// A key's recovery buffer is capped at 300000 blocks and `storage_recv` is a
// hard error past that, so a renewal that tops recovery up unconditionally
// eventually aborts its own transaction - and once every renewal reverts, the
// live lease drains and the record dies exactly like an abandoned one. Renewing
// diligently must never be the thing that kills the channel.
// ---------------------------------------------------------------------------
#[test]
fn scenario_k_renewal_must_not_saturate_itself_into_refusing() {
    let mut f = Hostile::open("k");
    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();
    let hub = f.hub.clone();
    let stranger = f.stranger.clone();

    let renew = format!("renew_channel({}, 150)", addr(&user).to_readable());
    let mut peak_recover = 0;
    for round in 0..40 {
        confirm_solo_call(&mut f.chain, &user, &contract, &renew, miner);
        confirm_solo_call(&mut f.chain, &user, &contract, "renew_registry(150)", miner);
        let (_, recover) = f
            .channel_lease("c_status_")
            .unwrap_or_else(|| panic!("record vanished during renewal round {round}"));
        peak_recover = recover;
    }
    assert!(
        peak_recover >= 299_000,
        "the test must actually push the recovery buffer to its 300000-block \
         ceiling, otherwise it never exercises saturation (got {peak_recover})"
    );
    println!(
        "SCENARIO K: 40 renewals later the recovery buffer sits at {peak_recover} blocks and renewal still works."
    );

    // And a saturated, heavily renewed channel still exits.
    let s3 = f.bill(3, 900_000, 100_000).cosigned(&user, &hub);
    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("challenge", &s3),
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
    let before = f.user_balance();
    let claim = submit_payout(&mut f.chain, &stranger, &contract, addr(&user), 900_000);
    f.chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&claim);
    assert_eq!(f.user_balance(), before + 900_000);
}

// ---------------------------------------------------------------------------
// SCENARIO L - the advertised renewal step has to be one a transaction can
// actually pay for. A ceiling nobody can reach is worse than a lower honest
// one: the user aims for it, gets OutOfGas, and cannot tell a gas problem from
// a broken contract.
// ---------------------------------------------------------------------------
#[test]
fn scenario_l_advertised_rent_step_must_be_reachable_in_one_transaction() {
    let mut f = Hostile::open("l");
    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();

    let max_step: u64 = 150;
    let (live_before, _) = f.channel_lease("c_status_").unwrap();
    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &format!("renew_channel({}, {max_step})", addr(&user).to_readable()),
        miner,
    );
    let (live_after, _) = f.channel_lease("c_status_").unwrap();
    assert_eq!(
        live_after,
        live_before + max_step * 100 - 1,
        "MAX_RENT_STEP must buy exactly what it says in ONE transaction"
    );
    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &format!("renew_registry({max_step})"),
        miner,
    );

    // Over the ceiling the contract must say so itself, not run out of gas
    // halfway through and leave the caller guessing.
    let over = confirm_solo_call_rejected(
        &mut f.chain,
        &user,
        &contract,
        &format!(
            "renew_channel({}, {})",
            addr(&user).to_readable(),
            max_step + 1
        ),
        miner,
    );
    assert!(
        !over.contains("OutOfGas"),
        "over-large renewals must be refused by the contract's own assert, not \
         by running out of storage gas: {over}"
    );
    println!(
        "SCENARIO L: step {max_step} is real; {} is refused cleanly: {over}",
        max_step + 1
    );
}

// ---------------------------------------------------------------------------
// SCENARIO M - the user opens a channel, then takes their time before paying
// the deposit in. Custody must never begin behind a record that is nearly worn
// out, because the record is the only thing that can ever release the coin.
// ---------------------------------------------------------------------------
#[test]
fn scenario_m_custody_never_starts_behind_a_worn_out_record() {
    // 9000 blocks after `init` the original 100-period lease is nearly gone.
    let mut f = Hostile::open_funding_after("m", 9_000);
    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();
    let hub = f.hub.clone();
    let stranger = f.stranger.clone();

    let (live, recover) = f.channel_lease("c_status_").expect("record exists");
    println!("SCENARIO M: funded 9000 blocks after init -> live={live} recover={recover}");
    assert!(
        live + recover >= 50_000,
        "the contract must not take custody behind a record with only {} blocks \
         of reach left",
        live + recover
    );
    assert!(
        live >= 5_000,
        "a late-funded channel must still be usable, not merely dormant-safe \
         (live={live})"
    );

    // And it behaves like any other channel.
    let s3 = f.bill(3, 900_000, 100_000).cosigned(&user, &hub);
    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("challenge", &s3),
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
    let before = f.user_balance();
    let claim = submit_payout(&mut f.chain, &stranger, &contract, addr(&user), 900_000);
    f.chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&claim);
    assert_eq!(f.user_balance(), before + 900_000);
}

// ---------------------------------------------------------------------------
// SCENARIO G - the Hub front-runs the user's challenge with a stale bill, so
// the user's own `challenge` transaction reverts. Is the user stuck?
// ---------------------------------------------------------------------------
#[test]
fn scenario_g_front_run_challenge_is_recoverable_by_responding() {
    let mut f = Hostile::open("g");
    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();
    let hub = f.hub.clone();

    let s2 = f.bill(2, 600_000, 400_000).cosigned(&user, &hub);
    let s3 = f.bill(3, 900_000, 100_000).cosigned(&user, &hub);

    let hostile = submit_solo_call(&mut f.chain, &hub, &contract, &bill_call("challenge", &s2));
    f.chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&hostile);

    // The user's queued `challenge` now hits a CHALLENGING channel and reverts.
    let reverted = confirm_solo_call_rejected(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("challenge", &s3),
        miner,
    );
    println!("SCENARIO G: user's own challenge reverted: {reverted}");

    // Same bill, right entry point, still inside the window.
    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("respond", &s3),
        miner,
    );
    assert_eq!(
        f.chain
            .storage(&contract, &channel_key("c_left_balance_", &addr(&user))),
        Value::U64(900_000)
    );
    println!("SCENARIO G: recoverable, but ONLY by switching to `respond` inside the window.");
}

// ---------------------------------------------------------------------------
// SCENARIO H - how wide is the window really? Confirm the deadline is set from
// the challenge block and is NOT extended by a response.
// ---------------------------------------------------------------------------
#[test]
fn scenario_h_response_does_not_extend_the_window() {
    let mut f = Hostile::open("h");
    let miner = f.miner;
    let contract = f.contract.clone();
    let user = f.user.clone();
    let hub = f.hub.clone();

    let s2 = f.bill(2, 600_000, 400_000).cosigned(&user, &hub);
    let s3 = f.bill(3, 900_000, 100_000).cosigned(&user, &hub);

    let hostile = submit_solo_call(&mut f.chain, &hub, &contract, &bill_call("challenge", &s2));
    f.chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&hostile);
    let opened_at = f.chain.height();
    let deadline = f.deadline();
    assert_eq!(
        deadline,
        opened_at + CHALLENGE_BLOCKS,
        "window is exactly challenge_blocks from the challenge block"
    );

    confirm_solo_call(
        &mut f.chain,
        &user,
        &contract,
        &bill_call("respond", &s3),
        miner,
    );
    assert_eq!(
        f.deadline(),
        deadline,
        "a response must NOT extend the arbitration window"
    );
    println!("SCENARIO H: window = {CHALLENGE_BLOCKS} blocks from challenge, never extended.");
}
