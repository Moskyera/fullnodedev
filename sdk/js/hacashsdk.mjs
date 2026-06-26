const U64_MAX = (1n << 64n) - 1n;

function isNodeRuntime() {
    return typeof process !== "undefined"
        && process.versions != null
        && process.versions.node != null;
}

function normalizeTarget(target) {
    const mode = target || "auto";
    if (mode !== "auto" && mode !== "node" && mode !== "web") {
        throw new Error(`unsupported target "${mode}", expected auto|node|web`);
    }
    return mode;
}

function pickBackendApi(moduleApi) {
    if (moduleApi && typeof moduleApi.create_account === "function") {
        return moduleApi;
    }
    if (moduleApi && moduleApi.default && typeof moduleApi.default.create_account === "function") {
        return moduleApi.default;
    }
    throw new Error("invalid hacash sdk backend: create_account not found");
}

function pickField(input, names) {
    for (const name of names) {
        if (Object.prototype.hasOwnProperty.call(input, name) && input[name] !== undefined) {
            return input[name];
        }
    }
    return undefined;
}

function toU64BigInt(field, value) {
    let num = value;
    if (typeof num === "number") {
        if (!Number.isFinite(num) || !Number.isInteger(num)) {
            throw new TypeError(`${field} must be an integer`);
        }
        num = BigInt(num);
    } else if (typeof num === "string") {
        let s = num.trim();
        if (s.endsWith("n")) {
            s = s.slice(0, -1);
        }
        if (!/^\d+$/.test(s)) {
            throw new TypeError(`${field} must be a uint64 string`);
        }
        num = BigInt(s);
    } else if (typeof num !== "bigint") {
        throw new TypeError(`${field} must be number|string|bigint`);
    }
    if (num < 0n || num > U64_MAX) {
        throw new RangeError(`${field} out of uint64 range`);
    }
    return num;
}

function isInstanceOf(value, klass) {
    return typeof klass === "function" && value instanceof klass;
}

function ensureObjectParam(name, value) {
    if (value === undefined || value === null) {
        return {};
    }
    if (typeof value !== "object") {
        throw new TypeError(`${name} expects an object or wasm-bindgen class instance`);
    }
    return value;
}

function createFriendlyApi(rawApi, env) {
    if (typeof rawApi.create_account !== "function"
        || typeof rawApi.create_coin_transfer !== "function"
        || typeof rawApi.sign_transaction !== "function") {
        throw new Error("invalid hacash sdk backend exports");
    }

    const create_coin_transfer_param = (input) => {
        if (isInstanceOf(input, rawApi.CoinTransferParam)) {
            return input;
        }
        const src = ensureObjectParam("create_coin_transfer_param", input);
        const param = new rawApi.CoinTransferParam();

        const main_prikey = pickField(src, ["main_prikey"]);
        const from_prikey = pickField(src, ["from_prikey"]);
        const fee = pickField(src, ["fee"]);
        const to_address = pickField(src, ["to_address"]);
        const hacash = pickField(src, ["hacash"]);
        const diamonds = pickField(src, ["diamonds"]);
        const timestamp = pickField(src, ["timestamp"]);
        const satoshi = pickField(src, ["satoshi"]);
        const chain_id = pickField(src, ["chain_id"]);

        if (main_prikey !== undefined && main_prikey !== null) {
            param.main_prikey = String(main_prikey);
        }
        if (from_prikey !== undefined && from_prikey !== null) {
            param.from_prikey = String(from_prikey);
        }
        if (fee !== undefined && fee !== null) {
            param.fee = String(fee);
        }
        if (to_address !== undefined && to_address !== null) {
            param.to_address = String(to_address);
        }
        if (hacash !== undefined && hacash !== null) {
            param.hacash = String(hacash);
        }
        if (diamonds !== undefined && diamonds !== null) {
            param.diamonds = String(diamonds);
        }
        if (timestamp !== undefined && timestamp !== null) {
            param.timestamp = toU64BigInt("timestamp", timestamp);
        }
        if (satoshi !== undefined && satoshi !== null) {
            param.satoshi = toU64BigInt("satoshi", satoshi);
        }
        if (chain_id !== undefined && chain_id !== null) {
            param.chain_id = toU64BigInt("chain_id", chain_id);
        }

        return param;
    };

    const create_sign_tx_param = (input) => {
        if (isInstanceOf(input, rawApi.SignTxParam)) {
            return input;
        }
        const src = ensureObjectParam("create_sign_tx_param", input);
        const param = new rawApi.SignTxParam();
        const prikey = pickField(src, ["prikey"]);
        const body = pickField(src, ["body"]);
        const hybrid_keystore = pickField(src, ["hybrid_keystore"]);
        const keystore_pass = pickField(src, ["keystore_pass"]);
        if (prikey !== undefined && prikey !== null) {
            param.prikey = String(prikey);
        }
        if (body !== undefined && body !== null) {
            param.body = String(body);
        }
        if (hybrid_keystore !== undefined && hybrid_keystore !== null) {
            param.hybrid_keystore = String(hybrid_keystore);
        }
        if (keystore_pass !== undefined && keystore_pass !== null) {
            param.keystore_pass = String(keystore_pass);
        }
        return param;
    };

    const create_sign_tx_v4_param = (input) => {
        if (isInstanceOf(input, rawApi.SignTxV4Param)) {
            return input;
        }
        const src = ensureObjectParam("create_sign_tx_v4_param", input);
        const param = new rawApi.SignTxV4Param();
        const body = pickField(src, ["body"]);
        const hybrid_keystore = pickField(src, ["hybrid_keystore"]);
        const keystore_pass = pickField(src, ["keystore_pass"]);
        if (body !== undefined && body !== null) {
            param.body = String(body);
        }
        if (hybrid_keystore !== undefined && hybrid_keystore !== null) {
            param.hybrid_keystore = String(hybrid_keystore);
        }
        if (keystore_pass !== undefined && keystore_pass !== null) {
            param.keystore_pass = String(keystore_pass);
        }
        return param;
    };

    const create_coin_transfer_v4_param = (input) => {
        if (isInstanceOf(input, rawApi.CoinTransferV4Param)) {
            return input;
        }
        const src = ensureObjectParam("create_coin_transfer_v4_param", input);
        const param = new rawApi.CoinTransferV4Param();
        const main_keystore = pickField(src, ["main_keystore"]);
        const keystore_pass = pickField(src, ["keystore_pass"]);
        const fee = pickField(src, ["fee"]);
        const to_address = pickField(src, ["to_address"]);
        const hacash = pickField(src, ["hacash"]);
        const timestamp = pickField(src, ["timestamp"]);
        const gas_max = pickField(src, ["gas_max"]);
        if (main_keystore !== undefined && main_keystore !== null) {
            param.main_keystore = String(main_keystore);
        }
        if (keystore_pass !== undefined && keystore_pass !== null) {
            param.keystore_pass = String(keystore_pass);
        }
        if (fee !== undefined && fee !== null) {
            param.fee = String(fee);
        }
        if (to_address !== undefined && to_address !== null) {
            param.to_address = String(to_address);
        }
        if (hacash !== undefined && hacash !== null) {
            param.hacash = String(hacash);
        }
        if (timestamp !== undefined && timestamp !== null) {
            param.timestamp = toU64BigInt("timestamp", timestamp);
        }
        if (gas_max !== undefined && gas_max !== null) {
            param.gas_max = Number(gas_max);
        }
        return param;
    };

    const create_coin_transfer = (input) => rawApi.create_coin_transfer(create_coin_transfer_param(input));
    const create_coin_transfer_v4 = (input) => rawApi.create_coin_transfer_v4(create_coin_transfer_v4_param(input));
    const sign_transaction = (input) => rawApi.sign_transaction(create_sign_tx_param(input));
    const sign_transaction_v4 = (input) => rawApi.sign_transaction_v4(create_sign_tx_v4_param(input));

    const sdk = {
        env,
        raw: rawApi,

        account_class: rawApi.Account,
        coin_transfer_param_class: rawApi.CoinTransferParam,
        coin_transfer_result_class: rawApi.CoinTransferResult,
        sign_tx_param_class: rawApi.SignTxParam,
        sign_tx_v4_param_class: rawApi.SignTxV4Param,
        sign_tx_result_class: rawApi.SignTxResult,
        verify_address_result_class: rawApi.VerifyAddressResult,
        coin_transfer_v4_param_class: rawApi.CoinTransferV4Param,
        hybrid_account_info_class: rawApi.HybridAccountInfo,
        hybrid_account_with_keystore_class: rawApi.HybridAccountWithKeystore,
        hybrid_keystore_export_class: rawApi.HybridKeystoreExport,

        to_u64_bigint: (field, value) => toU64BigInt(field, value),
        create_coin_transfer_param,
        create_coin_transfer_v4_param,
        create_sign_tx_param,
        create_sign_tx_v4_param,

        create_account: rawApi.create_account,
        create_pqc_account: rawApi.create_pqc_account,
        create_hybrid_account: rawApi.create_hybrid_account,
        create_hybrid_from_privakey: rawApi.create_hybrid_from_privakey,
        create_pqc_account_keystore: rawApi.create_pqc_account_keystore,
        create_hybrid_account_keystore: rawApi.create_hybrid_account_keystore,
        export_hybrid_keystore: rawApi.export_hybrid_keystore,
        unlock_hybrid_keystore: rawApi.unlock_hybrid_keystore,
        address_version_label: rawApi.address_version_label,
        hac_to_unit: rawApi.hac_to_unit,
        hac_to_mei: rawApi.hac_to_mei,
        verify_address: rawApi.verify_address,
        create_coin_transfer,
        create_coin_transfer_v4,
        sign_transaction,
        sign_transaction_v4,
    };

    return sdk;
}

async function loadNodeBackend() {
    const moduleApi = await import(new URL("../nodejs/hacashsdk.js", import.meta.url));
    return pickBackendApi(moduleApi);
}

async function loadWebBackend(initInput) {
    const moduleApi = await import(new URL("../web/hacashsdk.js", import.meta.url));
    if (typeof moduleApi.default === "function") {
        if (initInput === undefined) {
            await moduleApi.default();
        } else {
            await moduleApi.default(initInput);
        }
    }
    return pickBackendApi(moduleApi);
}

export async function create_hacash_sdk(options = {}) {
    const mode = normalizeTarget(options.target);
    const runtimeIsNode = isNodeRuntime();
    const target = mode === "auto" ? (runtimeIsNode ? "node" : "web") : mode;
    if (target === "node" && !runtimeIsNode) {
        throw new Error("node target requested in non-node runtime");
    }
    const backend = target === "node"
        ? await loadNodeBackend()
        : await loadWebBackend(options.web_init_input ?? options.wasm);
    return createFriendlyApi(backend, target);
}

export default create_hacash_sdk;
