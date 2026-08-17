//! HOSTILE HUB, ROUND 2. Three traps the first pass did not think of.
//!
//! The first pass (`hpay_hostile_hub_exit.rs`) asked one question thirteen
//! ways: with the Hub gone, can the user reach the coin behind THEIR channel?
//! It never asked what happens when the record the coin sits behind was already
//! dead before custody began (N), when the registry is holding somebody ELSE's
//! coin at the same time (O), or when the Hub holds a bill the user signed and
//! never got back (P).
//!
//! Same rules as round 1: real `TransactionType3` bytes, real signature
//! verification, real `BlockV1` execution, real HVM state, real Action 14
//! payouts, driven on `testkit::sim::memchain::MemChain`. The Hub signs nothing
//! after funding except where a scenario says so explicitly.
//!
//! Default assumption in every test below: THE USER IS TRAPPED. The assertions
//! are what has to prove otherwise.

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

    /// A bill the HUB manufactured entirely on its own - both signature slots
    /// filled by the Hub's key. This is what a Hub would need to be able to do
    /// to invent a debt the user never agreed to.
    fn hub_forged(mut self, hub: &Account) -> Self {
        let commitment = self.commitment();
        self.left_sign = Sign::create_by(hub, &commitment);
        self.hub_sign = Sign::create_by(hub, &commitment);
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
        "HOSTILE-HUB ESCAPE UNEXPECTEDLY SUCCEEDED: {call}"
    );
    format!("{:?}", receipt.error)
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

/// Pay `amount` out of the contract to `recipient`, paid for by `payer`, and
/// require it to succeed.
fn confirm_payout(
    chain: &mut MemChain,
    payer: &Account,
    contract: &ContractAddress,
    recipient: Address,
    amount: u64,
    miner: Address,
) {
    let hash = submit_payout(chain, payer, contract, recipient, amount);
    chain
        .confirm_formal_block(miner)
        .expect("execute payout")
        .expect_success(&hash);
}

fn confirm_payout_rejected(
    chain: &mut MemChain,
    payer: &Account,
    contract: &ContractAddress,
    recipient: Address,
    amount: u64,
    miner: Address,
) -> String {
    let hash = submit_payout(chain, payer, contract, recipient, amount);
    let block = chain
        .confirm_formal_block_observing_failures(miner)
        .expect("execute expected-rejection payout");
    let receipt = block.receipt(&hash).expect("expected-rejection receipt");
    assert!(
        receipt.is_error(),
        "PAYOUT UNEXPECTEDLY SUCCEEDED: {amount} zhu to {}",
        recipient.to_readable()
    );
    format!("{:?}", receipt.error)
}

/// A registry deployment plus however many channels a scenario needs. Round 1
/// always had exactly one channel; O needs two.
struct Registry {
    chain: MemChain,
    hub: Account,
    stranger: Account,
    miner: Address,
    network: [u8; 32],
    contract: ContractAddress,
}

impl Registry {
    fn deploy(seed: &str) -> Self {
        let mut chain = MemChain::new();
        chain.set_height(protocol::upgrade::ONLINE_OPEN_HEIGHT);
        let hub = Account::create_by(&format!("r2-hub-{seed}")).unwrap();
        let stranger = Account::create_by(&format!("r2-stranger-{seed}")).unwrap();
        let miner = addr(&Account::create_by(&format!("r2-miner-{seed}")).unwrap());
        let hub_a = addr(&hub);
        chain.mint_hac(&hub_a, 30_000_000_000_000);
        chain.mint_hac(&addr(&stranger), 30_000_000_000_000);

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
            stranger,
            miner,
            network,
            contract,
        }
    }

    fn new_user(&mut self, seed: &str) -> Account {
        let user = Account::create_by(&format!("r2-user-{seed}")).unwrap();
        self.chain.mint_hac(&addr(&user), 30_000_000_000_000);
        user
    }

    /// `init` only. Opening is cooperative by nature (both signatures asserted),
    /// so the Hub cooperates here and nowhere after.
    fn init_channel(&mut self, user: &Account, channel_id: [u8; 16]) {
        let call = format!(
            "init(0x{}, 0, {}, {}, {}, 100)",
            hex::encode(channel_id),
            addr(user).to_readable(),
            DEPOSIT,
            CHALLENGE_BLOCKS,
        );
        let hub = self.hub.clone();
        let hash = self
            .chain
            .submit_formal_main_call_fitsh_with_signers(
                user,
                &[&hub],
                vec![addr(user), self.contract.to_addr(), addr(&hub)],
                &call_source(&self.contract, &call),
                u8::MAX,
            )
            .expect("build channel init");
        self.chain
            .confirm_formal_block(self.miner)
            .unwrap()
            .expect_success(&hash);
    }

    fn submit_funding(&mut self, user: &Account) -> Hash {
        let mut fund = HacToTrs::new();
        fund.to = AddrOrPtr::from_addr(self.contract.to_addr());
        fund.hacash = Amount::zhu(DEPOSIT);
        self.chain
            .submit_formal_actions(
                user,
                vec![addr(user), self.contract.to_addr()],
                vec![Box::new(fund)],
                u8::MAX,
                TxOutput::None,
            )
            .expect("build channel funding")
    }

    fn fund_channel(&mut self, user: &Account) {
        let hash = self.submit_funding(user);
        self.chain
            .confirm_formal_block(self.miner)
            .unwrap()
            .expect_success(&hash);
        assert_eq!(
            self.chain
                .storage(&self.contract, &channel_key("c_status_", &addr(user))),
            Value::U8(2),
            "channel must be OPEN after funding"
        );
    }

    fn open_channel(&mut self, seed: &str, channel_id: [u8; 16]) -> Account {
        let user = self.new_user(seed);
        self.init_channel(&user, channel_id);
        self.fund_channel(&user);
        user
    }

    #[allow(clippy::too_many_arguments)]
    fn bill(
        &self,
        user: &Account,
        channel_id: [u8; 16],
        serial: u64,
        left_balance: u64,
        hub_balance: u64,
    ) -> Bill {
        Bill {
            network: self.network,
            contract: self.contract.to_addr(),
            channel_id,
            reuse: 0,
            left: addr(user),
            hub: addr(&self.hub),
            total: DEPOSIT,
            challenge_blocks: CHALLENGE_BLOCKS,
            serial,
            left_balance,
            hub_balance,
            left_sign: Sign::new(),
            hub_sign: Sign::new(),
        }
    }

    fn status(&self, user: &Account) -> Value {
        self.chain
            .storage(&self.contract, &channel_key("c_status_", &addr(user)))
    }

    fn deadline(&self, user: &Account) -> u64 {
        match self
            .chain
            .storage(&self.contract, &channel_key("c_deadline_", &addr(user)))
        {
            Value::U64(v) => v,
            other => panic!("deadline must be u64, got {other:?}"),
        }
    }

    fn global(&self, key: &str) -> Value {
        self.chain
            .storage(&self.contract, &Value::bytes(key.as_bytes().to_vec()))
    }

    fn contract_balance(&self) -> u64 {
        self.chain
            .balance(&self.contract.to_addr())
            .to_zhu_u64()
            .expect("contract balance")
    }

    fn balance_of(&self, account: &Account) -> u64 {
        self.chain
            .balance(&addr(account))
            .to_zhu_u64()
            .expect("account balance")
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

    /// Run challenge -> wait out the window -> finalize, all on `signer`'s key
    /// and fees, with no Hub participation whatsoever.
    fn unilateral_settle(&mut self, signer: &Account, user: &Account, bill: &Bill) {
        let miner = self.miner;
        let contract = self.contract.clone();
        confirm_solo_call(
            &mut self.chain,
            signer,
            &contract,
            &bill_call("challenge", bill),
            miner,
        );
        let deadline = self.deadline(user);
        self.chain
            .confirm_empty_formal_blocks_to_height(miner, deadline)
            .unwrap();
        confirm_solo_call(
            &mut self.chain,
            signer,
            &contract,
            &format!("finalize({})", addr(user).to_readable()),
            miner,
        );
        assert_eq!(self.status(user), Value::U8(4), "channel must be FINAL");
    }
}

// ---------------------------------------------------------------------------
// SCENARIO N - CUSTODY BEHIND A CORPSE.
//
// `init` mints every channel key with `storage_new(..., 100)`: 9999 live blocks
// and, at that moment, ZERO recovery. The record only gets its dormant window
// later, inside `PayableHAC`, when the deposit actually arrives. So there is a
// window - roughly 9999 blocks, about 34 days at the 300s target - in which an
// opened-but-unfunded channel record simply dies.
//
// Round 1's scenario M funded 9000 blocks late and proved the record was still
// usable. Nothing asked what happens one lease past that. The user is about to
// hand a real deposit to a contract whose record of their channel no longer
// exists. If the coin lands anyway, it is destroyed on arrival: PermitHAC reads
// `c_status_` to release anything, and `c_status_` is gone.
//
// DEFAULT ASSUMPTION: the deposit is swallowed. Prove otherwise.
// ---------------------------------------------------------------------------
#[test]
fn scenario_n_funding_a_dead_record_must_not_swallow_the_deposit() {
    let mut r = Registry::deploy("n");
    let miner = r.miner;
    let contract = r.contract.clone();
    let channel_id = [0x5C_u8; 16];
    let user = r.new_user("n");
    r.init_channel(&user, channel_id);

    let (live, recover) = r
        .lease(&channel_key("c_status_", &addr(&user)))
        .expect("c_status_ exists right after init");
    println!("SCENARIO N: an OPENED, UNFUNDED record has live={live} recover={recover}");
    assert_eq!(
        recover, 0,
        "an unfunded record has no dormant window at all, so it is deleted \
         outright when the live lease ends - that is the whole premise here"
    );

    // The user takes one lease longer than that to send the money.
    r.chain.set_height(r.chain.height() + live + 2);
    assert!(
        r.lease(&channel_key("c_status_", &addr(&user))).is_none(),
        "the channel record must really be gone for this test to mean anything"
    );
    println!(
        "SCENARIO N: {} blocks later the record is DELETED, not dormant",
        live + 2
    );

    let before = r.balance_of(&user);
    let funding = r.submit_funding(&user);
    let block = r
        .chain
        .confirm_formal_block_observing_failures(miner)
        .expect("execute funding into a dead record");
    let receipt = block.receipt(&funding).expect("funding receipt");
    println!(
        "SCENARIO N: funding a deleted record -> {:?}",
        receipt.error
    );

    assert!(
        receipt.is_error(),
        "MONEY DESTROYED: the contract accepted a {DEPOSIT} zhu deposit behind a \
         channel record that no longer exists, and PermitHAC reads that record \
         to release anything"
    );
    assert_eq!(
        r.contract_balance(),
        0,
        "MONEY DESTROYED: the deposit is sitting in the contract account with no \
         channel record to ever release it"
    );
    let after = r.balance_of(&user);
    assert!(
        after >= before.saturating_sub(DEPOSIT / 2),
        "the user must keep their deposit; they only lose the network fee \
         (before={before} after={after})"
    );
    println!(
        "SCENARIO N: deposit refused and returned. user {before} -> {after} zhu \
         (difference is the network fee, not the {DEPOSIT} zhu deposit)."
    );

    // Second finding, free with the first: the REGISTRY has been idle just as
    // long, so its own globals are dormant too, and `init` reads `g_hub`. Until
    // somebody pays the registry's rent, nothing at all works here - not even
    // opening a fresh channel.
    let dormant = confirm_solo_call_rejected(
        &mut r.chain,
        &user,
        &contract,
        &format!(
            "init(0x{}, 0, {}, {}, {}, 100)",
            hex::encode(channel_id),
            addr(&user).to_readable(),
            DEPOSIT,
            CHALLENGE_BLOCKS
        ),
        miner,
    );
    println!("SCENARIO N: init against a dormant registry -> {dormant}");
    assert!(
        dormant.contains("StorageNotActive"),
        "expected the dormant-registry refusal, got {dormant}"
    );
    let (g_live, g_recover) = r
        .lease(&Value::bytes(b"g_hub".to_vec()))
        .expect("g_hub must be dormant, never destroyed");
    assert_eq!(g_live, 0, "the registry's live lease really has run out");
    assert!(
        g_recover > 0,
        "REGISTRY DESTROYED: g_hub is gone, and PermitHAC reads it for every \
         payout in every channel of this deployment"
    );
    println!("SCENARIO N: g_hub is dormant with {g_recover} recover blocks, restorable by anyone");

    // Anyone at all can pay that rent. The user does it here with their own key
    // and no Hub in sight.
    confirm_solo_call(&mut r.chain, &user, &contract, "renew_registry(100)", miner);

    // And now the user can simply start over: the old record is gone, so `init`
    // sees nil and takes the reuse == 0 branch that a fresh channel takes.
    r.init_channel(&user, channel_id);
    r.fund_channel(&user);
    let s3 = r
        .bill(&user, channel_id, 3, 900_000, 100_000)
        .cosigned(&user, &r.hub.clone());
    r.unilateral_settle(&user.clone(), &user.clone(), &s3);
    let before = r.balance_of(&user);
    let stranger = r.stranger.clone();
    confirm_payout(
        &mut r.chain,
        &stranger,
        &contract,
        addr(&user),
        900_000,
        miner,
    );
    assert_eq!(r.balance_of(&user), before + 900_000);
    println!("SCENARIO N: user re-opened the same channel slot and exited in full, unaided.");
}

// ---------------------------------------------------------------------------
// SCENARIO O - THE OTHER DEPOSITOR.
//
// Every round-1 scenario had exactly one channel in the registry. A V2
// deployment is SHARED: one contract account holds every channel's coin at
// once, and `PermitHAC`'s Hub branch checks only the AGGREGATE
// `g_hub_claimable`, never which channel the money came from.
//
// So: two users fund 1_000_000 each. The Hub goes silent. User one exits. The
// questions round 1 never asked are all about user two -
//   - can user one, or the Hub, or a stranger, reach user two's still-locked
//     coin through the shared contract account?
//   - after user one has drained their share, is there still enough left, and
//     does the accounting still let user two out?
//
// DEFAULT ASSUMPTION: the second depositor is trapped by the first. Prove
// otherwise.
// ---------------------------------------------------------------------------
#[test]
fn scenario_o_one_users_exit_must_not_strand_the_other_depositor() {
    let mut r = Registry::deploy("o");
    let miner = r.miner;
    let contract = r.contract.clone();
    let hub = r.hub.clone();
    let stranger = r.stranger.clone();

    let id_one = [0x11_u8; 16];
    let id_two = [0x22_u8; 16];
    let one = r.open_channel("o-one", id_one);
    let two = r.open_channel("o-two", id_two);

    assert_eq!(
        r.contract_balance(),
        2 * DEPOSIT,
        "the registry holds both deposits in one account"
    );
    assert_eq!(r.global("g_locked"), Value::U64(2 * DEPOSIT));
    assert_eq!(r.global("g_open_count"), Value::U64(2));

    // Both users have spent 100_000 with the Hub. The Hub now vanishes and
    // signs nothing else, ever.
    let one_s3 = r
        .bill(&one, id_one, 3, 900_000, 100_000)
        .cosigned(&one, &hub);
    let two_s3 = r
        .bill(&two, id_two, 3, 900_000, 100_000)
        .cosigned(&two, &hub);

    // User one exits alone.
    r.unilateral_settle(&one.clone(), &one.clone(), &one_s3);
    assert_eq!(r.global("g_left_claimable"), Value::U64(900_000));
    assert_eq!(r.global("g_hub_claimable"), Value::U64(100_000));
    assert_eq!(
        r.global("g_locked"),
        Value::U64(DEPOSIT),
        "user two's deposit must still be locked, untouched by user one's exit"
    );

    // Before taking their own money, user one tries to take user two's.
    let cross = confirm_payout_rejected(&mut r.chain, &one, &contract, addr(&two), 900_000, miner);
    println!("SCENARIO O: user one paying user two's channel out early -> {cross}");
    let greedy = confirm_payout_rejected(
        &mut r.chain,
        &one,
        &contract,
        addr(&one),
        900_000 + 900_000,
        miner,
    );
    println!("SCENARIO O: user one claiming both shares at once -> {greedy}");

    // The Hub, still silent on chain but happy to spend, tries to take the
    // aggregate rather than its own 100_000 slice.
    let hub_grab =
        confirm_payout_rejected(&mut r.chain, &hub, &contract, addr(&hub), 200_000, miner);
    println!(
        "SCENARIO O: Hub claiming both channels' fees before the second settles -> {hub_grab}"
    );

    // User one takes exactly their own share, a stranger paying the fee.
    let one_before = r.balance_of(&one);
    confirm_payout(
        &mut r.chain,
        &stranger,
        &contract,
        addr(&one),
        900_000,
        miner,
    );
    assert_eq!(r.balance_of(&one), one_before + 900_000);
    let replay = confirm_payout_rejected(
        &mut r.chain,
        &stranger,
        &contract,
        addr(&one),
        900_000,
        miner,
    );
    println!("SCENARIO O: user one claiming a second time -> {replay}");

    assert_eq!(
        r.contract_balance(),
        DEPOSIT + 100_000,
        "what is left must be exactly user two's deposit plus the Hub's unclaimed fee"
    );

    // NOW the real question. User two has done nothing this whole time and the
    // Hub has never come back.
    r.unilateral_settle(&two.clone(), &two.clone(), &two_s3);
    // A stranger pays this fee so the delta on user two is exactly the coin the
    // contract released, with no fee noise in the number.
    let two_before = r.balance_of(&two);
    confirm_payout(
        &mut r.chain,
        &stranger,
        &contract,
        addr(&two),
        900_000,
        miner,
    );
    assert_eq!(
        r.balance_of(&two),
        two_before + 900_000,
        "SECOND DEPOSITOR TRAPPED: user two did not receive their full 900_000 \
         after user one had already exited"
    );

    assert_eq!(
        r.global("g_locked"),
        Value::U64(0),
        "nothing may remain locked once both channels are final"
    );
    assert_eq!(
        r.global("g_left_claimable"),
        Value::U64(0),
        "both users took exactly their own share"
    );
    assert_eq!(
        r.global("g_hub_claimable"),
        Value::U64(200_000),
        "the Hub's unclaimed fees are all that is left owed"
    );
    assert_eq!(
        r.contract_balance(),
        200_000,
        "the contract holds exactly the Hub's unclaimed fees and not one zhu of \
         either user's money"
    );
    println!(
        "SCENARIO O: BOTH depositors got out unaided. Neither could touch the \
         other's coin, and the first one out did not strand the second."
    );
}

// ---------------------------------------------------------------------------
// SCENARIO P - THE BILL THE USER SIGNED AND NEVER GOT BACK.
//
// The payment flow is: the user signs an unsigned bill and sends it to the Hub
// for countersignature. A Hub that countersigns and then never returns the
// result holds a fully valid bill at serial N+1 that the user does not have.
// The user's own durable head stops at N.
//
// Round 1's scenario D had the Hub CHALLENGE with a stale bill while the user
// slept. This is the mirror and it is live on today's one-directional rail: the
// user challenges with the newest bill they hold, and the Hub answers inside
// the window with a higher-serial bill that pays the user LESS. On a rail where
// every later bill moves money from the user to the Hub, the Hub always wins
// arbitration by telling the truth.
//
// DEFAULT ASSUMPTION: the Hub can settle at any number it likes. Prove that the
// loss is bounded by exactly what the user actually signed, and that the user
// still gets that.
// ---------------------------------------------------------------------------
#[test]
fn scenario_p_hub_answers_with_a_higher_bill_the_user_never_received() {
    let mut r = Registry::deploy("p");
    let miner = r.miner;
    let contract = r.contract.clone();
    let hub = r.hub.clone();
    let stranger = r.stranger.clone();
    let id = [0x33_u8; 16];
    let user = r.open_channel("p", id);

    // What the user's wallet has: serial 3, 900_000 theirs.
    let head = r.bill(&user, id, 3, 900_000, 100_000).cosigned(&user, &hub);
    // What the Hub is sitting on: the user's signature on serial 4, countersigned
    // and never sent back.
    let withheld = r.bill(&user, id, 4, 700_000, 300_000).cosigned(&user, &hub);
    // What the Hub would LIKE to have: a serial 5 the user never signed.
    let invented = r.bill(&user, id, 5, 0, DEPOSIT).hub_forged(&hub);

    // The user opens the exit with everything they have.
    confirm_solo_call(
        &mut r.chain,
        &user,
        &contract,
        &bill_call("challenge", &head),
        miner,
    );
    assert_eq!(r.status(&user), Value::U8(3));
    assert_eq!(
        r.chain
            .storage(&contract, &channel_key("c_left_balance_", &addr(&user))),
        Value::U64(900_000),
        "the user's own head is what is on chain at this instant"
    );

    // First the Hub tries to invent a debt outright.
    let forged = confirm_solo_call_rejected(
        &mut r.chain,
        &hub,
        &contract,
        &bill_call("respond", &invented),
        miner,
    );
    println!("SCENARIO P: Hub responding with a bill the user never signed -> {forged}");
    assert_eq!(
        r.chain
            .storage(&contract, &channel_key("c_left_balance_", &addr(&user))),
        Value::U64(900_000),
        "a forged response must not move the balance one zhu"
    );

    // Now the Hub plays the real withheld bill, inside the window.
    confirm_solo_call(
        &mut r.chain,
        &hub,
        &contract,
        &bill_call("respond", &withheld),
        miner,
    );
    assert_eq!(
        r.chain
            .storage(&contract, &channel_key("c_left_balance_", &addr(&user))),
        Value::U64(700_000),
        "the Hub's higher-serial bill supersedes the user's head"
    );
    println!(
        "SCENARIO P: the Hub's withheld serial-4 bill superseded the user's \
         serial-3 head. The user settles 200_000 lower than their wallet said."
    );

    // The user cannot undo it by re-playing their own older bill.
    let backwards = confirm_solo_call_rejected(
        &mut r.chain,
        &user,
        &contract,
        &bill_call("respond", &head),
        miner,
    );
    println!("SCENARIO P: user re-playing their serial-3 head -> {backwards}");

    // Anyone finalizes. A stranger does it, to make the point that finalize
    // cannot change who is paid.
    let deadline = r.deadline(&user);
    r.chain
        .confirm_empty_formal_blocks_to_height(miner, deadline)
        .unwrap();
    confirm_solo_call(
        &mut r.chain,
        &stranger,
        &contract,
        &format!("finalize({})", addr(&user).to_readable()),
        miner,
    );

    // The Hub cannot take the user's remainder even now.
    let grab = confirm_payout_rejected(&mut r.chain, &hub, &contract, addr(&hub), 700_000, miner);
    println!("SCENARIO P: Hub trying to take the user's settled 700_000 -> {grab}");

    // The user still gets exactly what the bill they really signed says.
    let before = r.balance_of(&user);
    confirm_payout(
        &mut r.chain,
        &stranger,
        &contract,
        addr(&user),
        700_000,
        miner,
    );
    assert_eq!(
        r.balance_of(&user),
        before + 700_000,
        "the user must still receive every zhu of the settled bill"
    );
    assert_eq!(
        r.global("g_left_claimable"),
        Value::U64(0),
        "the user's whole settled share was paid out"
    );
    println!(
        "SCENARIO P: user paid 700_000. The 200_000 difference came from a bill \
         the user's own key signed - the Hub could not invent a zhu beyond it. \
         The exposure is exactly the in-flight bills the wallet has signed and \
         not recorded as its head."
    );
}

// ---------------------------------------------------------------------------
// MEASUREMENT - CAN THE WALLET EVEN POINT AT THIS CONTRACT?
//
// Not a hostile-Hub scenario. Every scenario above proves what the CHAIN will
// let a user do. None of them prove the shipped wallet can build the bytes,
// because the wallet reaches this contract through one gate:
// `HvmRegistryBindingV2::validate()` refuses any binding whose `bytecode_sha3`
// is not the reviewed pin, and every exit builder - including the new
// `ChannelLeft` role - opens with `binding.validate()?`.
//
// So the pin is the join between the two repos, and it is worth measuring
// rather than assuming. This test only reports; it deliberately does not fail
// the fullnode build while the contract is being revised, because the contract
// SHOULD change and the pin SHOULD follow it. What must never happen silently
// is the two drifting apart with nobody noticing.
// ---------------------------------------------------------------------------
#[test]
fn measurement_the_wallets_reviewed_pin_against_this_contract() {
    use field::Hex;

    // Copied literals, not imports: this crate must not depend on the wallet
    // to be able to say what the wallet believes.
    // crates/l2-fast-pay-hub/src/hvm_registry.rs  HPAY_REGISTRY_BYTECODE_SHA3
    const WALLET_PINNED_BYTECODE_SHA3: &str =
        "276d8c205296cc50d06244c84d52c5a9f6f4711e0abae67f416e4fc79c9294be";
    // crates/l2-fast-pay-hub/src/hvm_registry_pilot.rs  HPAY_REGISTRY_SOURCE_SHA256
    const WALLET_PINNED_SOURCE_SHA256: &str =
        "58ab4ba8931190a5b83f5b30a96d842281adf9d7e7069cbf8bf79a68945ae8a8";

    let compiled = vm::fitshc::compile(CONTRACT_SOURCE).unwrap().0.into_sto();
    let bytecode_sha3 = compiled.calc_edition().hash.to_hex();
    let source_sha256 = hex::encode(sys::sha2(CONTRACT_SOURCE.as_bytes().to_vec()));

    println!("MEASUREMENT: contract bytecode sha3 = {bytecode_sha3}");
    println!("MEASUREMENT: wallet pinned bytecode = {WALLET_PINNED_BYTECODE_SHA3}");
    println!("MEASUREMENT: contract source sha256 = {source_sha256}");
    println!("MEASUREMENT: wallet pinned source   = {WALLET_PINNED_SOURCE_SHA256}");

    if bytecode_sha3 == WALLET_PINNED_BYTECODE_SHA3 {
        println!(
            "MEASUREMENT: the wallet can bind to this contract; every exit \
             builder's first line will pass."
        );
    } else {
        println!(
            "MEASUREMENT: DRIFTED. `HvmRegistryBindingV2::validate()` refuses \
             every binding to this contract, so the user-side exit driver \
             cannot be pointed at the code these scenarios just ran against. \
             The 16 scenarios above still describe the chain correctly; what \
             is unreachable is the wallet. Re-pin HPAY_REGISTRY_BYTECODE_SHA3 \
             and HPAY_REGISTRY_SOURCE_SHA256 once the contract revision settles."
        );
    }
    assert_eq!(
        bytecode_sha3.len(),
        64,
        "the edition hash must be a 32-byte hex digest"
    );
}
