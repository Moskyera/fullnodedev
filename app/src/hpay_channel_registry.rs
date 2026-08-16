//! Canonical fullnode snapshot for the reviewed shared HPAY HVM registry.
//! The endpoint proves the exact deployment transaction, constructor binding,
//! bytecode and all registry/channel storage leases from this node's own state.

use basis::interface::ApiExecCtx;
use field::{Address, Hash, Hex};
use protocol::state::CoreStateRead;
use serde_json::{Value, json};
use vm::rt::{GasExtra, SpaceCap};
use vm::value::Value as HvmValue;
use vm::{ContractAddress, VMStateRead};

const MANIFEST: &str = include_str!("../../vm/contracts/hpay_channel_registry_v2.manifest.json");
const SOURCE: &str = include_str!("../../vm/contracts/hpay_channel_registry_v2.fitsh");
const EXPECTED_BYTECODE_SHA3: &str =
    "276d8c205296cc50d06244c84d52c5a9f6f4711e0abae67f416e4fc79c9294be";

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
}
