use std::sync::Arc;

use basis::component::TX_ACTIONS_MAX;
use basis::config::EngineConf;
use basis::interface::{
    ApiExecCtx, ApiRequest, ApiResponse, ApiRoute, ApiService, PeerConnectivity,
};
use field::*;
use protocol::setup::ProtocolSetup;
use serde_json::{Value, json};

use crate::hpay_channel_exit::{
    MAINNET_MIN_SAFE_HEIGHT, channel_snapshot as hpay_channel_exit_snapshot,
    evidence as hpay_channel_exit_evidence,
};
use crate::hpay_channel_registry::{
    channel_snapshot as hpay_channel_registry_snapshot,
    evidence as hpay_channel_registry_exit_evidence,
};
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
const MAINNET_NETWORK_KIND: &str = "mainnet";
const MAINNET_PROFILE_ID: &str = "hacash-mainnet";
const TRANSACTION_FORMAT_VERSION: u64 = 2;

#[derive(Default)]
struct NodeCapabilitiesService;

impl ApiService for NodeCapabilitiesService {
    fn name(&self) -> &'static str {
        "node-capabilities"
    }

    fn routes(&self) -> Vec<ApiRoute> {
        vec![
            ApiRoute::get("/query/capabilities", query_capabilities),
            ApiRoute::get("/query/hpay/channel-exit", query_hpay_channel_exit),
            ApiRoute::get("/query/hpay/channel-registry", query_hpay_channel_registry),
            ApiRoute::post(
                "/submit/transaction/hpay-bound",
                submit_transaction_hpay_bound,
            ),
        ]
    }
}

fn submit_transaction_hpay_bound(ctx: &ApiExecCtx, req: ApiRequest) -> ApiResponse {
    let expected_chain_id = match required_bound_chain_id(&req) {
        Ok(value) => value,
        Err(error) => return ApiResponse::json(json!({"ret": 1, "err": error}).to_string()),
    };
    let expected_instance_id = match required_bound_network_instance_id(&req) {
        Ok(value) => value,
        Err(error) => return ApiResponse::json(json!({"ret": 1, "err": error}).to_string()),
    };
    mint::api::submit_transaction_with_pre_admission_check(ctx, req, move |ctx| {
        validate_hpay_bound_submit(ctx, expected_chain_id, &expected_instance_id)
    })
}

fn validate_hpay_bound_submit(
    ctx: &ApiExecCtx,
    expected_chain_id: u32,
    expected_instance_id: &str,
) -> Result<(), String> {
    let actual = current_network_instance(ctx).ok_or_else(|| {
        "HPAY bound submit is unavailable until canonical block 1 is readable".to_owned()
    })?;
    validate_bound_network_identity(expected_chain_id, expected_instance_id, &actual)
}

fn validate_bound_network_identity(
    expected_chain_id: u32,
    expected_instance_id: &str,
    actual: &CurrentNetworkInstance,
) -> Result<(), String> {
    if expected_chain_id != actual.chain_id {
        return Err("HPAY bound submit chain_id mismatch".to_owned());
    }
    if expected_instance_id != actual.instance_id {
        return Err("HPAY bound submit network_instance_id mismatch".to_owned());
    }
    Ok(())
}

fn required_bound_chain_id(req: &ApiRequest) -> Result<u32, String> {
    let value = req
        .query("chain_id")
        .ok_or_else(|| "HPAY bound submit requires chain_id".to_owned())?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("HPAY bound submit chain_id must be canonical decimal u32".to_owned());
    }
    let parsed = value
        .parse::<u32>()
        .map_err(|_| "HPAY bound submit chain_id must be canonical decimal u32".to_owned())?;
    if parsed.to_string() != value {
        return Err("HPAY bound submit chain_id must be canonical decimal u32".to_owned());
    }
    Ok(parsed)
}

fn required_bound_network_instance_id(req: &ApiRequest) -> Result<String, String> {
    let value = req
        .query("network_instance_id")
        .ok_or_else(|| "HPAY bound submit requires network_instance_id".to_owned())?;
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(
            "HPAY bound submit network_instance_id must be lowercase SHA-256 hex".to_owned(),
        );
    }
    Ok(value.to_owned())
}

fn query_hpay_channel_exit(ctx: &ApiExecCtx, req: ApiRequest) -> ApiResponse {
    let contract = req.query("contract").unwrap_or("");
    let deployment_tx_hash = req.query("deployment_tx_hash").unwrap_or("");
    let deployment_height = match req
        .query("deployment_height")
        .and_then(|value| value.parse::<u64>().ok())
    {
        Some(height) => height,
        None => {
            return ApiResponse::json(
                json!({"ret": 1, "err": "HPAY HVM deployment height is invalid"}).to_string(),
            );
        }
    };
    match hpay_channel_exit_snapshot(ctx, contract, deployment_tx_hash, deployment_height) {
        Ok(value) => ApiResponse::json(value.to_string()),
        Err(error) => ApiResponse::json(json!({"ret": 1, "err": error}).to_string()),
    }
}

fn query_hpay_channel_registry(ctx: &ApiExecCtx, req: ApiRequest) -> ApiResponse {
    let contract = req.query("contract").unwrap_or("");
    let deployment_tx_hash = req.query("deployment_tx_hash").unwrap_or("");
    let left = req.query("left").unwrap_or("");
    let deployment_height = match req
        .query("deployment_height")
        .and_then(|value| value.parse::<u64>().ok())
    {
        Some(height) => height,
        None => {
            return ApiResponse::json(
                json!({"ret": 1, "err": "HPAY HVM registry deployment height is invalid"})
                    .to_string(),
            );
        }
    };
    let Some(network) = current_network_instance(ctx) else {
        return ApiResponse::json(
            json!({"ret": 1, "err": "HPAY HVM registry requires canonical block 1"}).to_string(),
        );
    };
    match hpay_channel_registry_snapshot(
        ctx,
        contract,
        deployment_tx_hash,
        deployment_height,
        left,
        &network.instance_id,
    ) {
        Ok(value) => ApiResponse::json(value.to_string()),
        Err(error) => ApiResponse::json(json!({"ret": 1, "err": error}).to_string()),
    }
}

pub fn service() -> Arc<dyn ApiService> {
    Arc::new(NodeCapabilitiesService)
}

fn query_capabilities(ctx: &ApiExecCtx, _req: ApiRequest) -> ApiResponse {
    let height = ctx.engine.latest_block().height().uint();
    let tip_timestamp_unix = ctx.engine.latest_block().timestamp().uint();
    let observed_unix = sys::curtimes();
    let config = ctx.engine.config();
    let setup = protocol::setup::current_setup();
    let block_one_hash = canonical_block_one_hash(ctx);
    let funding_confirmed = confirmed_pilot_funding(ctx);
    let channel_exit_evidence = hpay_channel_exit_evidence(Some(ctx));
    let registry_exit_evidence = hpay_channel_registry_exit_evidence(Some(ctx));
    let peers = ctx.hnoder.peer_connectivity();
    ApiResponse::json(
        build_capabilities_with_tip_and_exit_evidence(
            config,
            setup.as_ref(),
            height,
            block_one_hash.as_deref(),
            funding_confirmed,
            tip_timestamp_unix,
            observed_unix,
            channel_exit_evidence,
            registry_exit_evidence,
            peers,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CurrentNetworkInstance {
    pub(crate) chain_id: u32,
    pub(crate) instance_id: String,
}

pub(crate) fn current_network_instance(ctx: &ApiExecCtx) -> Option<CurrentNetworkInstance> {
    let config = ctx.engine.config();
    let chain_id = config.chain_id;
    let mainnet = chain_id == protocol::upgrade::MAINNET_CHAIN_ID;
    let network_kind = if mainnet {
        MAINNET_NETWORK_KIND
    } else if config.network_kind == LOCAL_PILOT_NETWORK_KIND
        && config.node_profile_id == LOCAL_PILOT_PROFILE_ID
    {
        LOCAL_PILOT_NETWORK_KIND
    } else {
        "unidentified_non_mainnet"
    };
    let node_profile_id = if mainnet {
        MAINNET_PROFILE_ID
    } else {
        config.node_profile_id.as_str()
    };
    let block_one_hash = canonical_block_one_hash(ctx)?;
    Some(CurrentNetworkInstance {
        chain_id,
        instance_id: network_instance_id(
            network_kind,
            chain_id,
            mainnet,
            &block_one_hash,
            node_profile_id,
        ),
    })
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

/// The peer connectivity block of the capabilities document.
///
/// Deliberately verbose field names. This blob gets read alone, pasted into a
/// ticket, or parsed by something that never saw this file, and the mistake it
/// has to survive is reading a bound listening socket as proof of reachability.
/// `netstat` showing `0.0.0.0:3337 LISTENING` only means "ready to be called".
/// `inbound_established` is the count of peers that actually called.
fn peer_connectivity_report(peers: PeerConnectivity) -> Value {
    // Never report an unmeasured zero as a measured one. If the node build
    // cannot count peers, every count is null and the role is "unknown".
    let count = |n: usize| -> Value {
        match peers.measured {
            true => json!(n),
            false => Value::Null,
        }
    };
    let role = if !peers.measured {
        "unknown"
    } else if peers.inbound > 0 {
        // Someone reached us, so we accept connections and relay for others.
        "participant"
    } else {
        // We dial out, pull blocks, validate for ourselves, and serve nobody.
        "leaf"
    };
    json!({
        "measured": peers.measured,
        "total": count(peers.total),
        "inbound_established": count(peers.inbound),
        "outbound_established": count(peers.outbound),
        "public": count(peers.public),
        "inbound_proven": peers.inbound_proven(),
        "role": role,
        "note": "inbound_established counts remote peers that dialed this node and completed the p2p handshake. A listening socket is not a reached socket: a bound port and a green sync can both be true while inbound_established is 0, which means no peer has reached this node and it relays for nobody. Only a non zero inbound_established proves the p2p port is reachable from outside. outbound_established counts connections this node opened itself, and public counts peers we hold a dialable address for, which says nothing about us.",
    })
}

#[cfg(test)]
fn build_capabilities(
    config: &EngineConf,
    setup: &ProtocolSetup,
    height: u64,
    block_one_hash: Option<&str>,
    funding_confirmed: bool,
) -> Value {
    build_capabilities_with_tip(
        config,
        setup,
        height,
        block_one_hash,
        funding_confirmed,
        0,
        0,
    )
}

#[cfg(test)]
fn build_capabilities_with_tip(
    config: &EngineConf,
    setup: &ProtocolSetup,
    height: u64,
    block_one_hash: Option<&str>,
    funding_confirmed: bool,
    tip_timestamp_unix: u64,
    observed_unix: u64,
) -> Value {
    build_capabilities_with_peers(
        config,
        setup,
        height,
        block_one_hash,
        funding_confirmed,
        tip_timestamp_unix,
        observed_unix,
        PeerConnectivity::default(),
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn build_capabilities_with_peers(
    config: &EngineConf,
    setup: &ProtocolSetup,
    height: u64,
    block_one_hash: Option<&str>,
    funding_confirmed: bool,
    tip_timestamp_unix: u64,
    observed_unix: u64,
    peers: PeerConnectivity,
) -> Value {
    build_capabilities_with_tip_and_exit_evidence(
        config,
        setup,
        height,
        block_one_hash,
        funding_confirmed,
        tip_timestamp_unix,
        observed_unix,
        hpay_channel_exit_evidence(None),
        hpay_channel_registry_exit_evidence(None),
        peers,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_capabilities_with_tip_and_exit_evidence(
    config: &EngineConf,
    setup: &ProtocolSetup,
    height: u64,
    block_one_hash: Option<&str>,
    funding_confirmed: bool,
    tip_timestamp_unix: u64,
    observed_unix: u64,
    channel_exit_evidence: Value,
    registry_exit_evidence: Value,
    peers: PeerConnectivity,
) -> Value {
    const MAX_TIP_AGE_SECONDS: u64 = 3_600;
    const MAX_FUTURE_SKEW_SECONDS: u64 = 120;
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
        MAINNET_NETWORK_KIND
    } else if config.network_kind == LOCAL_PILOT_NETWORK_KIND
        && config.node_profile_id == LOCAL_PILOT_PROFILE_ID
    {
        LOCAL_PILOT_NETWORK_KIND
    } else {
        "unidentified_non_mainnet"
    };
    let node_profile_id = if mainnet {
        MAINNET_PROFILE_ID
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
    let tip_age_seconds = observed_unix.saturating_sub(tip_timestamp_unix);
    let tip_fresh = height > 0
        && tip_timestamp_unix > 0
        && tip_timestamp_unix <= observed_unix.saturating_add(MAX_FUTURE_SKEW_SECONDS)
        && tip_age_seconds <= MAX_TIP_AGE_SECONDS;
    let local_pilot_ready = !mainnet
        && network_kind == LOCAL_PILOT_NETWORK_KIND
        && height >= 2
        && block_one_hash.is_some()
        && funding_confirmed;
    let mainnet_ready = mainnet
        && height >= MAINNET_MIN_SAFE_HEIGHT
        && block_one_hash.is_some()
        && tip_fresh
        && enabled_transactions.contains(&2)
        && [1_u16, 2, 3, 14]
            .iter()
            .all(|kind| enabled_actions.contains(kind));
    let transaction_ready = local_pilot_ready || mainnet_ready;

    // These flags describe codecs/runtime actually wired into this node process.
    // Chain-height availability remains separately represented by `actions.enabled`.
    let hvm = setup.has_vm_assigner();
    let p2sh = hvm && setup.has_action_kind(P2SH_ACTION_KIND);
    let contract_runtime = hvm && has_all_action_kinds(setup, CONTRACT_ACTION_KINDS);
    let account_abstraction = hvm && has_all_action_kinds(setup, ACCOUNT_ABSTRACTION_ACTION_KINDS);
    let intent = contract_runtime;
    let contract_state_leasing = contract_runtime;
    // A verified deployed HVM artifact is necessary but not sufficient for
    // the existing native ChannelPay settlement profile. The wallet, Hub,
    // bill codec, funding path and recovery/watchtower must first bind every
    // channel to this exact contract profile. Never auto-enable the native
    // capability from deployment evidence alone.
    let channel_unilateral_exit = false;
    // The same rule for the shared registry V2 profile, which is the profile
    // this system actually settles on. It is deliberately a separate flag from
    // the V1 one above: they describe different contracts, and a node that can
    // execute one has said nothing about the other. Like V1 it is never
    // auto-enabled from deployment evidence — the evidence document beside it
    // is what a Hub weighs, and a `true` here would additionally assert that
    // this node parses and executes the registry challenge/finalize/claim
    // lifecycle.
    let channel_registry_unilateral_exit = false;

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
        "sync": {
            "tip_timestamp_unix": tip_timestamp_unix,
            "observed_unix": observed_unix,
            "tip_age_seconds": tip_age_seconds,
            "max_tip_age_seconds": MAX_TIP_AGE_SECONDS,
            "fresh": tip_fresh,
        },
        "peers": peer_connectivity_report(peers),
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
            // The Istanbul action registry currently has no non-conflicting,
            // registered challenge/respond/final-claim path for payment
            // channels. Legacy Go action numbers 22, 25 and 26 collide with
            // Istanbul TEX/AST actions, so operators must not infer unilateral
            // exit support from the persisted challenge fields alone.
            "channel_unilateral_exit": channel_unilateral_exit,
            "channel_unilateral_exit_evidence": channel_exit_evidence,
            // The shared-registry V2 settlement profile, reported beside V1
            // rather than in place of it. A consumer that measures the wrong
            // one of these two measures a contract this system does not use.
            "channel_registry_unilateral_exit": channel_registry_unilateral_exit,
            "channel_registry_unilateral_exit_evidence": registry_exit_evidence,
            "exact_unsigned_simulation": false,
        },
        // Registered by the mint API service in this same fullnode process.
        // Clients must not infer write support from a version string.
        "api": {
            "balance_query": true,
            "transaction_submit": true,
            "transaction_submit_bound": true,
            "transaction_query": true,
            "reconciliation_by_tx_hash": true,
            "contract_sandbox_query": hvm,
            "hpay_channel_registry_query": hvm,
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
    use crate::hpay_channel_exit::deployment_verified as hpay_channel_exit_deployment_verified;

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

    fn peers_value(peers: PeerConnectivity) -> Value {
        let config = test_config(protocol::upgrade::MAINNET_CHAIN_ID);
        let setup = test_setup();
        let value = build_capabilities_with_peers(
            &config,
            &setup,
            protocol::upgrade::ONLINE_OPEN_HEIGHT,
            None,
            false,
            0,
            0,
            peers,
        );
        value["peers"].clone()
    }

    #[test]
    fn peer_report_calls_a_node_nobody_dialed_a_leaf() {
        // Exactly what the live mainnet node measured: four outbound, zero in.
        let peers = peers_value(PeerConnectivity {
            total: 4,
            inbound: 0,
            outbound: 4,
            public: 4,
            measured: true,
        });
        assert_eq!(peers["measured"].as_bool(), Some(true));
        assert_eq!(peers["total"].as_u64(), Some(4));
        assert_eq!(peers["inbound_established"].as_u64(), Some(0));
        assert_eq!(peers["outbound_established"].as_u64(), Some(4));
        assert_eq!(peers["public"].as_u64(), Some(4));
        assert_eq!(peers["inbound_proven"].as_bool(), Some(false));
        assert_eq!(peers["role"].as_str(), Some("leaf"));
        // The note has to say the thing the socket state does not.
        assert!(
            peers["note"]
                .as_str()
                .unwrap()
                .contains("A listening socket is not a reached socket")
        );
    }

    #[test]
    fn peer_report_calls_a_node_someone_dialed_a_participant() {
        let peers = peers_value(PeerConnectivity {
            total: 5,
            inbound: 1,
            outbound: 4,
            public: 4,
            measured: true,
        });
        assert_eq!(peers["inbound_established"].as_u64(), Some(1));
        assert_eq!(peers["outbound_established"].as_u64(), Some(4));
        assert_eq!(peers["inbound_proven"].as_bool(), Some(true));
        assert_eq!(peers["role"].as_str(), Some("participant"));
    }

    #[test]
    fn peer_report_never_prints_an_unmeasured_zero_as_a_measured_one() {
        let peers = peers_value(PeerConnectivity::default());
        assert_eq!(peers["measured"].as_bool(), Some(false));
        assert!(peers["total"].is_null());
        assert!(peers["inbound_established"].is_null());
        assert!(peers["outbound_established"].is_null());
        assert!(peers["public"].is_null());
        assert_eq!(peers["inbound_proven"].as_bool(), Some(false));
        assert_eq!(peers["role"].as_str(), Some("unknown"));
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
        assert_eq!(
            value["features"]["channel_unilateral_exit"].as_bool(),
            Some(false),
        );
        let evidence = &value["features"]["channel_unilateral_exit_evidence"];
        assert_eq!(evidence["schema"], "hpay-hvm-channel-exit-evidence/1");
        assert_eq!(evidence["manifest_valid"], true);
        assert_eq!(evidence["contract_name"], "HPAYChannelExitV1");
        assert_eq!(
            evidence["bytecode_sha3"],
            "11a2efc27a0c951bbc6977186eb58bd076dd331a785f3c57242cf54a72238349"
        );
        assert_eq!(evidence["storage_key_count"], 18);
        assert_eq!(evidence["must_renew_every_storage_key"], true);
        assert_eq!(evidence["deployment"]["enabled"], false);
        assert_eq!(evidence["deployment"]["independently_verified"], false);
        assert_eq!(
            evidence["on_chain_verification"]["deployment_tx_confirmed"],
            false
        );
        assert_eq!(
            evidence["on_chain_verification"]["contract_code_matches"],
            false
        );
        assert_eq!(evidence["deployment_verified"], false);
        for name in [
            "balance_query",
            "transaction_submit",
            "transaction_submit_bound",
            "transaction_query",
            "reconciliation_by_tx_hash",
        ] {
            assert_eq!(value["api"][name].as_bool(), Some(true), "API {name}");
        }
        assert_eq!(
            value["features"]["exact_unsigned_simulation"].as_bool(),
            Some(false),
        );
        assert_eq!(
            value["api"]["contract_sandbox_query"].as_bool(),
            value["features"]["hvm"].as_bool(),
        );
        assert!(
            !numbers(&value["transactions"]["enabled"]).contains(&(TYPE4_TRANSACTION_TYPE as u64))
        );
    }

    #[test]
    fn channel_exit_deployment_requires_exact_mainnet_chain_evidence() {
        let height = MAINNET_MIN_SAFE_HEIGHT + 100;
        let address = Address::create_contract([7_u8; 20]).to_readable();
        let tx_hash = "11".repeat(32);
        let code_hash = "11a2efc27a0c951bbc6977186eb58bd076dd331a785f3c57242cf54a72238349";
        let deployment = json!({
            "enabled": true,
            "contract_address": address,
            "deployment_tx_hash": tx_hash,
            "deployment_height": height,
            "independently_verified": true,
        });

        assert!(hpay_channel_exit_deployment_verified(
            true,
            &deployment,
            Some(code_hash),
            Some(protocol::upgrade::MAINNET_CHAIN_ID),
            Some(height + 1),
            Some(height),
            Some(code_hash),
        ));
        assert!(!hpay_channel_exit_deployment_verified(
            true,
            &deployment,
            Some(code_hash),
            Some(protocol::upgrade::MAINNET_CHAIN_ID),
            Some(height + 1),
            Some(height - 1),
            Some(code_hash),
        ));
        assert!(!hpay_channel_exit_deployment_verified(
            true,
            &deployment,
            Some(code_hash),
            Some(protocol::upgrade::MAINNET_CHAIN_ID),
            Some(height + 1),
            Some(height),
            Some(&"22".repeat(32)),
        ));
        assert!(!hpay_channel_exit_deployment_verified(
            true,
            &deployment,
            Some(code_hash),
            Some(7),
            Some(height + 1),
            Some(height),
            Some(code_hash),
        ));
    }

    #[test]
    fn verified_hvm_artifact_does_not_masquerade_as_native_channel_exit() {
        let config = test_config(protocol::upgrade::MAINNET_CHAIN_ID);
        let setup = test_setup();
        let now = 2_000_000_u64;
        let block_one = "001e231cb03f9938d54f04407797b8188f0375eb10f0bcb426dccae87dcadb56";
        let evidence = json!({
            "schema": "hpay-hvm-channel-exit-evidence/1",
            "manifest_valid": true,
            "deployment_verified": true,
        });
        let registry_evidence = json!({
            "schema": "hpay-hvm-channel-registry-exit-evidence/2",
            "manifest_valid": true,
            "deployment_verified": true,
        });
        let value = build_capabilities_with_tip_and_exit_evidence(
            &config,
            &setup,
            MAINNET_MIN_SAFE_HEIGHT,
            Some(block_one),
            false,
            now - 60,
            now,
            evidence,
            registry_evidence,
            PeerConnectivity::default(),
        );
        assert_eq!(
            value["features"]["channel_unilateral_exit"].as_bool(),
            Some(false)
        );
        assert_eq!(
            value["features"]["channel_unilateral_exit_evidence"]["deployment_verified"],
            true
        );
        assert_eq!(
            value["features"]["channel_registry_unilateral_exit"].as_bool(),
            Some(false)
        );
        assert_eq!(
            value["features"]["channel_registry_unilateral_exit_evidence"]["deployment_verified"],
            true
        );
    }

    /// The two settlement profiles are reported side by side and never
    /// conflated. This is the test that would have caught the defect: a
    /// consumer reading `channel_unilateral_exit_evidence` is reading about
    /// `hpay-hvm-channel-v1`, which is not the profile this system settles on.
    #[test]
    fn capabilities_report_both_settlement_profiles_distinctly() {
        let config = test_config(protocol::upgrade::MAINNET_CHAIN_ID);
        let setup = test_setup();
        let value = build_capabilities(
            &config,
            &setup,
            protocol::upgrade::ONLINE_OPEN_HEIGHT,
            None,
            false,
        );
        let v1 = &value["features"]["channel_unilateral_exit_evidence"];
        let v2 = &value["features"]["channel_registry_unilateral_exit_evidence"];
        assert_eq!(v1["settlement_profile"], "hpay-hvm-channel-v1");
        assert_eq!(v2["settlement_profile"], "hpay-hvm-shared-registry-v2");
        assert_ne!(v1["schema"], v2["schema"]);
        assert_ne!(v1["bytecode_sha3"], v2["bytecode_sha3"]);
        assert_eq!(
            v2["bytecode_sha3"],
            "2fa7429d9e686dd2457eeb1b4476f972c7ddd9be6a0371c9765eff2910209b04"
        );
        assert_eq!(v2["contract_name"], "HPAYChannelRegistryV2");
        assert_eq!(v2["registry_key_count"], 6);
        assert_eq!(v2["channel_key_count"], 12);
        assert_eq!(v2["manifest_valid"], true);
        // Nothing is deployed on Hacash mainnet. Both must say so.
        assert_eq!(v1["deployment_verified"], false);
        assert_eq!(v2["deployment_verified"], false);
        assert_eq!(
            value["features"]["channel_registry_unilateral_exit"].as_bool(),
            Some(false)
        );
    }

    #[test]
    fn fresh_synced_mainnet_reports_type2_channel_payment_readiness() {
        let config = test_config(protocol::upgrade::MAINNET_CHAIN_ID);
        let setup = test_setup();
        let now = 2_000_000_u64;
        let block_one = "001e231cb03f9938d54f04407797b8188f0375eb10f0bcb426dccae87dcadb56";
        let value = build_capabilities_with_tip(
            &config,
            &setup,
            MAINNET_MIN_SAFE_HEIGHT,
            Some(block_one),
            false,
            now - 60,
            now,
        );
        assert_eq!(value["network"]["kind"], "mainnet");
        assert_eq!(value["network"]["node_profile_id"], "hacash-mainnet");
        assert_eq!(value["network"]["funding_confirmed"], false);
        assert_eq!(value["network"]["transaction_ready"], true);
        assert_eq!(value["sync"]["fresh"], true);
        assert_eq!(value["features"]["type4_mainnet"], false);
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
    fn sync_capability_rejects_stale_and_future_tips() {
        let config = test_config(protocol::upgrade::MAINNET_CHAIN_ID);
        let setup = test_setup();
        let now = 2_000_000_u64;
        let fresh = build_capabilities_with_tip(
            &config,
            &setup,
            protocol::upgrade::ONLINE_OPEN_HEIGHT,
            None,
            false,
            now - 60,
            now,
        );
        assert_eq!(fresh["sync"]["fresh"], true);
        assert_eq!(fresh["sync"]["tip_age_seconds"], 60);

        let stale = build_capabilities_with_tip(
            &config,
            &setup,
            protocol::upgrade::ONLINE_OPEN_HEIGHT,
            None,
            false,
            now - 3_601,
            now,
        );
        assert_eq!(stale["sync"]["fresh"], false);

        let future = build_capabilities_with_tip(&config, &setup, 1, None, false, now + 121, now);
        assert_eq!(future["sync"]["fresh"], false);
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

    #[test]
    fn hpay_security_routes_are_advertised_and_registered() {
        let routes = NodeCapabilitiesService.routes();
        assert!(routes.iter().any(|route| {
            route.method == basis::interface::ApiMethod::Post
                && route.path == "/submit/transaction/hpay-bound"
        }));
        assert!(routes.iter().any(|route| {
            route.method == basis::interface::ApiMethod::Get
                && route.path == "/query/hpay/channel-registry"
        }));

        let config = test_config(protocol::upgrade::MAINNET_CHAIN_ID);
        let setup = test_setup();
        let value = build_capabilities(&config, &setup, 1, None, false);
        assert_eq!(value["api"]["transaction_submit_bound"], true);
        assert_eq!(
            value["api"]["hpay_channel_registry_query"],
            value["features"]["hvm"]
        );
    }

    #[test]
    fn bound_submit_parameters_are_canonical_and_fail_closed() {
        let instance = "ab".repeat(32);
        let request = |chain_id: Option<&str>, instance_id: Option<&str>| ApiRequest {
            query: [
                chain_id.map(|value| ("chain_id".to_owned(), value.to_owned())),
                instance_id.map(|value| ("network_instance_id".to_owned(), value.to_owned())),
            ]
            .into_iter()
            .flatten()
            .collect(),
            ..ApiRequest::default()
        };

        let valid = request(Some("7"), Some(&instance));
        assert_eq!(required_bound_chain_id(&valid).unwrap(), 7);
        assert_eq!(
            required_bound_network_instance_id(&valid).unwrap(),
            instance
        );
        for invalid in [None, Some(""), Some("07"), Some("-1"), Some("4294967296")] {
            assert!(required_bound_chain_id(&request(invalid, Some(&"ab".repeat(32)))).is_err());
        }
        for invalid in [None, Some(""), Some("AB"), Some("zz"), Some("ab12")] {
            assert!(required_bound_network_instance_id(&request(Some("7"), invalid)).is_err());
        }
    }

    #[test]
    fn bound_submit_requires_exact_chain_and_network_instance() {
        let actual = CurrentNetworkInstance {
            chain_id: 7,
            instance_id: "ab".repeat(32),
        };
        assert!(validate_bound_network_identity(7, &actual.instance_id, &actual).is_ok());
        assert_eq!(
            validate_bound_network_identity(0, &actual.instance_id, &actual).unwrap_err(),
            "HPAY bound submit chain_id mismatch"
        );
        assert_eq!(
            validate_bound_network_identity(7, &"cd".repeat(32), &actual).unwrap_err(),
            "HPAY bound submit network_instance_id mismatch"
        );
    }
}
