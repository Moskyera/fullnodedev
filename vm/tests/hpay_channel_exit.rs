use basis::method::verify_signature;
use field::{AddrOrPtr, Address, Amount, Field, Hash, Serialize, Sign, Uint4};
use protocol::action::{HacFromToTrs, HacToTrs};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use sys::Account;
use testkit::sim::memchain::{MemChain, TxOutput};
use vm::value::Value;
use vm::{ContractAddress, VMStateRead};

const DOMAIN: &[u8] = b"HPAY/HVM-CHANNEL/V1";
const CHALLENGE_BLOCKS: u64 = 12;

// Canonical versioned source. The artifact remains disabled for mainnet until
// its deployment and every live storage lease are independently verified.
const CONTRACT_SOURCE: &str = include_str!("../contracts/hpay_channel_exit_v1.fitsh");
const CONTRACT_MANIFEST: &str = include_str!("../contracts/hpay_channel_exit_v1.manifest.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Open,
    Challenging { deadline: u64 },
    Final,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitError {
    Binding,
    Conservation,
    InvalidSignature,
    StaleSerial,
    InvalidStatus,
    ChallengeExpired,
    ChallengePending,
    HeightOverflow,
}

#[derive(Clone)]
struct Bill {
    network: [u8; 32],
    contract: Address,
    channel_id: [u8; 16],
    reuse: u32,
    left: Address,
    right: Address,
    total: u64,
    challenge_blocks: u64,
    serial: u64,
    left_balance: u64,
    right_balance: u64,
    left_sign: Sign,
    right_sign: Sign,
}

impl Bill {
    fn unsigned(
        network: [u8; 32],
        contract: Address,
        channel_id: [u8; 16],
        reuse: u32,
        left: Address,
        right: Address,
        total: u64,
        challenge_blocks: u64,
        serial: u64,
        left_balance: u64,
        right_balance: u64,
    ) -> Self {
        Self {
            network,
            contract,
            channel_id,
            reuse,
            left,
            right,
            total,
            challenge_blocks,
            serial,
            left_balance,
            right_balance,
            left_sign: Sign::new(),
            right_sign: Sign::new(),
        }
    }

    fn commitment(&self) -> Hash {
        let mut bytes =
            Vec::with_capacity(DOMAIN.len() + 32 + 21 + 16 + 4 + 21 + 21 + 8 + 8 + 8 + 8 + 8);
        bytes.extend_from_slice(DOMAIN);
        bytes.extend_from_slice(&self.network);
        bytes.extend_from_slice(self.contract.as_bytes());
        bytes.extend_from_slice(&self.channel_id);
        bytes.extend_from_slice(&self.reuse.to_be_bytes());
        bytes.extend_from_slice(self.left.as_bytes());
        bytes.extend_from_slice(self.right.as_bytes());
        bytes.extend_from_slice(&self.total.to_be_bytes());
        bytes.extend_from_slice(&self.challenge_blocks.to_be_bytes());
        bytes.extend_from_slice(&self.serial.to_be_bytes());
        bytes.extend_from_slice(&self.left_balance.to_be_bytes());
        bytes.extend_from_slice(&self.right_balance.to_be_bytes());
        Hash::from(sys::sha3(bytes))
    }

    fn sign(mut self, left: &Account, right: &Account) -> Self {
        let hash = self.commitment();
        self.left_sign = Sign::create_by(left, &hash);
        self.right_sign = Sign::create_by(right, &hash);
        self
    }
}

struct ReferenceChannel {
    network: [u8; 32],
    contract: Address,
    channel_id: [u8; 16],
    reuse: u32,
    left: Address,
    right: Address,
    total: u64,
    serial: u64,
    left_balance: u64,
    right_balance: u64,
    status: Status,
}

impl ReferenceChannel {
    fn validate_bill(&self, bill: &Bill) -> Result<(), ExitError> {
        if bill.network != self.network
            || bill.contract != self.contract
            || bill.channel_id != self.channel_id
            || bill.reuse != self.reuse
            || bill.left != self.left
            || bill.right != self.right
            || bill.total != self.total
            || bill.challenge_blocks != CHALLENGE_BLOCKS
        {
            return Err(ExitError::Binding);
        }
        if bill.serial <= self.serial {
            return Err(ExitError::StaleSerial);
        }
        let Some(total) = bill.left_balance.checked_add(bill.right_balance) else {
            return Err(ExitError::Conservation);
        };
        if total != self.total {
            return Err(ExitError::Conservation);
        }
        let hash = bill.commitment();
        if !verify_signature(&hash, &self.left, &bill.left_sign)
            || !verify_signature(&hash, &self.right, &bill.right_sign)
        {
            return Err(ExitError::InvalidSignature);
        }
        Ok(())
    }

    fn accept_bill(&mut self, bill: &Bill) {
        self.serial = bill.serial;
        self.left_balance = bill.left_balance;
        self.right_balance = bill.right_balance;
    }

    fn challenge(&mut self, bill: &Bill, height: u64) -> Result<(), ExitError> {
        if self.status != Status::Open {
            return Err(ExitError::InvalidStatus);
        }
        self.validate_bill(bill)?;
        let deadline = height
            .checked_add(CHALLENGE_BLOCKS)
            .ok_or(ExitError::HeightOverflow)?;
        self.accept_bill(bill);
        self.status = Status::Challenging { deadline };
        Ok(())
    }

    fn respond(&mut self, bill: &Bill, height: u64) -> Result<(), ExitError> {
        let Status::Challenging { deadline } = self.status else {
            return Err(ExitError::InvalidStatus);
        };
        if height >= deadline {
            return Err(ExitError::ChallengeExpired);
        }
        self.validate_bill(bill)?;
        self.accept_bill(bill);
        Ok(())
    }

    fn cooperative_close(&mut self, bill: &Bill) -> Result<(), ExitError> {
        if self.status == Status::Final {
            return Err(ExitError::InvalidStatus);
        }
        self.validate_bill(bill)?;
        self.accept_bill(bill);
        self.status = Status::Final;
        Ok(())
    }

    fn finalize(&mut self, height: u64) -> Result<(u64, u64), ExitError> {
        let Status::Challenging { deadline } = self.status else {
            return Err(ExitError::InvalidStatus);
        };
        if height < deadline {
            return Err(ExitError::ChallengePending);
        }
        self.status = Status::Final;
        Ok((self.left_balance, self.right_balance))
    }
}

struct Fixture {
    channel: ReferenceChannel,
    left: Account,
    right: Account,
}

impl Fixture {
    fn new(seed: u8) -> Self {
        let left = Account::create_by(&format!("hpay-hvm-left-{seed}")).unwrap();
        let right = Account::create_by(&format!("hpay-hvm-right-{seed}")).unwrap();
        let deployer = Account::create_by(&format!("hpay-hvm-contract-{seed}")).unwrap();
        Self {
            channel: ReferenceChannel {
                network: [0x11; 32],
                contract: Address::from(deployer.address().clone()),
                channel_id: [seed; 16],
                reuse: 7,
                left: Address::from(left.address().clone()),
                right: Address::from(right.address().clone()),
                total: 1_000_000,
                serial: 0,
                left_balance: 600_000,
                right_balance: 400_000,
                status: Status::Open,
            },
            left,
            right,
        }
    }

    fn bill(&self, serial: u64, left_balance: u64, right_balance: u64) -> Bill {
        Bill::unsigned(
            self.channel.network,
            self.channel.contract,
            self.channel.channel_id,
            self.channel.reuse,
            self.channel.left,
            self.channel.right,
            self.channel.total,
            CHALLENGE_BLOCKS,
            serial,
            left_balance,
            right_balance,
        )
        .sign(&self.left, &self.right)
    }
}

#[test]
fn hvm_contract_source_compiles_without_enabling_mainnet_capability() {
    let output =
        vm::fitshc::compile(CONTRACT_SOURCE).expect("prototype Fitsh contract must compile");
    let bytes = output.0.serialize();
    assert!(!bytes.is_empty());
    let manifest: serde_json::Value = serde_json::from_str(CONTRACT_MANIFEST).unwrap();
    let source_hash = hex::encode(Sha256::digest(CONTRACT_SOURCE.as_bytes()));
    let storage_keys = manifest["storage_keys"].as_array().unwrap();
    let unique_storage_keys = storage_keys
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(manifest["schema"], "hpay-hvm-channel-exit-manifest/1");
    assert_eq!(manifest["contract_name"], "HPAYChannelExitV1");
    assert_eq!(manifest["protocol_domain"], "HPAY/HVM-CHANNEL/V1");
    assert_eq!(manifest["settlement_profile"], "hpay-hvm-channel-v1");
    assert_eq!(manifest["source_sha256"], source_hash);
    assert_eq!(storage_keys.len(), 18);
    assert_eq!(unique_storage_keys.len(), storage_keys.len());
    assert_eq!(
        manifest["required_action_kinds"],
        serde_json::json!([40, 41, 44])
    );
    assert_eq!(manifest["funding_model"]["left_deposit"], "positive");
    assert_eq!(
        manifest["funding_model"]["right_hub_deposit"],
        "exactly_zero"
    );
    assert_eq!(manifest["lease_policy"]["permissionless_renewal"], true);
    assert_eq!(
        manifest["lease_policy"]["must_renew_every_storage_key"],
        true
    );
    assert_eq!(manifest["mainnet_deployment"]["enabled"], false);
    assert_eq!(
        manifest["mainnet_deployment"]["independently_verified"],
        false
    );
    assert!(manifest["mainnet_deployment"]["contract_address"].is_null());
    assert!(manifest["mainnet_deployment"]["deployment_tx_hash"].is_null());
    assert!(manifest["mainnet_deployment"]["deployment_height"].is_null());
    assert_eq!(
        hex::encode(sys::sha3(bytes)),
        "11a2efc27a0c951bbc6977186eb58bd076dd331a785f3c57242cf54a72238349",
        "review and pin the exact compiled contract commitment"
    );
    assert_eq!(
        manifest["bytecode_sha3"],
        "11a2efc27a0c951bbc6977186eb58bd076dd331a785f3c57242cf54a72238349"
    );
}

#[test]
fn higher_serial_response_wins_and_finalizes_after_deadline() {
    let mut fixture = Fixture::new(1);
    let first = fixture.bill(1, 550_000, 450_000);
    fixture.channel.challenge(&first, 100).unwrap();
    let latest = fixture.bill(2, 525_000, 475_000);
    fixture.channel.respond(&latest, 111).unwrap();
    assert_eq!(
        fixture.channel.finalize(111),
        Err(ExitError::ChallengePending)
    );
    assert_eq!(fixture.channel.finalize(112), Ok((525_000, 475_000)));
}

#[test]
fn stale_replay_and_double_finalize_are_rejected() {
    let mut fixture = Fixture::new(2);
    let bill = fixture.bill(1, 500_000, 500_000);
    fixture.channel.challenge(&bill, 200).unwrap();
    assert_eq!(
        fixture.channel.respond(&bill, 201),
        Err(ExitError::StaleSerial)
    );
    fixture.channel.finalize(212).unwrap();
    assert_eq!(fixture.channel.finalize(213), Err(ExitError::InvalidStatus));
    assert_eq!(
        fixture
            .channel
            .cooperative_close(&fixture.bill(2, 400_000, 600_000)),
        Err(ExitError::InvalidStatus)
    );
}

#[test]
fn wrong_network_contract_channel_reuse_party_total_or_policy_is_rejected() {
    let fixture = Fixture::new(3);
    let mut cases = vec![];
    let mut wrong_network = fixture.bill(1, 500_000, 500_000);
    wrong_network.network[0] ^= 1;
    cases.push(wrong_network);
    let mut wrong_contract = fixture.bill(1, 500_000, 500_000);
    wrong_contract.contract = Address::from(
        Account::create_by("hpay-wrong-contract")
            .unwrap()
            .address()
            .clone(),
    );
    cases.push(wrong_contract);
    let mut wrong_channel = fixture.bill(1, 500_000, 500_000);
    wrong_channel.channel_id[0] ^= 1;
    cases.push(wrong_channel);
    let mut wrong_reuse = fixture.bill(1, 500_000, 500_000);
    wrong_reuse.reuse += 1;
    cases.push(wrong_reuse);
    let mut wrong_left = fixture.bill(1, 500_000, 500_000);
    wrong_left.left = Address::from(
        Account::create_by("hpay-wrong-left")
            .unwrap()
            .address()
            .clone(),
    );
    cases.push(wrong_left);
    let mut wrong_right = fixture.bill(1, 500_000, 500_000);
    wrong_right.right = Address::from(
        Account::create_by("hpay-wrong-right")
            .unwrap()
            .address()
            .clone(),
    );
    cases.push(wrong_right);
    let mut wrong_total = fixture.bill(1, 500_000, 500_000);
    wrong_total.total += 1;
    cases.push(wrong_total);
    let mut wrong_policy = fixture.bill(1, 500_000, 500_000);
    wrong_policy.challenge_blocks += 1;
    cases.push(wrong_policy);
    for bill in cases {
        assert_eq!(
            fixture.channel.validate_bill(&bill),
            Err(ExitError::Binding)
        );
    }
}

#[test]
fn conservation_overflow_and_wrong_signer_are_rejected() {
    let fixture = Fixture::new(4);
    let non_conserving = fixture.bill(1, 500_000, 499_999);
    assert_eq!(
        fixture.channel.validate_bill(&non_conserving),
        Err(ExitError::Conservation)
    );
    let overflow = fixture.bill(1, u64::MAX, 1);
    assert_eq!(
        fixture.channel.validate_bill(&overflow),
        Err(ExitError::Conservation)
    );
    let attacker = Account::create_by("hpay-hvm-attacker").unwrap();
    let mut bad_signature = fixture.bill(1, 500_000, 500_000);
    bad_signature.left_sign = Sign::create_by(&attacker, &bad_signature.commitment());
    assert_eq!(
        fixture.channel.validate_bill(&bad_signature),
        Err(ExitError::InvalidSignature)
    );
}

#[test]
fn response_after_deadline_and_height_overflow_are_rejected() {
    let mut fixture = Fixture::new(5);
    let first = fixture.bill(1, 500_000, 500_000);
    fixture.channel.challenge(&first, 300).unwrap();
    let response = fixture.bill(2, 450_000, 550_000);
    assert_eq!(
        fixture.channel.respond(&response, 312),
        Err(ExitError::ChallengeExpired)
    );

    let mut overflow_fixture = Fixture::new(6);
    let bill = overflow_fixture.bill(1, 500_000, 500_000);
    assert_eq!(
        overflow_fixture.channel.challenge(&bill, u64::MAX),
        Err(ExitError::HeightOverflow)
    );
    assert_eq!(overflow_fixture.channel.status, Status::Open);
}

#[test]
fn cooperative_close_uses_exact_signed_state() {
    let mut fixture = Fixture::new(7);
    let bill = fixture.bill(1, 375_000, 625_000);
    fixture.channel.cooperative_close(&bill).unwrap();
    assert_eq!(fixture.channel.status, Status::Final);
    assert_eq!(fixture.channel.left_balance, 375_000);
    assert_eq!(fixture.channel.right_balance, 625_000);
    assert_eq!(
        fixture
            .channel
            .cooperative_close(&fixture.bill(2, 300_000, 700_000)),
        Err(ExitError::InvalidStatus)
    );
}

fn account_address(account: &Account) -> Address {
    Address::from(account.address().clone())
}

fn contract_call_source(contract: &ContractAddress, call: &str) -> String {
    format!(
        "lib Channel = 1: {}\nvar result = Channel.{}\nassert result == 0\nend",
        contract.to_readable(),
        call
    )
}

fn renew_all_source(contract: &ContractAddress, periods: u64) -> String {
    let manifest: serde_json::Value = serde_json::from_str(CONTRACT_MANIFEST).unwrap();
    let keys = manifest["storage_keys"].as_array().unwrap();
    let mut source = format!("lib Channel = 1: {}\n", contract.to_readable());
    for (index, key) in keys.iter().enumerate() {
        let key = key.as_str().unwrap();
        source.push_str(&format!(
            r#"var renewal_{index} = Channel.renew("{key}", {periods})
assert renewal_{index} == 0
"#
        ));
    }
    source.push_str("end");
    source
}

fn confirm_source(
    chain: &mut MemChain,
    payer: &Account,
    extra_signers: &[&Account],
    contract: &ContractAddress,
    source: &str,
    miner: Address,
) {
    let payer_address = account_address(payer);
    let mut addrs = vec![payer_address, contract.to_addr()];
    addrs.extend(extra_signers.iter().map(|account| account_address(account)));
    let hash = chain
        .submit_formal_main_call_fitsh_with_signers(payer, extra_signers, addrs, source, u8::MAX)
        .expect("build signed formal HVM source call");
    chain
        .confirm_formal_block(miner)
        .expect("execute formal HVM source block")
        .expect_success(&hash);
}

fn confirm_call(
    chain: &mut MemChain,
    payer: &Account,
    extra_signers: &[&Account],
    contract: &ContractAddress,
    call: &str,
    miner: Address,
) {
    confirm_source(
        chain,
        payer,
        extra_signers,
        contract,
        &contract_call_source(contract, call),
        miner,
    );
}

fn confirm_deposit(
    chain: &mut MemChain,
    payer: &Account,
    contract: &ContractAddress,
    amount: u64,
    miner: Address,
) {
    let payer_address = account_address(payer);
    let mut action = HacToTrs::new();
    action.to = AddrOrPtr::from_addr(contract.to_addr());
    action.hacash = Amount::zhu(amount);
    let hash = chain
        .submit_formal_actions(
            payer,
            vec![payer_address, contract.to_addr()],
            vec![Box::new(action)],
            u8::MAX,
            TxOutput::None,
        )
        .expect("build signed formal deposit");
    chain
        .confirm_formal_block(miner)
        .expect("execute formal deposit block")
        .expect_success(&hash);
}

fn submit_payout(
    chain: &mut MemChain,
    payer: &Account,
    contract: &ContractAddress,
    recipient: Address,
    amount: u64,
) -> Hash {
    let payer_address = account_address(payer);
    let mut action = HacFromToTrs::new();
    action.from = AddrOrPtr::from_addr(contract.to_addr());
    action.to = AddrOrPtr::from_addr(recipient);
    action.hacash = Amount::zhu(amount);
    chain
        .submit_formal_actions(
            payer,
            vec![payer_address, contract.to_addr(), recipient],
            vec![Box::new(action)],
            u8::MAX,
            TxOutput::None,
        )
        .expect("build signed formal payout")
}

#[test]
fn private_chain_executes_storage_challenge_and_one_time_payout_hooks() {
    const LEFT_DEPOSIT: u64 = 600_000;
    const RIGHT_DEPOSIT: u64 = 0;
    const LEFT_FINAL: u64 = 300_000;
    const RIGHT_FINAL: u64 = 300_000;

    let mut chain = MemChain::new();
    chain.set_height(protocol::upgrade::ONLINE_OPEN_HEIGHT);
    let deployer = Account::create_by("hpay-hvm-e2e-deployer").unwrap();
    let left = Account::create_by("hpay-hvm-e2e-left").unwrap();
    let right = Account::create_by("hpay-hvm-e2e-right").unwrap();
    let watchtower = Account::create_by("hpay-hvm-e2e-watchtower").unwrap();
    let miner = account_address(&Account::create_by("hpay-hvm-e2e-miner").unwrap());
    let left_address = account_address(&left);
    let right_address = account_address(&right);
    let watchtower_address = account_address(&watchtower);
    for (address, amount) in [
        (account_address(&deployer), 20_000_000_000_000_u64),
        (left_address, 10_000_000_000_000),
        (right_address, 10_000_000_000_000),
        (watchtower_address, 10_000_000_000_000),
    ] {
        chain.mint_hac(&address, amount);
    }

    let compiled = vm::fitshc::compile(CONTRACT_SOURCE).unwrap().0.into_sto();
    let deployer_address = account_address(&deployer);
    let contract = ContractAddress::calculate(&deployer_address, &Uint4::from(0));
    let mut deploy = vm::action::ContractDeploy::new();
    deploy.nonce = Uint4::from(0);
    deploy.contract = compiled;
    deploy.protocol_cost = Amount::unit238(2_000_000_000_000);
    let deploy_hash = chain
        .submit_formal_actions(
            &deployer,
            vec![deployer_address],
            vec![Box::new(deploy)],
            u8::MAX,
            TxOutput::ContractAddress(contract.clone()),
        )
        .expect("build formal contract deployment");
    chain
        .confirm_formal_block(miner)
        .expect("execute deployment block")
        .expect_success(&deploy_hash);

    let network = [0x11_u8; 32];
    let channel_id = [0x42_u8; 16];
    let invalid_hub_funded_init = format!(
        "init(0x{}, 0x{}, 7, {}, {}, {}, 1, {}, 100)",
        hex::encode(network),
        hex::encode(channel_id),
        left_address.to_readable(),
        right_address.to_readable(),
        LEFT_DEPOSIT,
        CHALLENGE_BLOCKS,
    );
    let invalid_init_hash = chain
        .submit_formal_main_call_fitsh_with_signers(
            &left,
            &[&right],
            vec![left_address, contract.to_addr(), right_address],
            &contract_call_source(&contract, &invalid_hub_funded_init),
            u8::MAX,
        )
        .expect("build invalid Hub-funded init");
    let invalid_init_block = chain
        .confirm_formal_block_observing_failures(miner)
        .expect("execute invalid Hub-funded init block");
    assert!(
        invalid_init_block
            .receipt(&invalid_init_hash)
            .expect("invalid init receipt")
            .is_error(),
        "a non-zero Hub principal must never initialize an HPAY HVM channel"
    );

    let init = format!(
        "init(0x{}, 0x{}, 7, {}, {}, {}, {}, {}, 100)",
        hex::encode(network),
        hex::encode(channel_id),
        left_address.to_readable(),
        right_address.to_readable(),
        LEFT_DEPOSIT,
        RIGHT_DEPOSIT,
        CHALLENGE_BLOCKS,
    );
    confirm_call(&mut chain, &left, &[&right], &contract, &init, miner);
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"status".to_vec())),
        Value::U8(1)
    );
    confirm_source(
        &mut chain,
        &watchtower,
        &[],
        &contract,
        &renew_all_source(&contract, 100),
        miner,
    );
    let manifest: serde_json::Value = serde_json::from_str(CONTRACT_MANIFEST).unwrap();
    for key in manifest["storage_keys"].as_array().unwrap() {
        let key = key.as_str().unwrap();
        assert_ne!(
            chain.storage(&contract, &Value::bytes(key.as_bytes().to_vec())),
            Value::Nil,
            "{key} must remain active after atomic renewal"
        );
    }

    confirm_deposit(&mut chain, &left, &contract, LEFT_DEPOSIT, miner);
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"status".to_vec())),
        Value::U8(2)
    );
    assert_eq!(
        chain.balance(&contract.to_addr()).to_zhu_u64(),
        Ok(LEFT_DEPOSIT + RIGHT_DEPOSIT)
    );

    let first = Bill::unsigned(
        network,
        contract.to_addr(),
        channel_id,
        7,
        left_address,
        right_address,
        LEFT_DEPOSIT + RIGHT_DEPOSIT,
        CHALLENGE_BLOCKS,
        1,
        450_000,
        150_000,
    )
    .sign(&left, &right);
    let challenge = format!(
        "challenge({}, {}, {}, 0x{}, 0x{})",
        first.serial,
        first.left_balance,
        first.right_balance,
        hex::encode(first.left_sign.serialize()),
        hex::encode(first.right_sign.serialize()),
    );
    let before_challenge = chain.snapshot();
    confirm_call(&mut chain, &watchtower, &[], &contract, &challenge, miner);
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"status".to_vec())),
        Value::U8(3)
    );
    chain.restore(before_challenge);
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"status".to_vec())),
        Value::U8(2)
    );
    confirm_call(&mut chain, &watchtower, &[], &contract, &challenge, miner);
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"status".to_vec())),
        Value::U8(3)
    );

    let latest = Bill::unsigned(
        network,
        contract.to_addr(),
        channel_id,
        7,
        left_address,
        right_address,
        LEFT_DEPOSIT + RIGHT_DEPOSIT,
        CHALLENGE_BLOCKS,
        2,
        LEFT_FINAL,
        RIGHT_FINAL,
    )
    .sign(&left, &right);
    let respond = format!(
        "respond({}, {}, {}, 0x{}, 0x{})",
        latest.serial,
        latest.left_balance,
        latest.right_balance,
        hex::encode(latest.left_sign.serialize()),
        hex::encode(latest.right_sign.serialize()),
    );
    confirm_call(&mut chain, &watchtower, &[], &contract, &respond, miner);
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"serial".to_vec())),
        Value::U64(2)
    );
    let Value::U64(deadline) = chain.storage(&contract, &Value::bytes(b"deadline".to_vec())) else {
        panic!("challenge deadline must be stored as u64")
    };
    chain
        .confirm_empty_formal_blocks_to_height(miner, deadline.saturating_sub(1))
        .unwrap();
    confirm_call(&mut chain, &watchtower, &[], &contract, "finalize()", miner);
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"status".to_vec())),
        Value::U8(4)
    );

    let left_before = chain.balance(&left_address).to_zhu_u64().unwrap();
    let right_before = chain.balance(&right_address).to_zhu_u64().unwrap();
    let left_payout = submit_payout(&mut chain, &watchtower, &contract, left_address, LEFT_FINAL);
    chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&left_payout);

    // The contract still holds enough HAC for this replay, so rejection proves
    // the durable claimed flag is enforced by PermitHAC rather than merely
    // relying on an empty contract balance.
    let replay = submit_payout(&mut chain, &watchtower, &contract, left_address, LEFT_FINAL);
    let failed = chain
        .confirm_formal_block_observing_failures(miner)
        .unwrap();
    failed
        .receipt(&replay)
        .expect("replayed payout receipt")
        .expect_error_contains("HPAY_LEFT_ALREADY_CLAIMED");
    assert_eq!(
        chain.balance(&left_address).to_zhu_u64(),
        Ok(left_before + LEFT_FINAL)
    );

    let right_payout = submit_payout(
        &mut chain,
        &watchtower,
        &contract,
        right_address,
        RIGHT_FINAL,
    );
    chain
        .confirm_formal_block(miner)
        .unwrap()
        .expect_success(&right_payout);
    assert_eq!(
        chain.balance(&left_address).to_zhu_u64(),
        Ok(left_before + LEFT_FINAL)
    );
    assert_eq!(
        chain.balance(&right_address).to_zhu_u64(),
        Ok(right_before + RIGHT_FINAL)
    );
    assert_eq!(chain.balance(&contract.to_addr()).to_zhu_u64(), Ok(0));

    // Value growth can consume lease credit at different rates. Move far
    // enough that at least one key is recoverable, but not absent, then prove
    // a third party can atomically restore all 18 without either channel key.
    let base_height = chain.height();
    chain.set_height(base_height.saturating_add(16_000));
    let mut recoverable_keys = 0usize;
    for key in manifest["storage_keys"].as_array().unwrap() {
        let key = key.as_str().unwrap();
        let debug = VMStateRead::wrap(chain.state())
            .debug_storage_get(
                &vm::rt::GasExtra::new(chain.height()),
                &vm::rt::SpaceCap::new(chain.height()),
                chain.height(),
                &contract.to_addr(),
                &Value::bytes(key.as_bytes().to_vec()),
            )
            .unwrap();
        let debug = debug.unwrap_or_else(|| panic!("{key} must still be recoverable"));
        recoverable_keys += usize::from(debug.recoverable);
    }
    assert!(
        recoverable_keys > 0,
        "the recovery fixture must contain at least one recoverable lease"
    );
    confirm_source(
        &mut chain,
        &watchtower,
        &[],
        &contract,
        &renew_all_source(&contract, 100),
        miner,
    );
    for key in manifest["storage_keys"].as_array().unwrap() {
        let key = key.as_str().unwrap();
        assert_ne!(
            chain.storage(&contract, &Value::bytes(key.as_bytes().to_vec())),
            Value::Nil,
            "{key} must be restored by the atomic recovery renewal"
        );
    }
    assert_eq!(
        chain.storage(&contract, &Value::bytes(b"status".to_vec())),
        Value::U8(4)
    );
}
