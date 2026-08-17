//! Canonical fullnode snapshot and deployment evidence for the reviewed shared
//! HPAY HVM registry. Both prove the exact deployment transaction, constructor
//! binding, bytecode and storage leases from this node's own state.
//!
//! This is the V2 sibling of [`crate::hpay_channel_exit`], and it exists
//! because the settlement profile this system actually uses is
//! `hpay-hvm-shared-registry-v2`. A V1 evidence document says nothing about a
//! V2 deployment: it is bound to a different contract, a different protocol
//! domain and a different bytecode, so a Hub that measured V1 would have gone
//! on reading `false` no matter how many registries were deployed.
//!
//! A manifest is never authority by itself. `enabled`,
//! `independently_verified` and the deployment coordinates are all *claims*
//! read out of a checked-in JSON file; every one of them is re-derived here
//! from this node's own block store and state tree before
//! `deployment_verified` can be `true`.

use basis::interface::ApiExecCtx;
use field::{Address, Hash, Hex};
use protocol::state::CoreStateRead;
use serde_json::{Value, json};
use vm::rt::{GasExtra, SpaceCap};
use vm::value::Value as HvmValue;
use vm::{ContractAddress, VMStateRead};

use crate::hpay_channel_exit::MAINNET_MIN_SAFE_HEIGHT;

const MANIFEST: &str = include_str!("../../vm/contracts/hpay_channel_registry_v2.manifest.json");
const SOURCE: &str = include_str!("../../vm/contracts/hpay_channel_registry_v2.fitsh");
const EXPECTED_BYTECODE_SHA3: &str =
    "2fa7429d9e686dd2457eeb1b4476f972c7ddd9be6a0371c9765eff2910209b04";
const EVIDENCE_SCHEMA: &str = "hpay-hvm-channel-registry-exit-evidence/2";

const REGISTRY_KEYS: &[&str] = &[
    "g_network",
    "g_hub",
    "g_locked",
    "g_left_claimable",
    "g_hub_claimable",
    "g_open_count",
];

const CHANNEL_FIELDS: &[(&str, &str)] = &[
    ("status", "c_status_"),
    ("channel_id", "c_id_"),
    ("reuse", "c_reuse_"),
    ("deposit", "c_deposit_"),
    ("paid", "c_paid_"),
    ("total", "c_total_"),
    ("serial", "c_serial_"),
    ("left_balance", "c_left_balance_"),
    ("hub_balance", "c_hub_balance_"),
    ("challenge_blocks", "c_challenge_"),
    ("deadline", "c_deadline_"),
    ("left_claimed", "c_left_claimed_"),
];

fn manifest_valid(manifest: &Value) -> bool {
    let source_sha256 = hex::encode(sys::sha2(SOURCE.as_bytes()));
    manifest["schema"] == "hpay-hvm-channel-registry-manifest/2"
        && manifest["contract_name"] == "HPAYChannelRegistryV2"
        && manifest["protocol_domain"] == "HPAY/HVM-CHANNEL-REGISTRY/V2"
        && manifest["settlement_profile"] == "hpay-hvm-shared-registry-v2"
        && manifest["source_file"] == "hpay_channel_registry_v2.fitsh"
        && manifest["source_sha256"].as_str() == Some(source_sha256.as_str())
        && manifest["bytecode_sha3"] == EXPECTED_BYTECODE_SHA3
        && manifest["required_action_kinds"] == json!([40, 41, 44])
        && manifest["deployment_model"]["scope"] == "one_registry_per_hub_and_network"
        && manifest["deployment_model"]["hub_binding"] == "contract_deploy_main_signer"
        && manifest["deployment_model"]["network_binding"] == "exact_32_byte_constructor_argument"
        && manifest["deployment_model"]["per_channel_contract_deploy"] == false
        && manifest["channel_model"]["maximum_active_channels_per_left_address"] == 1
        && manifest["channel_model"]["first_reuse"] == 0
        && manifest["channel_model"]["right_hub_deposit"] == "exactly_zero"
        && manifest["channel_model"]["left_deposit"] == "positive"
        && manifest["maximum_renewal_step_periods"] == MAX_RENT_STEP
        && manifest["registry_storage_keys"] == json!(REGISTRY_KEYS)
        && manifest["channel_storage_prefixes"]
            == json!(
                CHANNEL_FIELDS
                    .iter()
                    .map(|(_, prefix)| *prefix)
                    .collect::<Vec<_>>()
            )
        && manifest["lease_policy"]["permissionless_registry_renewal"] == true
        && manifest["lease_policy"]["permissionless_channel_renewal"] == true
        && manifest["lease_policy"]["must_renew_every_registry_key"] == true
        && manifest["lease_policy"]["must_renew_every_channel_key"] == true
        && manifest["lease_policy"]["production_watchtower_required"] == true
        && manifest["lease_policy"]["recovery_buffer_seeded_on_funding"] == true
        && manifest["lease_policy"]["lapsed_lease_outcome"]
            == "dormant_and_restorable_by_permissionless_renewal"
}

/// `MAX_RENT_STEP` as the reviewed registry contract declares it. Kept in step
/// with the source by `rent_step_matches_the_reviewed_contract` below rather
/// than by hand, because this exact number drifted once already.
const MAX_RENT_STEP: u64 = 150;

/// Every part of a claimed V2 mainnet deployment that this node re-derived for
/// itself, rather than read out of the manifest.
///
/// Split out so the caller cannot accidentally build the evidence document
/// from the manifest's own claims: each field here came from the block store,
/// the transaction index or the HVM state tree of *this* process.
struct OnChainDerivation {
    observed_height: Option<u64>,
    confirmed_tx_height: Option<u64>,
    contract_code_sha3: Option<String>,
    deployment_action_verified: bool,
    hub_address: Option<String>,
    constructor_network_instance_id: Option<String>,
    node_network_instance_id: Option<String>,
}

impl OnChainDerivation {
    const fn empty() -> Self {
        Self {
            observed_height: None,
            confirmed_tx_height: None,
            contract_code_sha3: None,
            deployment_action_verified: false,
            hub_address: None,
            constructor_network_instance_id: None,
            node_network_instance_id: None,
        }
    }

    /// The registry is bound to one network by an exact 32-byte constructor
    /// argument. This holds only when the bytes the deploying transaction
    /// actually carried are the network instance id *this node computes for
    /// itself*, so a registry deployed against another chain cannot be
    /// presented to this one as its own.
    fn network_binding_matches(&self) -> bool {
        self.constructor_network_instance_id.is_some()
            && self.constructor_network_instance_id == self.node_network_instance_id
    }

    /// Does the code actually living at the claimed address hash to the code
    /// the manifest names?
    ///
    /// Both sides must be present. Written as an explicit match rather than
    /// `Option == Option` because that form answers `true` for `None == None` —
    /// two absences agreeing with each other — which is the one answer this
    /// question must never give. Nothing reaches that today (the manifest
    /// always names a bytecode hash, so the right side is always `Some`), but
    /// it is one deleted manifest line away from publishing
    /// `contract_code_matches: true` for a node that never read a contract.
    fn contract_code_matches(&self, expected_sha3: Option<&str>) -> bool {
        match (self.contract_code_sha3.as_deref(), expected_sha3) {
            (Some(live), Some(expected)) => live == expected,
            _ => false,
        }
    }
}

/// The whole conjunction that has to hold before this node will say a reviewed
/// V2 registry is deployed and verified on Hacash mainnet.
///
/// The manifest supplies only the coordinates to check. Every term below is
/// either a manifest *consistency* check or a comparison against something
/// this node derived itself; none of them can be satisfied by editing the
/// manifest alone, because the moment the manifest names a contract address
/// and a transaction hash, this node goes and reads that block.
fn deployment_verified(
    manifest_valid: bool,
    deployment: &Value,
    expected_code_sha3: Option<&str>,
    chain_id: Option<u32>,
    derivation: &OnChainDerivation,
) -> bool {
    let Some(contract_address) = deployment["contract_address"].as_str() else {
        return false;
    };
    let Some(deployment_tx_hash) = deployment["deployment_tx_hash"].as_str() else {
        return false;
    };
    let Some(deployment_height) = deployment["deployment_height"].as_u64() else {
        return false;
    };
    let contract_address_valid = Address::from_readable(contract_address)
        .ok()
        .and_then(|address| ContractAddress::from_addr(address).ok())
        .is_some();
    manifest_valid
        && deployment["enabled"].as_bool() == Some(true)
        && deployment["independently_verified"].as_bool() == Some(true)
        && contract_address_valid
        && Hash::from_hex(deployment_tx_hash.as_bytes()).is_ok()
        && deployment_height >= MAINNET_MIN_SAFE_HEIGHT
        && derivation
            .observed_height
            .is_some_and(|observed| observed >= deployment_height)
        && chain_id == Some(protocol::upgrade::MAINNET_CHAIN_ID)
        && derivation.confirmed_tx_height == Some(deployment_height)
        && derivation.contract_code_matches(expected_code_sha3)
        // V2-only, and the reason a V1 document could never stand in for this
        // one: the registry is one contract shared by one Hub on one network,
        // so the deploying transaction itself has to be the artifact and the
        // binding, not merely a transaction that happens to exist.
        && derivation.deployment_action_verified
        && derivation.hub_address.is_some()
        && derivation.network_binding_matches()
}

/// Fail-closed evidence about the reviewed shared registry artifact, and about
/// whether it is deployed on Hacash mainnet.
///
/// With `ctx` absent — the offline/self-description case — every derived term
/// is `None`/`false` and `deployment_verified` is `false`, because a node that
/// has not looked at a chain has not verified anything.
pub(crate) fn evidence(ctx: Option<&ApiExecCtx>) -> Value {
    let Ok(manifest) = serde_json::from_str::<Value>(MANIFEST) else {
        return json!({
            "schema": EVIDENCE_SCHEMA,
            "manifest_valid": false,
            "deployment_verified": false,
        });
    };
    let deployment = &manifest["mainnet_deployment"];
    let manifest_valid = manifest_valid(&manifest);
    let contract_address = deployment["contract_address"].as_str();
    let deployment_tx_hash = deployment["deployment_tx_hash"].as_str();
    let deployment_height = deployment["deployment_height"].as_u64();

    let mut derivation = OnChainDerivation::empty();
    if let (Some(ctx), Some(address), Some(tx_hash), Some(height)) =
        (ctx, contract_address, deployment_tx_hash, deployment_height)
    {
        derivation.observed_height = Some(ctx.engine.latest_block().height().uint());
        derivation.node_network_instance_id =
            crate::node_api::current_network_instance(ctx).map(|network| network.instance_id);
        let state = ctx.engine.state();
        let parsed_tx_hash = Hash::from_hex(tx_hash.as_bytes()).ok();
        if let Some(hash) = parsed_tx_hash.as_ref() {
            let core = CoreStateRead::wrap(state.as_ref().as_ref());
            derivation.confirmed_tx_height = core.tx_exist(hash).map(|height| height.uint());
        }
        let parsed_contract = Address::from_readable(address)
            .ok()
            .and_then(|address| ContractAddress::from_addr(address).ok());
        if let Some(contract) = parsed_contract.as_ref() {
            let hvm = VMStateRead::wrap(state.as_ref().as_ref());
            derivation.contract_code_sha3 = hvm
                .contract_edition(contract)
                .map(|edition| edition.hash.to_hex());
        }
        // Re-read the deploying block. This is what makes the document
        // evidence rather than a restatement of the manifest: the Hub binding
        // is the transaction's own main signer and the network binding is the
        // constructor argument that transaction actually carried.
        if let (Some(hash), Some(contract)) = (parsed_tx_hash.as_ref(), parsed_contract.as_ref())
            && let Some(verified) = crate::hpay_contract_deployment::verify_contract_deployment(
                ctx,
                hash,
                height,
                contract,
                EXPECTED_BYTECODE_SHA3,
            )
        {
            derivation.deployment_action_verified = true;
            derivation.hub_address = Some(verified.main_address.to_readable());
            derivation.constructor_network_instance_id =
                Some(hex::encode(&verified.construct_argv));
        }
    }

    // The deployment block is *rebuilt* here, key by key, rather than handed
    // through from the manifest.
    //
    // `"deployment": deployment` used to publish the manifest's own object
    // verbatim, which made a checked-in JSON file part of the wire schema: the
    // Hub parses this document with `deny_unknown_fields`, so one extra key
    // added to the manifest in this repository would fail the Hub's whole
    // capability probe and stop the bounded mainnet pilot taking payments —
    // from a file edit that has nothing to do with any deployment. Naming the
    // six fields the schema defines makes the shape fixed here and the manifest
    // purely a source of *values*.
    //
    // Every value is also normalised to its declared type, so a mistyped entry
    // degrades to `null`/`false` — the honest "no coordinate" answer, which
    // `deployment_verified` already treats as not deployed — instead of
    // travelling as a type the consumer must reject. Nothing here can turn a
    // claim into a verification: all three flags below are manifest claims, and
    // every one of them is re-derived before `deployment_verified` can be true.
    let deployment_document = json!({
        "enabled": deployment["enabled"].as_bool().unwrap_or(false),
        "contract_address": contract_address,
        "deployment_tx_hash": deployment_tx_hash,
        "deployment_height": deployment_height,
        "independently_verified":
            deployment["independently_verified"].as_bool().unwrap_or(false),
        "external_audit_complete":
            deployment["external_audit_complete"].as_bool().unwrap_or(false),
    });
    let deployment_tx_confirmed =
        deployment_height.is_some() && derivation.confirmed_tx_height == deployment_height;
    let contract_code_matches =
        derivation.contract_code_matches(manifest["bytecode_sha3"].as_str());
    // Weighed over the same normalised block that is published, so the verdict
    // and the coordinates a reader checks it against can never be two different
    // things.
    let deployment_verified = deployment_verified(
        manifest_valid,
        &deployment_document,
        manifest["bytecode_sha3"].as_str(),
        ctx.map(|ctx| ctx.engine.config().chain_id),
        &derivation,
    );
    json!({
        "schema": EVIDENCE_SCHEMA,
        "manifest_valid": manifest_valid,
        "contract_name": manifest["contract_name"],
        "protocol_domain": manifest["protocol_domain"],
        "settlement_profile": manifest["settlement_profile"],
        "source_sha256": manifest["source_sha256"],
        "bytecode_sha3": manifest["bytecode_sha3"],
        "required_action_kinds": manifest["required_action_kinds"],
        "channel_model": {
            "left_deposit": manifest["channel_model"]["left_deposit"],
            "right_hub_deposit": manifest["channel_model"]["right_hub_deposit"],
            "maximum_active_channels_per_left_address":
                manifest["channel_model"]["maximum_active_channels_per_left_address"],
            "first_reuse": manifest["channel_model"]["first_reuse"],
        },
        "registry_key_count": manifest["registry_storage_keys"].as_array().map(Vec::len),
        "channel_key_count": manifest["channel_storage_prefixes"].as_array().map(Vec::len),
        "must_renew_every_registry_key":
            manifest["lease_policy"]["must_renew_every_registry_key"],
        "must_renew_every_channel_key": manifest["lease_policy"]["must_renew_every_channel_key"],
        "maximum_renewal_step_periods": manifest["maximum_renewal_step_periods"],
        "deployment": deployment_document,
        "on_chain_verification": {
            "observed_height": derivation.observed_height,
            "confirmed_tx_height": derivation.confirmed_tx_height,
            "deployment_tx_confirmed": deployment_tx_confirmed,
            "contract_code_sha3": derivation.contract_code_sha3,
            "contract_code_matches": contract_code_matches,
            "deployment_action_verified": derivation.deployment_action_verified,
            "hub_address": derivation.hub_address,
            "constructor_network_instance_id": derivation.constructor_network_instance_id,
            "node_network_instance_id": derivation.node_network_instance_id,
            "network_binding_matches": derivation.network_binding_matches(),
        },
        "deployment_verified": deployment_verified,
    })
}

fn channel_key(prefix: &str, left: &Address) -> HvmValue {
    let mut key = prefix.as_bytes().to_vec();
    key.extend_from_slice(left.as_bytes());
    HvmValue::bytes(key)
}

fn debug_entry(
    hvm: &VMStateRead,
    gas: &GasExtra,
    cap: &SpaceCap,
    evaluation_height: u64,
    contract: &ContractAddress,
    key: HvmValue,
    label: &str,
    value: impl FnOnce(&HvmValue) -> Result<Value, String>,
) -> Result<(Value, u64, u64, bool), String> {
    let debug = hvm
        .debug_storage_get(gas, cap, evaluation_height, &contract.to_addr(), &key)
        .map_err(|_| format!("HPAY HVM registry storage {label} cannot be read"))?
        .ok_or_else(|| format!("HPAY HVM registry storage {label} is missing"))?;
    Ok((
        json!({
            "value": value(&debug.value)?,
            "live_blocks": debug.live_blocks,
            "recover_blocks": debug.recover_blocks,
            "active": debug.active,
            "recoverable": debug.recoverable,
        }),
        debug.live_blocks,
        debug.recover_blocks,
        debug.active,
    ))
}

fn registry_value(key: &str, value: &HvmValue) -> Result<Value, String> {
    let invalid = || format!("HPAY HVM registry key {key} has an unexpected type");
    match key {
        "g_network" => match value {
            HvmValue::Bytes(bytes) if bytes.len() == 32 => Ok(Value::from(hex::encode(bytes))),
            _ => Err(invalid()),
        },
        "g_hub" => value
            .extract_address()
            .map(|address| Value::from(address.to_readable()))
            .map_err(|_| invalid()),
        _ => match value {
            HvmValue::U64(value) => Ok(Value::from(*value)),
            _ => Err(invalid()),
        },
    }
}

fn channel_value(field: &str, value: &HvmValue) -> Result<Value, String> {
    let invalid = || format!("HPAY HVM registry channel field {field} has an unexpected type");
    match field {
        "status" => match value {
            HvmValue::U8(value) => Ok(Value::from(*value)),
            _ => Err(invalid()),
        },
        "channel_id" => match value {
            HvmValue::Bytes(bytes) if bytes.len() == 16 => Ok(Value::from(hex::encode(bytes))),
            _ => Err(invalid()),
        },
        "reuse" => match value {
            HvmValue::U32(value) => Ok(Value::from(*value)),
            _ => Err(invalid()),
        },
        "left_claimed" => match value {
            HvmValue::Bool(value) => Ok(Value::from(*value)),
            _ => Err(invalid()),
        },
        _ => match value {
            HvmValue::U64(value) => Ok(Value::from(*value)),
            _ => Err(invalid()),
        },
    }
}

fn validate_channel_invariants(channel: &serde_json::Map<String, Value>) -> Result<(), String> {
    let value = |name: &str| {
        channel
            .get(name)
            .and_then(|entry| entry["value"].as_u64())
            .ok_or_else(|| format!("HPAY HVM registry channel field {name} is invalid"))
    };
    let status = value("status")?;
    let deposit = value("deposit")?;
    let paid = value("paid")?;
    let total = value("total")?;
    let left = value("left_balance")?;
    let hub = value("hub_balance")?;
    if !(1..=4).contains(&status) || deposit == 0 || total != deposit {
        return Err("HPAY HVM registry channel funding invariant failed".into());
    }
    if !matches!(paid, 0) && paid != deposit {
        return Err("HPAY HVM registry channel paid amount is invalid".into());
    }
    if status == 1 && paid != 0 {
        return Err("HPAY HVM registry funding channel already reports a deposit".into());
    }
    if status >= 2 && paid != deposit {
        return Err("HPAY HVM registry live channel is not exactly funded".into());
    }
    if left.checked_add(hub) != Some(total) {
        return Err("HPAY HVM registry channel balances do not conserve the deposit".into());
    }
    Ok(())
}

pub(crate) fn channel_snapshot(
    ctx: &ApiExecCtx,
    contract_address: &str,
    deployment_tx_hash: &str,
    deployment_height: u64,
    left_address: &str,
    expected_network_instance_id: &str,
) -> Result<Value, String> {
    let config = ctx.engine.config();
    let observed_height = ctx.engine.latest_block().height().uint();
    if !crate::hpay_channel_exit::snapshot_network_allowed(
        config.chain_id,
        observed_height,
        deployment_height,
    ) {
        return Err(
            "HPAY HVM registry snapshot does not match the requested chain and deployment height"
                .into(),
        );
    }
    if expected_network_instance_id.len() != 64
        || !expected_network_instance_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("HPAY HVM registry network instance is invalid".into());
    }
    let expected_network = hex::decode(expected_network_instance_id)
        .map_err(|_| "HPAY HVM registry network instance is invalid".to_owned())?;
    let contract = Address::from_readable(contract_address)
        .map_err(|_| "HPAY HVM registry contract address is invalid".to_owned())
        .and_then(|address| {
            ContractAddress::from_addr(address)
                .map_err(|_| "HPAY HVM registry address is not a contract".to_owned())
        })?;
    let left = Address::from_readable(left_address)
        .map_err(|_| "HPAY HVM registry left address is invalid".to_owned())?;
    let deployment_hash = Hash::from_hex(deployment_tx_hash.as_bytes())
        .map_err(|_| "HPAY HVM registry deployment transaction hash is invalid".to_owned())?;
    let state = ctx.engine.state();
    let core = CoreStateRead::wrap(state.as_ref().as_ref());
    if core.tx_exist(&deployment_hash).map(|height| height.uint()) != Some(deployment_height) {
        return Err(
            "HPAY HVM registry deployment transaction is not included at the bound height".into(),
        );
    }
    let deployment = crate::hpay_contract_deployment::verify_contract_deployment(
        ctx,
        &deployment_hash,
        deployment_height,
        &contract,
        EXPECTED_BYTECODE_SHA3,
    )
    .ok_or_else(|| {
        "HPAY HVM registry deployment does not contain the exact reviewed artifact".to_owned()
    })?;
    if deployment.construct_argv != expected_network {
        return Err("HPAY HVM registry constructor is not bound to this network instance".into());
    }

    let manifest: Value =
        serde_json::from_str(MANIFEST).map_err(|_| "HPAY HVM registry manifest is invalid")?;
    if !manifest_valid(&manifest) {
        return Err("HPAY HVM registry manifest does not match the reviewed artifact".into());
    }
    let hvm = VMStateRead::wrap(state.as_ref().as_ref());
    let code_sha3 = hvm
        .contract_edition(&contract)
        .map(|edition| edition.hash.to_hex())
        .ok_or_else(|| "HPAY HVM registry contract does not exist".to_owned())?;
    if code_sha3 != EXPECTED_BYTECODE_SHA3 {
        return Err("HPAY HVM registry bytecode does not match the reviewed artifact".into());
    }

    let evaluation_height = observed_height
        .checked_add(1)
        .ok_or_else(|| "HPAY HVM registry snapshot height overflow".to_owned())?;
    let gas = GasExtra::new(evaluation_height);
    let cap = SpaceCap::new(evaluation_height);
    let mut registry = serde_json::Map::new();
    let mut channel = serde_json::Map::new();
    let mut minimum_live_blocks = u64::MAX;
    let mut minimum_recover_blocks = u64::MAX;
    let mut all_keys_active = true;
    for key in REGISTRY_KEYS {
        let (entry, live, recover, active) = debug_entry(
            &hvm,
            &gas,
            &cap,
            evaluation_height,
            &contract,
            HvmValue::bytes(key.as_bytes().to_vec()),
            key,
            |value| registry_value(key, value),
        )?;
        minimum_live_blocks = minimum_live_blocks.min(live);
        minimum_recover_blocks = minimum_recover_blocks.min(recover);
        all_keys_active &= active;
        registry.insert((*key).to_owned(), entry);
    }
    for (field, prefix) in CHANNEL_FIELDS {
        let (entry, live, recover, active) = debug_entry(
            &hvm,
            &gas,
            &cap,
            evaluation_height,
            &contract,
            channel_key(prefix, &left),
            field,
            |value| channel_value(field, value),
        )?;
        minimum_live_blocks = minimum_live_blocks.min(live);
        minimum_recover_blocks = minimum_recover_blocks.min(recover);
        all_keys_active &= active;
        channel.insert((*field).to_owned(), entry);
    }
    if registry["g_network"]["value"].as_str() != Some(expected_network_instance_id) {
        return Err("HPAY HVM registry storage network binding is invalid".into());
    }
    if registry["g_hub"]["value"].as_str() != Some(deployment.main_address.to_readable().as_str()) {
        return Err("HPAY HVM registry Hub does not match the deployment signer".into());
    }
    if left == deployment.main_address {
        return Err("HPAY HVM registry left address cannot be the Hub".into());
    }
    validate_channel_invariants(&channel)?;

    Ok(json!({
        "ret": 0,
        "schema": "hpay-hvm-channel-registry-live-snapshot/2",
        "settlement_profile": "hpay-hvm-shared-registry-v2",
        "chain_id": config.chain_id,
        "network_instance_id": expected_network_instance_id,
        "observed_height": observed_height,
        "evaluation_height": evaluation_height,
        "contract_address": contract.to_readable(),
        "deployment_tx_hash": deployment_hash.to_hex(),
        "deployment_height": deployment_height,
        "deployment_action_verified": true,
        "bytecode_sha3": code_sha3,
        "hub_address": deployment.main_address.to_readable(),
        "left_address": left.to_readable(),
        "registry_key_count": REGISTRY_KEYS.len(),
        "channel_key_count": CHANNEL_FIELDS.len(),
        "all_keys_active": all_keys_active,
        "minimum_live_blocks": minimum_live_blocks,
        "minimum_recover_blocks": minimum_recover_blocks,
        "registry": registry,
        "channel": channel,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_inventory_and_hashes_are_exact() {
        let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
        assert!(manifest_valid(&manifest));
        assert_eq!(manifest["registry_storage_keys"], json!(REGISTRY_KEYS));
        assert_eq!(
            manifest["channel_storage_prefixes"],
            json!(
                CHANNEL_FIELDS
                    .iter()
                    .map(|(_, prefix)| *prefix)
                    .collect::<Vec<_>>()
            )
        );
        let compiled = vm::fitshc::compile(SOURCE).unwrap().0.serialize();
        assert_eq!(hex::encode(sys::sha3(compiled)), EXPECTED_BYTECODE_SHA3);
    }

    #[test]
    fn typed_storage_and_conservation_fail_closed() {
        assert_eq!(
            registry_value("g_locked", &HvmValue::U64(3)).unwrap(),
            json!(3)
        );
        assert!(registry_value("g_locked", &HvmValue::U128(3)).is_err());
        assert_eq!(
            channel_value("channel_id", &HvmValue::Bytes(vec![1; 16])).unwrap(),
            json!("01010101010101010101010101010101")
        );
        assert!(channel_value("channel_id", &HvmValue::Bytes(vec![1; 15])).is_err());

        let mut channel = serde_json::Map::new();
        for (key, value) in [
            ("status", json!({"value": 2})),
            ("deposit", json!({"value": 100})),
            ("paid", json!({"value": 100})),
            ("total", json!({"value": 100})),
            ("left_balance", json!({"value": 60})),
            ("hub_balance", json!({"value": 40})),
        ] {
            channel.insert(key.into(), value);
        }
        assert!(validate_channel_invariants(&channel).is_ok());
        channel["hub_balance"]["value"] = json!(41);
        assert!(validate_channel_invariants(&channel).is_err());
    }

    /// The one number in this file that lives in another repository too.
    /// Parsed back out of the reviewed contract so it cannot drift by hand.
    #[test]
    fn rent_step_matches_the_reviewed_contract() {
        let declared = SOURCE
            .lines()
            .find_map(|line| line.trim().strip_prefix("const MAX_RENT_STEP"))
            .and_then(|rest| rest.trim().strip_prefix('='))
            .and_then(|rest| rest.trim().parse::<u64>().ok())
            .expect("the reviewed registry contract declares const MAX_RENT_STEP");
        assert_eq!(declared, MAX_RENT_STEP);
        let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
        assert_eq!(manifest["maximum_renewal_step_periods"], MAX_RENT_STEP);
    }

    /// Nothing is deployed, so the honest answer is `false` — and it is false
    /// for the right reasons: the manifest is valid and the artifact hashes
    /// are exact, but there is no deployment to verify.
    #[test]
    fn undeployed_registry_evidence_is_exact_about_the_artifact_and_false_about_the_chain() {
        let value = evidence(None);
        assert_eq!(value["schema"], EVIDENCE_SCHEMA);
        assert_eq!(value["manifest_valid"], true);
        assert_eq!(value["contract_name"], "HPAYChannelRegistryV2");
        assert_eq!(value["settlement_profile"], "hpay-hvm-shared-registry-v2");
        assert_eq!(value["protocol_domain"], "HPAY/HVM-CHANNEL-REGISTRY/V2");
        assert_eq!(value["bytecode_sha3"], EXPECTED_BYTECODE_SHA3);
        assert_eq!(value["registry_key_count"], REGISTRY_KEYS.len());
        assert_eq!(value["channel_key_count"], CHANNEL_FIELDS.len());
        assert_eq!(value["must_renew_every_registry_key"], true);
        assert_eq!(value["must_renew_every_channel_key"], true);
        assert_eq!(value["maximum_renewal_step_periods"], MAX_RENT_STEP);
        assert_eq!(value["deployment"]["enabled"], false);
        assert_eq!(value["deployment"]["independently_verified"], false);
        assert_eq!(
            value["on_chain_verification"]["deployment_action_verified"],
            false
        );
        assert_eq!(
            value["on_chain_verification"]["network_binding_matches"],
            false
        );
        assert_eq!(value["deployment_verified"], false);
    }

    fn deployed_manifest_claim() -> Value {
        json!({
            "enabled": true,
            "contract_address": Address::create_contract([9_u8; 20]).to_readable(),
            "deployment_tx_hash": "aa".repeat(32),
            "deployment_height": MAINNET_MIN_SAFE_HEIGHT + 10,
            "independently_verified": true,
            "external_audit_complete": false,
        })
    }

    fn full_derivation() -> OnChainDerivation {
        OnChainDerivation {
            observed_height: Some(MAINNET_MIN_SAFE_HEIGHT + 40),
            confirmed_tx_height: Some(MAINNET_MIN_SAFE_HEIGHT + 10),
            contract_code_sha3: Some(EXPECTED_BYTECODE_SHA3.to_owned()),
            deployment_action_verified: true,
            hub_address: Some(Address::create_contract([3_u8; 20]).to_readable()),
            constructor_network_instance_id: Some("bb".repeat(32)),
            node_network_instance_id: Some("bb".repeat(32)),
        }
    }

    fn verified_with(derivation: &OnChainDerivation) -> bool {
        deployment_verified(
            true,
            &deployed_manifest_claim(),
            Some(EXPECTED_BYTECODE_SHA3),
            Some(protocol::upgrade::MAINNET_CHAIN_ID),
            derivation,
        )
    }

    /// A manifest claiming a deployment is not a deployment. Every term the
    /// node derives for itself is load bearing: drop any one and the answer
    /// goes back to `false`.
    #[test]
    fn manifest_claims_alone_never_verify_a_registry_deployment() {
        assert!(verified_with(&full_derivation()));

        let mut wrong_chain = deployment_verified(
            true,
            &deployed_manifest_claim(),
            Some(EXPECTED_BYTECODE_SHA3),
            Some(7),
            &full_derivation(),
        );
        assert!(!wrong_chain, "a non-mainnet chain must never verify");
        wrong_chain = deployment_verified(
            true,
            &deployed_manifest_claim(),
            Some(EXPECTED_BYTECODE_SHA3),
            None,
            &full_derivation(),
        );
        assert!(!wrong_chain, "an unprobed chain must never verify");

        let mut derivation = full_derivation();
        derivation.deployment_action_verified = false;
        assert!(
            !verified_with(&derivation),
            "a transaction that does not deploy this artifact must never verify"
        );

        let mut derivation = full_derivation();
        derivation.constructor_network_instance_id = Some("cc".repeat(32));
        assert!(
            !verified_with(&derivation),
            "a registry constructed for another network must never verify"
        );

        let mut derivation = full_derivation();
        derivation.node_network_instance_id = None;
        assert!(
            !verified_with(&derivation),
            "a node that cannot identify its own network must never verify"
        );

        let mut derivation = full_derivation();
        derivation.hub_address = None;
        assert!(!verified_with(&derivation));

        let mut derivation = full_derivation();
        derivation.contract_code_sha3 = Some("ff".repeat(32));
        assert!(
            !verified_with(&derivation),
            "live code that is not the reviewed bytecode must never verify"
        );

        let mut derivation = full_derivation();
        derivation.confirmed_tx_height = Some(MAINNET_MIN_SAFE_HEIGHT + 11);
        assert!(!verified_with(&derivation));

        let mut derivation = full_derivation();
        derivation.observed_height = Some(MAINNET_MIN_SAFE_HEIGHT + 9);
        assert!(!verified_with(&derivation));

        assert!(
            !deployment_verified(
                false,
                &deployed_manifest_claim(),
                Some(EXPECTED_BYTECODE_SHA3),
                Some(protocol::upgrade::MAINNET_CHAIN_ID),
                &full_derivation(),
            ),
            "an invalid manifest must never verify"
        );

        let mut below_floor = deployed_manifest_claim();
        below_floor["deployment_height"] = json!(MAINNET_MIN_SAFE_HEIGHT - 1);
        let mut derivation = full_derivation();
        derivation.confirmed_tx_height = Some(MAINNET_MIN_SAFE_HEIGHT - 1);
        assert!(
            !deployment_verified(
                true,
                &below_floor,
                Some(EXPECTED_BYTECODE_SHA3),
                Some(protocol::upgrade::MAINNET_CHAIN_ID),
                &derivation,
            ),
            "a deployment below the pinned mainnet checkpoint must never verify"
        );

        let mut not_enabled = deployed_manifest_claim();
        not_enabled["enabled"] = json!(false);
        assert!(!deployment_verified(
            true,
            &not_enabled,
            Some(EXPECTED_BYTECODE_SHA3),
            Some(protocol::upgrade::MAINNET_CHAIN_ID),
            &full_derivation(),
        ));
    }

    /// Two absences do not agree with each other.
    ///
    /// `contract_code_matches` used to be `Option == Option`, which answers
    /// `true` when a node read no contract *and* the manifest named no
    /// bytecode. It is unreachable today because the manifest always names one
    /// — but it is exactly the shape that publishes `contract_code_matches:
    /// true` beside `contract_code_sha3: null`, and a consumer reading the
    /// boolean would be told the code on chain is the reviewed code by a node
    /// that never looked at a chain.
    #[test]
    fn two_absent_code_hashes_never_count_as_a_match() {
        let empty = OnChainDerivation::empty();
        assert!(
            !empty.contract_code_matches(None),
            "no live code and no expected code is not a match"
        );
        assert!(!empty.contract_code_matches(Some(EXPECTED_BYTECODE_SHA3)));

        let derivation = full_derivation();
        assert!(
            !derivation.contract_code_matches(None),
            "live code with nothing to compare it against is not a match"
        );
        assert!(derivation.contract_code_matches(Some(EXPECTED_BYTECODE_SHA3)));
        assert!(!derivation.contract_code_matches(Some(&"ff".repeat(32))));

        // And the same conjunction inside `deployment_verified`, which used to
        // guard this with a separate `expected_code_sha3.is_some()` term.
        assert!(!deployment_verified(
            true,
            &deployed_manifest_claim(),
            None,
            Some(protocol::upgrade::MAINNET_CHAIN_ID),
            &full_derivation(),
        ));
    }

    /// The evidence document's shape is defined here, not in a JSON file.
    ///
    /// The Hub parses this block with `deny_unknown_fields`. When it was the
    /// manifest's own object, republished verbatim, adding one key to a
    /// checked-in file in this repository would have failed the Hub's entire
    /// capability probe — taking the bounded mainnet pilot's `payments_enabled`
    /// and `close_enabled` down with it, from an edit about nothing.
    #[test]
    fn the_published_deployment_block_is_this_files_schema_not_the_manifests() {
        let value = evidence(None);
        let deployment = value["deployment"]
            .as_object()
            .expect("the deployment block must be an object");
        let mut keys = deployment.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "contract_address".to_owned(),
                "deployment_height".to_owned(),
                "deployment_tx_hash".to_owned(),
                "enabled".to_owned(),
                "external_audit_complete".to_owned(),
                "independently_verified".to_owned(),
            ],
            "exactly the six fields the evidence schema declares, and no others"
        );

        // Every claim flag is a strict boolean and every coordinate is either
        // its declared type or null, whatever the manifest happens to hold.
        for flag in [
            "enabled",
            "independently_verified",
            "external_audit_complete",
        ] {
            assert!(
                deployment[flag].is_boolean(),
                "{flag} must always be a boolean on the wire"
            );
        }
        for coordinate in ["contract_address", "deployment_tx_hash"] {
            assert!(deployment[coordinate].is_string() || deployment[coordinate].is_null());
        }
        assert!(
            deployment["deployment_height"].is_u64() || deployment["deployment_height"].is_null()
        );
    }
}
