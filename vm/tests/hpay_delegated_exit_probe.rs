//! DESIGN PROBE for the wallet-side unilateral exit driver.
//!
//! Two questions the hostile-Hub suite left open, both load bearing for the
//! design of a user exit and of any watchtower that would answer a challenge
//! while the user sleeps:
//!
//! 1. How many storage rent periods can ONE user-signed `renew_channel` /
//!    `renew_registry` transaction actually buy? (Scenario F only bracketed it
//!    between 100 and 500.)
//! 2. Can a party that is NEITHER the user NOR the Hub drive the whole exit -
//!    `challenge`, `respond`, `finalize` and the Action 14 payout - holding
//!    nothing but a copy of the co-signed bill and no key of the user's? And if
//!    it can, can it redirect a single zhu to itself?
//!
//! Answer 2 decides whether a watchtower has to hold the user's private key.

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
    stranger: Account,
    miner: Address,
    network: [u8; 32],
    contract: ContractAddress,
    channel_id: [u8; 16],
}

fn open(seed: &str) -> Fixture {
    let mut chain = MemChain::new();
    chain.set_height(protocol::upgrade::ONLINE_OPEN_HEIGHT);
    let hub = Account::create_by(&format!("probe-hub-{seed}")).unwrap();
    let user = Account::create_by(&format!("probe-user-{seed}")).unwrap();
    let stranger = Account::create_by(&format!("probe-stranger-{seed}")).unwrap();
    let miner = addr(&Account::create_by(&format!("probe-miner-{seed}")).unwrap());
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
        stranger,
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
    /// A bill both parties signed off chain. Neither key is used again.
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
        let mut action = HacFromToTrs::new();
        action.from = AddrOrPtr::from_addr(self.contract.to_addr());
        action.to = AddrOrPtr::from_addr(recipient);
        action.hacash = Amount::zhu(amount);
        let hash = self
            .chain
            .submit_formal_actions(
                payer,
                vec![addr(payer), self.contract.to_addr(), recipient],
                vec![Box::new(action)],
                u8::MAX,
                TxOutput::None,
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

    fn deadline(&self) -> u64 {
        match self.chain.storage(
            &self.contract,
            &channel_key("c_deadline_", &addr(&self.user)),
        ) {
            Value::U64(v) => v,
            other => panic!("deadline must be u64, got {other:?}"),
        }
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
}

// ---------------------------------------------------------------------------
// PROBE 1 - how long does one renewal transaction buy?
// ---------------------------------------------------------------------------
#[test]
fn probe_max_renew_periods_per_transaction() {
    println!(
        "one rent period = 100 blocks; CONTRACT_STORE_PERM_PERIODS = {}",
        protocol::params::CONTRACT_STORE_PERM_PERIODS
    );
    let mut f = open("rent");
    println!(
        "AT OPEN: c_status_ live_blocks = {} (init rent_periods = 100)",
        f.live_blocks("c_status_")
    );
    for periods in [100_u64, 150, 200, 250, 400, 500] {
        let user = f.user.clone();
        let call = format!("renew_channel({}, {periods})", addr(&user).to_readable());
        match f.call_by(&user, &call) {
            Ok(()) => println!(
                "renew_channel({periods}) OK -> c_status_ live_blocks now {}",
                f.live_blocks("c_status_")
            ),
            Err(error) => println!("renew_channel({periods}) REFUSED: {error}"),
        }
        match f.call_by(&user, &format!("renew_registry({periods})")) {
            Ok(()) => println!("renew_registry({periods}) OK"),
            Err(error) => println!("renew_registry({periods}) REFUSED: {error}"),
        }
    }
}

// ---------------------------------------------------------------------------
// PROBE 2 - a stranger holding only the co-signed bill drives the entire exit
// while the user is offline, and cannot take a single zhu.
// ---------------------------------------------------------------------------
#[test]
fn probe_stranger_with_only_the_bill_completes_the_whole_exit() {
    let mut f = open("delegate");
    let stranger = f.stranger.clone();
    let hub = f.hub.clone();
    let user = f.user.clone();

    let s2 = f.cosigned(2, 600_000, 400_000);
    let s3 = f.cosigned(3, 900_000, 100_000);

    // Hostile Hub opens with the stale split. The user is asleep from here on
    // and signs nothing, pays nothing and is never asked for anything.
    let challenge = f.bill_call("challenge", &s2);
    f.call_by(&hub, &challenge).expect("hub stale challenge");

    // The watchtower is a stranger. It holds a copy of serial 3 and no key of
    // the user's. Does the contract let it answer?
    let respond = f.bill_call("respond", &s3);
    f.call_by(&stranger, &respond)
        .expect("STRANGER COULD NOT RESPOND - a watchtower would need the user's key");
    assert_eq!(
        f.chain
            .storage(&f.contract, &channel_key("c_left_balance_", &addr(&user))),
        Value::U64(900_000),
        "the stranger's response must install the user's true latest split"
    );
    println!("PROBE 2: a stranger with only the bill RESPONDED and installed serial 3.");

    let deadline = f.deadline();
    f.chain
        .confirm_empty_formal_blocks_to_height(f.miner, deadline)
        .unwrap();
    let finalize = format!("finalize({})", addr(&user).to_readable());
    f.call_by(&stranger, &finalize).expect("stranger finalize");

    // Can the watchtower pay itself instead? Both shapes of theft.
    let to_self = f.payout_by(&stranger, addr(&stranger), 900_000);
    println!("PROBE 2: stranger paying itself -> {to_self:?}");
    assert!(to_self.is_err(), "WATCHTOWER DRAINED THE USER");
    let skim = f.payout_by(&stranger, addr(&user), 100_000);
    println!("PROBE 2: stranger paying the user a WRONG amount -> {skim:?}");
    assert!(skim.is_err(), "PARTIAL PAYOUT ACCEPTED");

    let before = f.zhu(&addr(&user));
    f.payout_by(&stranger, addr(&user), 900_000)
        .expect("stranger completes the payout");
    assert_eq!(
        f.zhu(&addr(&user)),
        before + 900_000,
        "the coin must land on the user, paid for by the stranger"
    );
    println!(
        "PROBE 2: the whole exit ran on a stranger's key and fees. User +900_000, stranger could take nothing."
    );
}

// ---------------------------------------------------------------------------
// PROBE 3 - the same stranger opening the exit from OPEN, not just answering.
// This is the "user's own key is lost or offline for good" case.
// ---------------------------------------------------------------------------
#[test]
fn probe_stranger_can_open_the_challenge_too() {
    let mut f = open("open-by-stranger");
    let stranger = f.stranger.clone();
    let user = f.user.clone();
    let s3 = f.cosigned(3, 900_000, 100_000);

    let challenge = f.bill_call("challenge", &s3);
    f.call_by(&stranger, &challenge)
        .expect("STRANGER COULD NOT OPEN A CHALLENGE");
    assert_eq!(
        f.chain
            .storage(&f.contract, &channel_key("c_status_", &addr(&user))),
        Value::U8(3)
    );
    let deadline = f.deadline();
    f.chain
        .confirm_empty_formal_blocks_to_height(f.miner, deadline)
        .unwrap();
    f.call_by(
        &stranger,
        &format!("finalize({})", addr(&user).to_readable()),
    )
    .expect("stranger finalize");
    let before = f.zhu(&addr(&user));
    f.payout_by(&stranger, addr(&user), 900_000)
        .expect("stranger payout");
    assert_eq!(f.zhu(&addr(&user)), before + 900_000);
    println!(
        "PROBE 3: a stranger opened, finalised and paid out an exit for a user who did nothing."
    );
}

// ---------------------------------------------------------------------------
// PROBE 4 - can a stranger keep the storage lease alive for a channel that is
// not theirs? (Scenario F only proved the user can renew their own.)
// ---------------------------------------------------------------------------
#[test]
fn probe_stranger_can_pay_the_rent_for_someone_elses_channel() {
    let mut f = open("rent-by-stranger");
    let stranger = f.stranger.clone();
    let user = f.user.clone();
    let before = f.live_blocks("c_status_");
    // 150 is the contract's MAX_RENT_STEP: the largest step one transaction's
    // storage gas can actually pay for.
    let call = format!("renew_channel({}, 150)", addr(&user).to_readable());
    f.call_by(&stranger, &call)
        .expect("STRANGER COULD NOT PAY ANOTHER PARTY'S RENT");
    let after = f.live_blocks("c_status_");
    assert!(after > before);
    println!(
        "PROBE 4: a stranger extended someone else's channel lease {before} -> {after} blocks."
    );
}
