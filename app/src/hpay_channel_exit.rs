//! Canonical, fail-closed evidence for the reviewed HPAY HVM channel-exit
//! candidate. A manifest is never authority by itself: deployment evidence is
//! derived from this node's own mainnet state.

use basis::interface::ApiExecCtx;
use field::{Address, Hash, Hex};
use protocol::state::CoreStateRead;
use serde_json::{Value, json};
use vm::rt::{GasExtra, SpaceCap};
use vm::value::Value as HvmValue;
use vm::{ContractAddress, VMStateRead};

pub(crate) const MAINNET_MIN_SAFE_HEIGHT: u64 = 765_432;
const MANIFEST: &str = include_str!("../../vm/contracts/hpay_channel_exit_v1.manifest.json");
const SOURCE: &str = include_str!("../../vm/contracts/hpay_channel_exit_v1.fitsh");

const STORAGE_KEYS: &[&str] = &[
    "status",
    "network",
    "channel_id",
    "reuse",
    "left",
    "right",
    "left_deposit",
    "right_deposit",
    "left_paid",
    "right_paid",
    "total",
    "serial",
    "left_balance",
    "right_balance",
    "challenge_blocks",
    "deadline",
    "left_claimed",
    "right_claimed",
];

pub(crate) fn snapshot_network_allowed(
    chain_id: u32,
    observed_height: u64,
    deployment_height: u64,
) -> bool {
    if deployment_height == 0 || deployment_height > observed_height {
        return false;
    }
    chain_id != protocol::upgrade::MAINNET_CHAIN_ID
        || (observed_height >= MAINNET_MIN_SAFE_HEIGHT
            && deployment_height >= MAINNET_MIN_SAFE_HEIGHT)
}

fn manifest_valid(manifest: &Value) -> bool {
    let source_sha256 = hex::encode(sys::sha2(SOURCE.as_bytes()));
    manifest["schema"] == "hpay-hvm-channel-exit-manifest/1"
        && manifest["contract_name"] == "HPAYChannelExitV1"
        && manifest["protocol_domain"] == "HPAY/HVM-CHANNEL/V1"
        && manifest["settlement_profile"] == "hpay-hvm-channel-v1"
        && manifest["source_file"] == "hpay_channel_exit_v1.fitsh"
        && manifest["source_sha256"].as_str() == Some(source_sha256.as_str())
        && manifest["bytecode_sha3"]
            == "11a2efc27a0c951bbc6977186eb58bd076dd331a785f3c57242cf54a72238349"
        && manifest["required_action_kinds"] == json!([40, 41, 44])
        && manifest["funding_model"]["left_deposit"] == "positive"
        && manifest["funding_model"]["right_hub_deposit"] == "exactly_zero"
        && manifest["storage_keys"]
            == json!([
                "status",
                "network",
                "channel_id",
                "reuse",
                "left",
                "right",
                "left_deposit",
                "right_deposit",
                "left_paid",
                "right_paid",
                "total",
                "serial",
                "left_balance",
                "right_balance",
                "challenge_blocks",
                "deadline",
                "left_claimed",
                "right_claimed"
            ])
        && manifest["lease_policy"]["permissionless_renewal"] == true
        && manifest["lease_policy"]["must_renew_every_storage_key"] == true
        && manifest["lease_policy"]["production_watchtower_required"] == true
}

pub(crate) fn deployment_verified(
    manifest_valid: bool,
    deployment: &Value,
    expected_code_sha3: Option<&str>,
    chain_id: Option<u32>,
    observed_height: Option<u64>,
    confirmed_tx_height: Option<u64>,
    contract_code_sha3: Option<&str>,
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
        && observed_height.is_some_and(|observed| observed >= deployment_height)
        && chain_id == Some(protocol::upgrade::MAINNET_CHAIN_ID)
        && confirmed_tx_height == Some(deployment_height)
        && expected_code_sha3.is_some()
        && contract_code_sha3 == expected_code_sha3
}

pub(crate) fn evidence(ctx: Option<&ApiExecCtx>) -> Value {
    let Ok(manifest) = serde_json::from_str::<Value>(MANIFEST) else {
        return json!({
            "schema": "hpay-hvm-channel-exit-evidence/1",
            "manifest_valid": false,
            "deployment_verified": false,
        });
    };
    let deployment = &manifest["mainnet_deployment"];
    let manifest_valid = manifest_valid(&manifest);
    let contract_address = deployment["contract_address"].as_str();
    let deployment_tx_hash = deployment["deployment_tx_hash"].as_str();
    let deployment_height = deployment["deployment_height"].as_u64();

    let mut observed_height = None;
    let mut confirmed_tx_height = None;
    let mut contract_code_sha3 = None;
    if let (Some(ctx), Some(address), Some(tx_hash)) = (ctx, contract_address, deployment_tx_hash) {
        observed_height = Some(ctx.engine.latest_block().height().uint());
        let state = ctx.engine.state();
        if let Ok(hash) = Hash::from_hex(tx_hash.as_bytes()) {
            let core = protocol::state::CoreStateRead::wrap(state.as_ref().as_ref());
            confirmed_tx_height = core.tx_exist(&hash).map(|height| height.uint());
        }
        if let Ok(address) = Address::from_readable(address)
            && let Ok(contract) = ContractAddress::from_addr(address)
        {
            let hvm = VMStateRead::wrap(state.as_ref().as_ref());
            contract_code_sha3 = hvm
                .contract_edition(&contract)
                .map(|edition| edition.hash.to_hex());
        }
    }

    let deployment_tx_confirmed =
        deployment_height.is_some() && confirmed_tx_height == deployment_height;
    let contract_code_matches = contract_code_sha3.as_deref() == manifest["bytecode_sha3"].as_str();
    let deployment_verified = deployment_verified(
        manifest_valid,
        deployment,
        manifest["bytecode_sha3"].as_str(),
        ctx.map(|ctx| ctx.engine.config().chain_id),
        observed_height,
        confirmed_tx_height,
        contract_code_sha3.as_deref(),
    );
    json!({
        "schema": "hpay-hvm-channel-exit-evidence/1",
        "manifest_valid": manifest_valid,
        "contract_name": manifest["contract_name"],
        "protocol_domain": manifest["protocol_domain"],
        "settlement_profile": manifest["settlement_profile"],
        "source_sha256": manifest["source_sha256"],
        "bytecode_sha3": manifest["bytecode_sha3"],
        "required_action_kinds": manifest["required_action_kinds"],
        "funding_model": manifest["funding_model"],
        "storage_key_count": manifest["storage_keys"].as_array().map(Vec::len),
        "must_renew_every_storage_key": manifest["lease_policy"]["must_renew_every_storage_key"],
        "deployment": deployment,
        "on_chain_verification": {
            "observed_height": observed_height,
            "confirmed_tx_height": confirmed_tx_height,
            "deployment_tx_confirmed": deployment_tx_confirmed,
            "contract_code_sha3": contract_code_sha3,
            "contract_code_matches": contract_code_matches,
        },
        "deployment_verified": deployment_verified,
    })
}

pub(crate) fn channel_snapshot(
    ctx: &ApiExecCtx,
    contract_address: &str,
    deployment_tx_hash: &str,
    deployment_height: u64,
) -> Result<Value, String> {
    let config = ctx.engine.config();
    let observed_height = ctx.engine.latest_block().height().uint();
    if !snapshot_network_allowed(config.chain_id, observed_height, deployment_height) {
        return Err(
            "HPAY HVM channel snapshot does not match the requested chain and deployment height"
                .into(),
        );
    }
    let contract = Address::from_readable(contract_address)
        .map_err(|_| "HPAY HVM channel contract address is invalid".to_owned())
        .and_then(|address| {
            ContractAddress::from_addr(address)
                .map_err(|_| "HPAY HVM channel address is not a contract".to_owned())
        })?;
    let deployment_hash = Hash::from_hex(deployment_tx_hash.as_bytes())
        .map_err(|_| "HPAY HVM deployment transaction hash is invalid".to_owned())?;
    let state = ctx.engine.state();
    let core = CoreStateRead::wrap(state.as_ref().as_ref());
    if core.tx_exist(&deployment_hash).map(|height| height.uint()) != Some(deployment_height) {
        return Err("HPAY HVM deployment transaction is not included at the bound height".into());
    }
    if !deployment_action_verified(
        ctx,
        &deployment_hash,
        deployment_height,
        &contract,
        manifest_bytecode_sha3()?,
    ) {
        return Err(
            "HPAY HVM deployment transaction does not deploy this exact contract artifact".into(),
        );
    }
    let hvm = VMStateRead::wrap(state.as_ref().as_ref());
    let code_sha3 = hvm
        .contract_edition(&contract)
        .map(|edition| edition.hash.to_hex())
        .ok_or_else(|| "HPAY HVM channel contract does not exist".to_owned())?;
    let manifest: Value =
        serde_json::from_str(MANIFEST).map_err(|_| "HPAY HVM manifest is invalid".to_owned())?;
    if !manifest_valid(&manifest) || manifest["bytecode_sha3"].as_str() != Some(&code_sha3) {
        return Err(
            "HPAY HVM channel contract bytecode does not match the reviewed artifact".into(),
        );
    }

    let evaluation_height = observed_height
        .checked_add(1)
        .ok_or_else(|| "HPAY HVM snapshot height overflow".to_owned())?;
    let gas = GasExtra::new(evaluation_height);
    let cap = SpaceCap::new(evaluation_height);
    let mut storage = serde_json::Map::new();
    let mut minimum_live_blocks = u64::MAX;
    let mut minimum_recover_blocks = u64::MAX;
    for key in STORAGE_KEYS {
        let debug = hvm
            .debug_storage_get(
                &gas,
                &cap,
                evaluation_height,
                &contract.to_addr(),
                &HvmValue::bytes(key.as_bytes().to_vec()),
            )
            .map_err(|_| format!("HPAY HVM storage key {key} cannot be read"))?
            .ok_or_else(|| format!("HPAY HVM storage key {key} is missing"))?;
        let value = typed_storage_value(key, &debug.value)?;
        minimum_live_blocks = minimum_live_blocks.min(debug.live_blocks);
        minimum_recover_blocks = minimum_recover_blocks.min(debug.recover_blocks);
        storage.insert(
            (*key).to_owned(),
            json!({
                "value": value,
                "live_blocks": debug.live_blocks,
                "recover_blocks": debug.recover_blocks,
                "active": debug.active,
                "recoverable": debug.recoverable,
            }),
        );
    }
    let all_keys_active = storage
        .values()
        .all(|entry| entry["active"].as_bool() == Some(true));
    Ok(json!({
        "ret": 0,
        "schema": "hpay-hvm-channel-live-snapshot/1",
        "chain_id": config.chain_id,
        "observed_height": observed_height,
        "evaluation_height": evaluation_height,
        "contract_address": contract.to_readable(),
        "deployment_tx_hash": deployment_hash.to_hex(),
        "deployment_height": deployment_height,
        "deployment_action_verified": true,
        "bytecode_sha3": code_sha3,
        "storage_key_count": STORAGE_KEYS.len(),
        "all_keys_active": all_keys_active,
        "minimum_live_blocks": minimum_live_blocks,
        "minimum_recover_blocks": minimum_recover_blocks,
        "storage": storage,
    }))
}

fn manifest_bytecode_sha3() -> Result<&'static str, String> {
    const EXPECTED: &str = "11a2efc27a0c951bbc6977186eb58bd076dd331a785f3c57242cf54a72238349";
    Ok(EXPECTED)
}

fn deployment_action_verified(
    ctx: &ApiExecCtx,
    expected_tx_hash: &Hash,
    deployment_height: u64,
    expected_contract: &ContractAddress,
    expected_code_sha3: &str,
) -> bool {
    crate::hpay_contract_deployment::verify_contract_deployment(
        ctx,
        expected_tx_hash,
        deployment_height,
        expected_contract,
        expected_code_sha3,
    )
    .is_some()
}

#[cfg(test)]
fn deployment_transaction_matches(
    transaction: &dyn basis::interface::TransactionRead,
    expected_contract: &ContractAddress,
    expected_code_sha3: &str,
) -> bool {
    crate::hpay_contract_deployment::matching_deployment_in_transaction(
        transaction,
        expected_contract,
        expected_code_sha3,
    )
    .is_some()
}

fn typed_storage_value(key: &str, value: &HvmValue) -> Result<Value, String> {
    let invalid = || format!("HPAY HVM storage key {key} has an unexpected type");
    match key {
        "status" => match value {
            HvmValue::U8(value) => Ok(Value::from(*value)),
            _ => Err(invalid()),
        },
        "reuse" => match value {
            HvmValue::U32(value) => Ok(Value::from(*value)),
            _ => Err(invalid()),
        },
        "network" => match value {
            HvmValue::Bytes(bytes) if bytes.len() == 32 => Ok(Value::from(hex::encode(bytes))),
            _ => Err(invalid()),
        },
        "channel_id" => match value {
            HvmValue::Bytes(bytes) if bytes.len() == 16 => Ok(Value::from(hex::encode(bytes))),
            _ => Err(invalid()),
        },
        "left" | "right" => value
            .extract_address()
            .map(|address| Value::from(address.to_readable()))
            .map_err(|_| invalid()),
        "left_claimed" | "right_claimed" => match value {
            HvmValue::Bool(value) => Ok(Value::from(*value)),
            _ => Err(invalid()),
        },
        // `hac_to_zhu` is a U128 native result. The reviewed contract stores
        // the initial paid counters as U64, then writes that native result
        // after funding. Accept only unsigned values that losslessly fit the
        // protocol's u64 Zhu fields; all other storage widths remain strict.
        "left_paid" | "right_paid" => value.extract_u64().map(Value::from).map_err(|_| invalid()),
        _ => match value {
            HvmValue::U64(value) => Ok(Value::from(*value)),
            _ => Err(invalid()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use basis::interface::Transaction;
    use field::{Amount, Field, Uint4};
    use protocol::transaction::TransactionType3;
    use sys::Account;

    #[test]
    fn snapshot_network_policy_keeps_mainnet_strict_and_allows_isolated_testnet() {
        assert!(snapshot_network_allowed(
            protocol::upgrade::MAINNET_CHAIN_ID,
            MAINNET_MIN_SAFE_HEIGHT,
            MAINNET_MIN_SAFE_HEIGHT
        ));
        assert!(!snapshot_network_allowed(
            protocol::upgrade::MAINNET_CHAIN_ID,
            MAINNET_MIN_SAFE_HEIGHT,
            MAINNET_MIN_SAFE_HEIGHT - 1
        ));
        assert!(snapshot_network_allowed(7, 2, 1));
        assert!(!snapshot_network_allowed(7, 1, 0));
        assert!(!snapshot_network_allowed(7, 1, 2));
    }

    #[test]
    fn storage_schema_requires_exact_hvm_widths_and_shapes() {
        assert_eq!(
            typed_storage_value("status", &HvmValue::U8(2)).unwrap(),
            json!(2)
        );
        assert!(typed_storage_value("status", &HvmValue::U64(2)).is_err());
        assert_eq!(
            typed_storage_value("reuse", &HvmValue::U32(7)).unwrap(),
            json!(7)
        );
        assert!(typed_storage_value("reuse", &HvmValue::U8(7)).is_err());
        assert_eq!(
            typed_storage_value("left_deposit", &HvmValue::U64(10)).unwrap(),
            json!(10)
        );
        assert!(typed_storage_value("left_deposit", &HvmValue::U32(10)).is_err());
        assert_eq!(
            typed_storage_value("left_paid", &HvmValue::U128(10)).unwrap(),
            json!(10)
        );
        assert_eq!(
            typed_storage_value("right_paid", &HvmValue::U64(0)).unwrap(),
            json!(0)
        );
        assert!(
            typed_storage_value("left_paid", &HvmValue::U128(u128::from(u64::MAX) + 1)).is_err()
        );
        assert!(typed_storage_value("left_paid", &HvmValue::Bytes(vec![10])).is_err());
        assert_eq!(
            typed_storage_value("left_claimed", &HvmValue::Bool(false)).unwrap(),
            json!(false)
        );
        assert!(typed_storage_value("left_claimed", &HvmValue::U8(0)).is_err());
        assert!(typed_storage_value("network", &HvmValue::Bytes(vec![1; 31])).is_err());
        assert!(typed_storage_value("channel_id", &HvmValue::Bytes(vec![1; 17])).is_err());
    }

    #[test]
    fn manifest_and_endpoint_cover_the_same_exact_storage_inventory() {
        let manifest: Value = serde_json::from_str(MANIFEST).unwrap();
        let keys = manifest["storage_keys"].as_array().unwrap();
        assert_eq!(keys.len(), STORAGE_KEYS.len());
        for (manifest_key, expected) in keys.iter().zip(STORAGE_KEYS) {
            assert_eq!(manifest_key.as_str(), Some(*expected));
        }
    }

    #[test]
    fn deployment_transaction_is_bound_to_exact_derived_address_and_code() {
        let deployer = Account::create_by("hpay-deployment-proof").unwrap();
        let deployer_address = Address::from(deployer.address().clone());
        let nonce = Uint4::from(7);
        let expected_contract = ContractAddress::calculate(&deployer_address, &nonce);
        let compiled = vm::fitshc::compile(SOURCE).unwrap().0.into_sto();
        let expected_code = compiled.calc_edition().hash.to_hex();
        let mut deploy = vm::action::ContractDeploy::new();
        deploy.nonce = nonce;
        deploy.contract = compiled;
        let mut transaction = TransactionType3::new_by(deployer_address, Amount::unit238(1), 1);
        transaction.push_action(Box::new(deploy.clone())).unwrap();
        assert!(deployment_transaction_matches(
            &transaction,
            &expected_contract,
            &expected_code
        ));

        let wrong_contract = ContractAddress::calculate(&deployer_address, &Uint4::from(8));
        assert!(!deployment_transaction_matches(
            &transaction,
            &wrong_contract,
            &expected_code
        ));
        assert!(!deployment_transaction_matches(
            &transaction,
            &expected_contract,
            &"ff".repeat(32)
        ));

        transaction.push_action(Box::new(deploy)).unwrap();
        assert!(!deployment_transaction_matches(
            &transaction,
            &expected_contract,
            &expected_code
        ));
    }
}
