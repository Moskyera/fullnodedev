use std::sync::Arc;

use basis::component::TX_ACTIONS_MAX;
use basis::config::EngineConf;
use basis::interface::{ApiExecCtx, ApiRequest, ApiResponse, ApiRoute, ApiService};
use field::*;
use protocol::setup::ProtocolSetup;
use serde_json::{json, Value};

use crate::{HACASH_NODE_BUILD_TIME, HACASH_NODE_VERSION};

const CAPABILITIES_API_VERSION: u32 = 1;
const ACTION_GUARD_ACTION_KINDS: &[u16] = &[0x0411, 0x0412, 0x0413, 0x0414];
const TX_BLOB_ACTION_KIND: u16 = 0x0402;
const AST_ACTION_KINDS: &[u16] = &[25, 26];
const TEX_ACTION_KIND: u16 = 22;
const NATIVE_ASSET_ACTION_KINDS: &[u16] = &[17, 18, 19];
const HIP20_PRIMITIVE_ACTION_KINDS: &[u16] = &[16, 17, 18, 19];
const CONTRACT_ACTION_KINDS: &[u16] = &[40, 41, 44];
const P2SH_ACTION_KIND: u16 = 46;
const REQ_SIGN_LIST_ACTION_KIND: u16 = 0x0414;
const TYPE4_TRANSACTION_TYPE: u8 = 4;
const ACCOUNT_ABSTRACTION_ACTION_KINDS: &[u16] = &[40, 41, 44, P2SH_ACTION_KIND];
const LOCAL_PILOT_NETWORK_KIND: &str = "local_pilot_v1";
const LOCAL_PILOT_PROFILE_ID: &str = "hpay-local-pilot-chain-v1";
const TRANSACTION_FORMAT_VERSION: u64 = 2;

#[derive(Default)]
struct NodeCapabilitiesService;

impl ApiService for NodeCapabilitiesService {
    fn name(&self) -> &'static str {
        "node-capabilities"
    }

    fn routes(&self) -> Vec<ApiRoute> {
        vec![ApiRoute::get("/query/capabilities", query_capabilities)]
    }
}

pub fn service() -> Arc<dyn ApiService> {
    Arc::new(NodeCapabilitiesService)
}

fn query_capabilities(ctx: &ApiExecCtx, _req: ApiRequest) -> ApiResponse {
    let height = ctx.engine.latest_block().height().uint();
    let config = ctx.engine.config();
    let setup = protocol::setup::current_setup();
    let block_one_hash = canonical_block_one_hash(ctx);
    let funding_confirmed = confirmed_pilot_funding(ctx);
    ApiResponse::json(
        build_capabilities(
            config,
            setup.as_ref(),
            height,
            block_one_hash.as_deref(),
            funding_confirmed,
        )
        .to_string(),
    )
}

fn canonical_block_one_hash(ctx: &ApiExecCtx) -> Option<String> {
    let (stored_hash, bytes) = ctx
        .engine
        .store()
        .block_data_by_height(&BlockHeight::from(1))?;
    let package = protocol::block::build_block_package(bytes).ok()?;
    if package.block().height().uint() != 1 || package.hash() != stored_hash {
        return None;
    }
    Some(stored_hash.to_hex())
}

fn confirmed_pilot_funding(ctx: &ApiExecCtx) -> bool {
    let Some(address) = ctx.engine.config().pilot_funding_address.as_ref() else {
        return false;
    };
    let state = ctx.engine.state();
    let core = protocol::state::CoreStateRead::wrap(state.as_ref().as_ref());
    core.balance(address)
        .unwrap_or_default()
        .hacash
        .is_positive()
}

fn push_identity_field(bytes: &mut Vec<u8>, value: &str) {
    bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
}

fn network_instance_id(
    network_kind: &str,
    chain_id: u32,
    mainnet: bool,
    block_one_hash: &str,
    node_profile_id: &str,
) -> String {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"HPAY/NETWORK-INSTANCE/V1");
    push_identity_field(&mut bytes, network_kind);
    bytes.extend_from_slice(&chain_id.to_be_bytes());
    bytes.push(u8::from(mainnet));
    push_identity_field(&mut bytes, block_one_hash);
    push_identity_field(&mut bytes, node_profile_id);
    bytes.extend_from_slice(&TRANSACTION_FORMAT_VERSION.to_be_bytes());
    hex::encode(sys::sha2(&bytes))
}

fn enabled_transaction_types(
    setup: &ProtocolSetup,
    chain_id: u32,
    evaluation_height: u64,
) -> Vec<u8> {
    setup
        .registered_tx_types()
        .into_iter()
        .filter(|ty| protocol::upgrade::check_gated_tx(chain_id, evaluation_height, *ty).is_ok())
        .collect()
}

fn enabled_action_kinds(setup: &ProtocolSetup, chain_id: u32, evaluation_height: u64) -> Vec<u16> {
    setup
        .registered_action_kinds()
        .into_iter()
        .filter(|kind| {
            protocol::upgrade::check_gated_action(chain_id, evaluation_height, *kind).is_ok()
        })
        .collect()
}

fn has_all_action_kinds(setup: &ProtocolSetup, kinds: &[u16]) -> bool {
    kinds.iter().all(|kind| setup.has_action_kind(*kind))
}

fn build_capabilities(
    config: &EngineConf,
    setup: &ProtocolSetup,
    height: u64,
    block_one_hash: Option<&str>,
    funding_confirmed: bool,
) -> Value {
    let chain_id = config.chain_id;
    let next_height = height.saturating_add(1);
    let registered_transactions = setup.registered_tx_types();
    let registered_actions = setup.registered_action_kinds();
    let enabled_transactions = enabled_transaction_types(setup, chain_id, next_height);
    let enabled_actions = enabled_action_kinds(setup, chain_id, next_height);
    let mainnet = chain_id == protocol::upgrade::MAINNET_CHAIN_ID;
    let istanbul_active = !mainnet || protocol::upgrade::is_online_upgrade_open(next_height);
    let type4_mainnet = setup.has_tx_type(TYPE4_TRANSACTION_TYPE)
        && protocol::upgrade::check_gated_tx(
            protocol::upgrade::MAINNET_CHAIN_ID,
            next_height,
            TYPE4_TRANSACTION_TYPE,
        )
        .is_ok();
    let network_kind = if mainnet {
        "mainnet"
    } else if config.network_kind == LOCAL_PILOT_NETWORK_KIND
        && config.node_profile_id == LOCAL_PILOT_PROFILE_ID
    {
        LOCAL_PILOT_NETWORK_KIND
    } else {
        "unidentified_non_mainnet"
    };
    let node_profile_id = if mainnet {
        "hacash-mainnet"
    } else {
        config.node_profile_id.as_str()
    };
    let block_one_hash = block_one_hash.filter(|hash| {
        hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    let instance_id = block_one_hash
        .map(|hash| network_instance_id(network_kind, chain_id, mainnet, hash, node_profile_id));
    let transaction_ready = !mainnet
        && network_kind == LOCAL_PILOT_NETWORK_KIND
        && height >= 2
        && block_one_hash.is_some()
        && funding_confirmed;

    // These flags describe codecs/runtime actually wired into this node process.
    // Chain-height availability remains separately represented by `actions.enabled`.
    let hvm = setup.has_vm_assigner();
    let p2sh = hvm && setup.has_action_kind(P2SH_ACTION_KIND);
    let contract_runtime = hvm && has_all_action_kinds(setup, CONTRACT_ACTION_KINDS);
    let account_abstraction = hvm && has_all_action_kinds(setup, ACCOUNT_ABSTRACTION_ACTION_KINDS);
    let intent = contract_runtime;
    let contract_state_leasing = contract_runtime;

    json!({
        "ret": 0,
        "api_version": CAPABILITIES_API_VERSION,
        "node": {
            "name": "hacash-fullnode",
            "version": HACASH_NODE_VERSION,
            "build_time": HACASH_NODE_BUILD_TIME,
        },
        "chain": {
            "id": chain_id,
            "height": height,
            "next_height": next_height,
            "mainnet": mainnet,
        },
        "network": {
            "kind": network_kind,
            "node_profile_id": node_profile_id,
            "block_1_available": block_one_hash.is_some(),
            "block_1_hash": block_one_hash,
            "instance_id": instance_id,
            "funding_confirmed": funding_confirmed,
            "transaction_ready": transaction_ready,
            "current_height": height,
            "transaction_format_version": TRANSACTION_FORMAT_VERSION,
        },
        "istanbul": {
            "activation_height": protocol::upgrade::ONLINE_OPEN_HEIGHT,
            "evaluation_height": next_height,
            "active": istanbul_active,
        },
        "transactions": {
            "registered": registered_transactions,
            "enabled": enabled_transactions,
        },
        "actions": {
            "registered": registered_actions,
            "enabled": enabled_actions,
        },
        "features": {
            "action_guard": has_all_action_kinds(setup, ACTION_GUARD_ACTION_KINDS),
            "tx_blob": setup.has_action_kind(TX_BLOB_ACTION_KIND),
            "ast": has_all_action_kinds(setup, AST_ACTION_KINDS),
            "tex": setup.has_action_kind(TEX_ACTION_KIND),
            "native_assets": has_all_action_kinds(setup, NATIVE_ASSET_ACTION_KINDS),
            "hip20_primitives": has_all_action_kinds(setup, HIP20_PRIMITIVE_ACTION_KINDS),
            "hip20": false,
            "hvm": hvm,
            "p2sh": p2sh,
            "account_abstraction": account_abstraction,
            "intent": intent,
            "contract_state_leasing": contract_state_leasing,
            "ir_decompilation": false,
            "req_sign_list": setup.has_action_kind(REQ_SIGN_LIST_ACTION_KIND),
            "type4_mainnet": type4_mainnet,
            "exact_unsigned_simulation": false,
        },
        // Registered by the mint API service in this same fullnode process.
        // Clients must not infer write support from a version string.
        "api": {
            "balance_query": true,
            "transaction_submit": true,
            "transaction_query": true,
            "reconciliation_by_tx_hash": true,
        },
        "limits": {
            "max_tx_size": config.max_tx_size,
            "max_tx_actions": config.max_tx_actions.min(TX_ACTIONS_MAX),
            "max_type3_signers": protocol::params::MAX_TYPE3_SIGNERS,
            "gas_max_byte": protocol::context::TX_GAS_BUDGET_CAP_BYTE,
            "gas_max": protocol::context::decode_gas_budget(
                protocol::context::TX_GAS_BUDGET_CAP_BYTE,
            ),
            "ast_depth": protocol::action::AST_TREE_DEPTH_MAX,
        },
    })
}

#[cfg(test)]
mod node_capabilities_tests {
    use super::*;

    fn test_config(chain_id: u32) -> EngineConf {
        let mut config = EngineConf::new(&sys::IniObj::new());
        config.chain_id = chain_id;
        config
    }

    fn test_setup() -> ProtocolSetup {
        let mut setup = protocol::setup::new_standard_protocol_setup(x16rs::block_hash);
        mint::setup::register_protocol_extensions(&mut setup);
        setup
    }

    fn numbers(value: &Value) -> Vec<u64> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item.as_u64().unwrap())
            .collect()
    }

    #[test]
    fn mainnet_capabilities_keep_type4_disabled() {
        let config = test_config(protocol::upgrade::MAINNET_CHAIN_ID);
        let setup = test_setup();
        let value = build_capabilities(
            &config,
            &setup,
            protocol::upgrade::ONLINE_OPEN_HEIGHT,
            None,
            false,
        );

        assert_eq!(value["ret"].as_u64(), Some(0));
        assert_eq!(value["api_version"].as_u64(), Some(1));
        assert_eq!(value["istanbul"]["active"].as_bool(), Some(true));
        assert_eq!(value["features"]["type4_mainnet"].as_bool(), Some(false));
        for name in [
            "balance_query",
            "transaction_submit",
            "transaction_query",
            "reconciliation_by_tx_hash",
        ] {
            assert_eq!(value["api"][name].as_bool(), Some(true), "API {name}");
        }
        assert_eq!(
            value["features"]["exact_unsigned_simulation"].as_bool(),
            Some(false),
        );
        assert!(
            !numbers(&value["transactions"]["enabled"]).contains(&(TYPE4_TRANSACTION_TYPE as u64))
        );
    }

    #[test]
    fn registered_and_enabled_lists_are_sorted() {
        let config = test_config(protocol::upgrade::MAINNET_CHAIN_ID);
        let setup = test_setup();
        let value = build_capabilities(
            &config,
            &setup,
            protocol::upgrade::ONLINE_OPEN_HEIGHT,
            None,
            false,
        );

        for path in [
            &value["transactions"]["registered"],
            &value["transactions"]["enabled"],
            &value["actions"]["registered"],
            &value["actions"]["enabled"],
        ] {
            let values = numbers(path);
            assert!(values.windows(2).all(|pair| pair[0] < pair[1]));
        }
    }

    #[test]
    fn closed_mainnet_window_does_not_claim_istanbul() {
        let config = test_config(protocol::upgrade::MAINNET_CHAIN_ID);
        let setup = test_setup();
        let height = protocol::upgrade::ONLINE_OPEN_HEIGHT - 2;
        let value = build_capabilities(&config, &setup, height, None, false);

        assert_eq!(value["istanbul"]["active"].as_bool(), Some(false));
        assert!(!numbers(&value["transactions"]["enabled"]).contains(&3));
    }

    #[test]
    fn protocol_only_test_setup_reports_non_vm_features_honestly() {
        let config = test_config(protocol::upgrade::MAINNET_CHAIN_ID);
        let setup = test_setup();
        assert!(!setup.has_vm_assigner());

        let value = build_capabilities(
            &config,
            &setup,
            protocol::upgrade::ONLINE_OPEN_HEIGHT,
            None,
            false,
        );
        let features = &value["features"];

        for name in [
            "action_guard",
            "tx_blob",
            "ast",
            "tex",
            "native_assets",
            "hip20_primitives",
        ] {
            assert_eq!(features[name].as_bool(), Some(true), "feature {name}");
        }
        for name in [
            "hip20",
            "hvm",
            "p2sh",
            "account_abstraction",
            "intent",
            "contract_state_leasing",
            "ir_decompilation",
        ] {
            assert_eq!(features[name].as_bool(), Some(false), "feature {name}");
        }
    }

    #[test]
    fn local_pilot_requires_block_one_two_blocks_and_confirmed_funding() {
        let mut config = test_config(7);
        config.network_kind = LOCAL_PILOT_NETWORK_KIND.to_owned();
        config.node_profile_id = LOCAL_PILOT_PROFILE_ID.to_owned();
        let setup = test_setup();
        let block_one = "000008c8c945c4ca797f5aa70530caa51030ee0037e76410fd113852d50f2dff";

        let empty = build_capabilities(&config, &setup, 0, None, false);
        assert_eq!(empty["network"]["block_1_available"], false);
        assert_eq!(empty["network"]["transaction_ready"], false);

        let one_block = build_capabilities(&config, &setup, 1, Some(block_one), true);
        assert_eq!(one_block["network"]["transaction_ready"], false);

        let unfunded = build_capabilities(&config, &setup, 2, Some(block_one), false);
        assert_eq!(unfunded["network"]["transaction_ready"], false);

        let ready = build_capabilities(&config, &setup, 2, Some(block_one), true);
        assert_eq!(ready["network"]["kind"], LOCAL_PILOT_NETWORK_KIND);
        assert_eq!(ready["network"]["block_1_hash"], block_one);
        assert_eq!(ready["network"]["funding_confirmed"], true);
        assert_eq!(ready["network"]["transaction_ready"], true);
        assert_eq!(ready["network"]["instance_id"].as_str().unwrap().len(), 64);
    }
}
