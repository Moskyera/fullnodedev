//! Shared helpers for the pool spikes: HTTP glue + off-node block assembly that
//! mirrors the node's `impl_packing_next_block` for a block containing a
//! coinbase plus optional extra transactions. Targets a fresh local testnet
//! (bootstrap LOWEST_DIFFICULTY); does not reproduce mainnet ASERT difficulty.

pub mod difficulty;
pub mod pool_core;

use difficulty::ChainParams;

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

use basis::difficulty::*;
use basis::interface::*;
use field::*;
use protocol::block::*;
use protocol::transaction::*;
use sys::*;

use serde_json::Value;
use zeroize::Zeroizing;

pub fn http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .expect("http client")
}

pub fn get_json(client: &reqwest::blocking::Client, url: &str) -> Value {
    let text = client
        .get(url)
        .send()
        .and_then(|r| r.text())
        .unwrap_or_else(|e| format!("{{\"http_error\":\"{e}\"}}"));
    serde_json::from_str(&text).unwrap_or_else(|_| Value::String(text))
}

pub fn post_hex(client: &reqwest::blocking::Client, url: &str, body: &str) -> String {
    client
        .post(url)
        .header("content-type", "text/plain")
        .body(body.to_string())
        .send()
        .and_then(|r| r.text())
        .unwrap_or_else(|e| format!("http_error: {e}"))
}

pub fn find_u64(v: &Value, key: &str) -> Option<u64> {
    find_value(v, key).and_then(|x| {
        x.as_u64()
            .or_else(|| x.as_str().and_then(|s| s.trim().parse().ok()))
    })
}

pub fn find_str(v: &Value, key: &str) -> Option<String> {
    find_value(v, key).and_then(|x| x.as_str().map(|s| s.to_string()))
}

pub fn find_value<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(map) => map
            .get(key)
            .or_else(|| map.values().find_map(|child| find_value(child, key))),
        Value::Array(arr) => arr.iter().find_map(|child| find_value(child, key)),
        _ => None,
    }
}

/// The recipient's "hacash" balance string (e.g. "1:248"), or "" if none.
pub fn balance(client: &reqwest::blocking::Client, base: &str, addr: &str) -> String {
    let j = get_json(client, &format!("{base}/query/balance?address={addr}"));
    find_str(&j, "hacash").unwrap_or_default()
}

/// The largest balance the pool will act on, in units of 0.1 HAC. Hacash's whole
/// coin supply is tens of millions of HAC, so anything past 100 billion HAC is a
/// corrupt or hostile answer, not a wallet. Refusing it keeps a bad number out of
/// the payout split instead of turning it into a maximal payout plan.
pub const MAX_PLAUSIBLE_UNITS: u64 = 1_000_000_000_000;

/// A node "mantissa:unit" balance expressed in whole units of 0.1 HAC (unit 247).
///
/// Hacash stores amounts normalized (trailing zeros stripped, unit raised), so a
/// balance like 4.9 HAC comes back as "49:246", not "490:247". FLOOR to 0.1-HAC
/// granularity, keeping the whole part, rather than discarding a balance just
/// because it is finer than 0.1 HAC. Shared by the pool server and the payout
/// tool so both value a balance identically.
///
/// `None` means the node's answer was missing a separator, unparseable, or
/// larger than any real wallet: the caller must SKIP settlement rather than pay
/// out on it. Saturating to u64::MAX here (as this used to) means "infinite
/// money" to `distributable_units` and `split_payout`, which then plan a payout
/// of the whole u64 range off one malformed response. An EMPTY string is not an
/// error: the node simply omits the field for an address holding nothing.
pub fn balance_units(bal: &str) -> Option<u64> {
    if bal.trim().is_empty() {
        return Some(0);
    }
    let (m, u) = bal.split_once(':')?;
    let (Ok(m), Ok(u)) = (m.trim().parse::<u64>(), u.trim().parse::<i64>()) else {
        return None;
    };
    let units = if u >= 247 {
        let exp = u - 247;
        if exp > 18 {
            return None; // beyond any representable wallet, not a big balance
        }
        m.checked_mul(10u64.pow(exp as u32))?
    } else {
        let exp = 247 - u;
        if exp > 18 {
            return Some(0); // finer than 0.1 HAC: floors to nothing payable
        }
        m / 10u64.pow(exp as u32)
    };
    (units <= MAX_PLAUSIBLE_UNITS).then_some(units)
}

/// The coinbase subsidy of the block at `height`, in units of 0.1 HAC. The pool
/// mines coinbase-only blocks, so this is the entire income a found block brings
/// into the wallet (`block_reward` is a whole number of HAC = unit 248).
pub fn block_reward_units(height: u64) -> u64 {
    mint::genesis::block_reward_number(height) as u64 * 10
}

/// How deep a payout transaction must be buried before the pool stops tracking
/// it. The node keeps up to `unstable_block` (4) blocks reorg-able, so a payout
/// that is only 1-3 confirmations deep can still come back to the mempool;
/// forgetting it that early lets the next cycle pay the same PPLNS window a
/// second time. 6 keeps a margin over the node's own window.
pub const PAYOUT_MATURITY_DEPTH: u64 = 6;

/// What the node says about a payout transaction we previously submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayoutTxState {
    /// Still waiting in the mempool.
    Pending,
    /// Mined, but shallower than [`PAYOUT_MATURITY_DEPTH`] - a reorg could still
    /// put it back in the mempool, so it is not finished with.
    Confirming(u64),
    /// Mined and buried deep enough that a reorg cannot undo it.
    Buried(u64),
    /// The node definitively does not know this hash: it was rejected, never
    /// relayed, or dropped from the mempool. Settling again is the right move.
    Gone,
    /// We could not reach the node, or could not understand its answer. This is
    /// NOT a resolution: treating it as one is exactly what opens a double-payout
    /// window, so the caller must keep the hash and skip the cycle.
    Unknown,
}

/// Classify a `/query/transaction?hash=...` response. Fails SAFE: anything that
/// is not an unambiguous verdict from the node comes back as `Unknown`, and a
/// shallow confirmation counts as still in flight.
pub fn classify_payout_tx(j: &Value) -> PayoutTxState {
    // get_json encodes a transport failure as {"http_error": "..."} and a
    // non-JSON body as a bare string. Neither is the node speaking.
    if !j.is_object() || j.get("http_error").is_some() {
        return PayoutTxState::Unknown;
    }
    let Some(ret) = find_u64(j, "ret") else {
        return PayoutTxState::Unknown;
    };
    if ret != 0 {
        return PayoutTxState::Gone; // the node answered "transaction not found"
    }
    let is_pending = j
        .get("data")
        .and_then(|d| d.get("pending"))
        .and_then(|v| v.as_bool())
        .or_else(|| j.get("pending").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    if is_pending {
        return PayoutTxState::Pending;
    }
    // ret=0 and not pending means mined; the node reports the burial depth.
    match find_u64(j, "confirm") {
        Some(d) if d >= PAYOUT_MATURITY_DEPTH => PayoutTxState::Buried(d),
        Some(d) => PayoutTxState::Confirming(d),
        // ret=0 with neither `pending` nor `confirm` is a shape we do not
        // recognise; unresolved is the safe reading.
        None => PayoutTxState::Unknown,
    }
}

/// What a settlement may actually pay out: the wallet balance MINUS income a
/// reorg could still take back, MINUS the fee reserve. `None` means "nothing
/// spendable, do not settle this cycle".
///
/// `immature_units` is the coinbase of blocks the pool found that are not yet
/// buried deep enough to be final. Distributing that and then losing the block
/// to a reorg is an unrecoverable operator loss: the income disappears from the
/// canonical chain while the payout transaction that spent it stays valid.
///
/// All arithmetic saturates, so an out-of-range reserve can never wrap the
/// guard open the way `reserve + 1` used to.
pub fn distributable_units(
    balance_units: u64,
    immature_units: u64,
    reserve_units: u64,
) -> Option<u64> {
    let matured = balance_units.saturating_sub(immature_units);
    if matured <= reserve_units.saturating_add(1) {
        return None;
    }
    Some(matured - reserve_units)
}

/// Atomic file write (temp + optional fsync + rename) so a crash or a full disk
/// mid-write can never leave a truncated or corrupt file behind. `durable`
/// fsyncs before the rename.
pub fn atomic_write(path: &str, body: &[u8], durable: bool) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = format!("{path}.tmp.{}", std::process::id());
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body)?;
        if durable {
            let _ = f.sync_all();
        }
    }
    std::fs::rename(&tmp, path)
}

/// The pool's accounting file for `wallet_file`. The auto-settle server and the
/// manual payout tool MUST agree on this path: it carries the ONE pending-payout
/// ledger that stops the two of them paying the same PPLNS window twice.
pub fn pool_state_path(wallet_file: &str) -> String {
    format!("{wallet_file}.state.json")
}

fn read_state_json(state_file: &str) -> Option<Value> {
    let txt = std::fs::read_to_string(state_file).ok()?;
    let j: Value = serde_json::from_str(&txt).ok()?;
    j.is_object().then_some(j)
}

/// The shared pending-payout ledger. A missing or corrupt file reads as an empty
/// ledger (the server rewrites that file wholesale and reports the corruption).
pub fn load_pending_payout_txs(state_file: &str) -> Vec<String> {
    let Some(j) = read_state_json(state_file) else {
        return Vec::new();
    };
    j.get("settle_pending_txs")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Rolling PPLNS window: the last N accepted shares decide the payout split.
pub const PPLNS_WINDOW: usize = 4096;

/// Rebuild the PPLNS share counts from the pool's own accounting file.
///
/// The manual payout tool needs this because the server holds the wallet's
/// settlement lock for its whole run: if the tool is able to settle at all then
/// the server is stopped, so its `/stats` endpoint cannot answer and the file it
/// left behind is the authority on who is owed what.
pub fn load_pplns_counts(state_file: &str) -> Vec<(String, u64)> {
    let Some(j) = read_state_json(state_file) else {
        return Vec::new();
    };
    let window = j
        .get("window")
        .and_then(|v| v.as_u64())
        .unwrap_or(PPLNS_WINDOW as u64) as usize;
    let order: Vec<String> = j
        .get("order")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if order.is_empty() {
        return Vec::new();
    }
    pool_core::Pplns::restore(window, order).counts()
}

/// Total held-back (not yet final) block income recorded by the pool server, in
/// units of 0.1 HAC. The manual payout tool reads it so it applies the SAME
/// maturity gate as the automatic settlement instead of paying at the tip.
pub fn load_immature_units(state_file: &str) -> u64 {
    let Some(j) = read_state_json(state_file) else {
        return 0;
    };
    j.get("immature")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("units").and_then(|v| v.as_u64()))
                .sum()
        })
        .unwrap_or(0)
}

/// Replace `settle_pending_txs` in the pool state file, preserving every other
/// field the server keeps there (share window, counters, immature income).
pub fn save_pending_payout_txs(state_file: &str, hashes: &[String]) -> std::io::Result<()> {
    let mut j = read_state_json(state_file).unwrap_or_else(|| serde_json::json!({}));
    j["settle_pending_txs"] = serde_json::json!(hashes);
    atomic_write(state_file, j.to_string().as_bytes(), true)
}

/// The lock file guarding one wallet's settlement.
pub fn settle_lock_path(wallet_file: &str) -> String {
    format!("{wallet_file}.settle.lock")
}

/// An exclusive, cross-process claim on one wallet's settlement, held for as
/// long as the value lives. The OS releases it if the holder dies, so a crash
/// can never wedge payouts the way a hand-rolled PID file would.
pub struct SettleLock {
    _file: std::fs::File,
}

/// Take the wallet's settlement lock, or fail if another process holds it.
///
/// The pool server takes this for its whole run and `pool-payout` takes it for
/// its whole run. Without it the two paths each see the full CONFIRMED balance
/// (a payout sitting in the mempool does not reduce it) and each pays the same
/// PPLNS window - a real double payout of the pool's distributable balance.
pub fn acquire_settle_lock(wallet_file: &str) -> std::io::Result<SettleLock> {
    let path = settle_lock_path(wallet_file);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    // Call it as a trait function so it can never be confused with a same-named
    // inherent method on File.
    fs2::FileExt::try_lock_exclusive(&file)?;
    Ok(SettleLock { _file: file })
}

/// Is this string a payable Hacash address (normal single-key PRIVAKEY)?
/// Workers announce one as `&worker=<address>`; the pool then uses the address
/// itself as the share-accounting key, so payouts need no name->address map.
pub fn is_payout_address(s: &str) -> bool {
    Address::from_readable(s)
        .map(|a| a.is_privakey())
        .unwrap_or(false)
}

/// Environment variable holding the pool wallet passphrase. When it is set the
/// key file is stored ENCRYPTED (Argon2id + AES-256-GCM), so a stolen backup, a
/// VSS/disk snapshot or a decommissioned drive is inert without the passphrase.
pub const WALLET_PASSWORD_ENV: &str = "HBIT_WALLET_PASSWORD";
/// Alternative source for that passphrase: a file holding it, for services that
/// cannot carry secrets in the environment.
pub const WALLET_PASSWORD_FILE_ENV: &str = "HBIT_WALLET_PASSWORD_FILE";

const WALLET_ENVELOPE_VERSION: u64 = 1;
const WALLET_KDF_M_COST_KB: u32 = 19456;
const WALLET_KDF_T_COST: u32 = 2;
const WALLET_KDF_P_COST: u32 = 1;
/// Upper bounds on the KDF parameters read back from a file, so a tampered
/// envelope cannot turn a startup into an out-of-memory or an endless grind.
const WALLET_KDF_MAX_M_COST_KB: u32 = 256 * 1024;
const WALLET_KDF_MAX_T_COST: u32 = 16;
const WALLET_KDF_MAX_P_COST: u32 = 16;

/// The configured wallet passphrase, or None for the (loudly warned about)
/// plaintext mode. A passphrase under 8 characters is refused outright rather
/// than silently weakening the only thing protecting the pool's funds.
fn wallet_password() -> Option<Zeroizing<String>> {
    let mut pass = Zeroizing::new(std::env::var(WALLET_PASSWORD_ENV).unwrap_or_default());
    if pass.is_empty()
        && let Ok(f) = std::env::var(WALLET_PASSWORD_FILE_ENV)
    {
        match std::fs::read_to_string(&f) {
            Ok(t) => pass = Zeroizing::new(t.trim().to_string()),
            Err(e) => panic!("cannot read {WALLET_PASSWORD_FILE_ENV} ({f}): {e}"),
        }
    }
    if pass.is_empty() {
        return None;
    }
    if pass.len() < 8 {
        panic!("the wallet passphrase in {WALLET_PASSWORD_ENV} must be at least 8 characters");
    }
    Some(pass)
}

fn wallet_derive_key(
    pass: &str,
    salt: &[u8],
    m_cost_kb: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<Zeroizing<[u8; 32]>, String> {
    use argon2::{Algorithm, Argon2, Params, Version};
    let params = Params::new(m_cost_kb, t_cost, p_cost, Some(32))
        .map_err(|e: argon2::Error| e.to_string())?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(pass.as_bytes(), salt, &mut *key)
        .map_err(|e: argon2::Error| e.to_string())?;
    Ok(key)
}

/// Wrap a 64-hex private key in a versioned Argon2id + AES-256-GCM envelope.
fn encrypt_key_hex(key_hex: &str, pass: &str) -> Result<String, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut salt).map_err(|e| e.to_string())?;
    getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
    let key = wallet_derive_key(
        pass,
        &salt,
        WALLET_KDF_M_COST_KB,
        WALLET_KDF_T_COST,
        WALLET_KDF_P_COST,
    )?;
    let cipher = Aes256Gcm::new_from_slice(&*key).map_err(|e| e.to_string())?;
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), key_hex.as_bytes())
        .map_err(|e: aes_gcm::Error| e.to_string())?;
    Ok(serde_json::json!({
        "hbit_wallet": WALLET_ENVELOPE_VERSION,
        "kdf": "argon2id",
        "kdf_salt": hex::encode(salt),
        "kdf_m_cost_kb": WALLET_KDF_M_COST_KB,
        "kdf_t_cost": WALLET_KDF_T_COST,
        "kdf_p_cost": WALLET_KDF_P_COST,
        "cipher": "aes-256-gcm",
        "cipher_nonce": hex::encode(nonce),
        "ciphertext": hex::encode(ciphertext),
    })
    .to_string())
}

fn envelope_u32(j: &Value, key: &str, default: u32, max: u32) -> Result<u32, String> {
    let v = j.get(key).and_then(|v| v.as_u64()).unwrap_or(default as u64);
    if v == 0 || v > max as u64 {
        return Err(format!("{key} out of range"));
    }
    Ok(v as u32)
}

/// Unwrap an envelope written by [`encrypt_key_hex`].
fn decrypt_key_hex(body: &str, pass: &str) -> Result<Zeroizing<String>, String> {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    let j: Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let ver = j.get("hbit_wallet").and_then(|v| v.as_u64()).unwrap_or(0);
    if ver != WALLET_ENVELOPE_VERSION {
        return Err(format!("unsupported wallet file version {ver}"));
    }
    let hex_field = |k: &str| -> Result<Vec<u8>, String> {
        let s = j
            .get(k)
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("wallet file is missing `{k}`"))?;
        hex::decode(s).map_err(|_| format!("wallet file field `{k}` is not hex"))
    };
    let salt = hex_field("kdf_salt")?;
    let nonce = hex_field("cipher_nonce")?;
    let ciphertext = hex_field("ciphertext")?;
    if nonce.len() != 12 {
        return Err("wallet file nonce must be 12 bytes".to_string());
    }
    let key = wallet_derive_key(
        pass,
        &salt,
        envelope_u32(&j, "kdf_m_cost_kb", WALLET_KDF_M_COST_KB, WALLET_KDF_MAX_M_COST_KB)?,
        envelope_u32(&j, "kdf_t_cost", WALLET_KDF_T_COST, WALLET_KDF_MAX_T_COST)?,
        envelope_u32(&j, "kdf_p_cost", WALLET_KDF_P_COST, WALLET_KDF_MAX_P_COST)?,
    )?;
    let cipher = Aes256Gcm::new_from_slice(&*key).map_err(|e| e.to_string())?;
    let plain = cipher
        .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
        .map(Zeroizing::new)
        .map_err(|_e: aes_gcm::Error| "wrong passphrase or corrupted wallet file".to_string())?;
    let txt = String::from_utf8(plain.to_vec())
        .map(Zeroizing::new)
        .map_err(|_| "decrypted wallet content is not text".to_string())?;
    Ok(txt)
}

/// True the FIRST time this (tag, path) pair comes up in this process. The
/// settlement loop reloads the wallet on every cycle, so once-per-wallet work
/// and warnings must not repeat with it.
fn first_time_for(tag: &str, path: &str) -> bool {
    static SEEN: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    seen.insert(format!("{tag}:{path}"))
}

/// Say plainly what an unencrypted key file costs. Printed once per path per
/// process so it cannot be lost in the settlement loop's output.
fn warn_plaintext_wallet(path: &str) {
    if !first_time_for("plaintext-warned", path) {
        return;
    }
    eprintln!(
        "[wallet] WARNING: {path} holds the pool's private key in PLAINTEXT. Anything that can\n\
         [wallet] read those bytes - a backup, a VSS or disk snapshot, an old drive - can spend\n\
         [wallet] every coin the pool holds. Set {WALLET_PASSWORD_ENV} (or {WALLET_PASSWORD_FILE_ENV})\n\
         [wallet] and restart: the file is then re-written encrypted (Argon2id + AES-256-GCM)."
    );
}

/// Load the pool wallet from `path`, creating a fresh random one if the file
/// does not exist. The file holds either a 64-hex secp256k1 private key or, when
/// a passphrase is configured, an encrypted envelope. The private key only ever
/// lives in that file: it is never printed or logged; only the address is shown.
pub fn load_or_create_wallet(path: &str) -> Account {
    match std::fs::read_to_string(path) {
        Ok(txt) => {
            let acc = account_from_wallet_file(path, &txt);
            // Re-apply and re-verify the owner-only permissions on the LOAD path
            // too, not only at creation: a key that lost its ACL (restored from a
            // backup, copied by hand) must not keep serving funds.
            secure_existing_key_file(path);
            println!("pool wallet {} (from {path})", acc.readable());
            acc
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // No wallet yet: generate one and persist it owner-only.
            let acc = loop {
                let mut key = [0u8; 32];
                getrandom::fill(&mut key).expect("system RNG");
                if let Ok(a) = Account::create_by_secret_key_value(key) {
                    break a;
                }
            };
            let key_hex = Zeroizing::new(hex::encode(acc.secret_key().serialize()));
            let pass = wallet_password();
            let body = match pass.as_deref() {
                Some(p) => match encrypt_key_hex(&key_hex, p) {
                    Ok(b) => b,
                    Err(e2) => panic!("cannot encrypt the new wallet key: {e2}"),
                },
                None => key_hex.to_string(),
            };
            if let Err(e2) = write_key_file(path, &body) {
                // A key we could not protect must never be left lying around.
                let _ = std::fs::remove_file(path);
                panic!("cannot write wallet file {path} securely: {e2}");
            }
            println!("CREATED A NEW POOL WALLET -> {path}");
            println!("  address: {}", acc.readable());
            if pass.is_some() {
                println!("  the file is ENCRYPTED with {WALLET_PASSWORD_ENV}.");
                println!("  BACK UP THAT FILE **AND** THAT PASSPHRASE: neither one alone can spend,");
                println!("  and losing either one loses the pool's funds for good.");
            } else {
                println!("  BACK UP THAT FILE. Whoever holds it controls the pool's funds.");
                warn_plaintext_wallet(path);
            }
            acc
        }
        // Never generate-and-overwrite on a non-NotFound error: a locked or
        // transiently-unreadable key file must not be silently replaced.
        Err(e) => panic!("cannot read wallet file {path}: {e} (refusing to overwrite it)"),
    }
}

/// Turn the wallet file's contents into an Account, transparently handling both
/// the encrypted envelope and the legacy plaintext-hex form.
fn account_from_wallet_file(path: &str, txt: &str) -> Account {
    let body = txt.trim();
    if body.starts_with('{') {
        let Some(pass) = wallet_password() else {
            panic!(
                "wallet file {path} is encrypted but no passphrase is configured; \
                 set {WALLET_PASSWORD_ENV} (or {WALLET_PASSWORD_FILE_ENV})"
            );
        };
        let key_hex = match decrypt_key_hex(body, &pass) {
            Ok(k) => k,
            Err(e) => panic!("cannot decrypt wallet file {path}: {e}"),
        };
        return Account::create_by(key_hex.trim()).expect("invalid key in wallet file");
    }
    if body.len() != 64 {
        panic!("wallet file {path} must hold a 64-hex private key");
    }
    let acc = Account::create_by(body).expect("invalid key in wallet file");
    // Plaintext on disk: move it into an encrypted envelope as soon as a
    // passphrase is configured, otherwise say plainly what is at risk.
    match wallet_password() {
        Some(pass) => migrate_key_file_to_encrypted(path, body, &pass),
        None => warn_plaintext_wallet(path),
    }
    acc
}

/// Re-write a legacy plaintext key file as an encrypted envelope. The envelope
/// is decrypted back and compared BEFORE it replaces the only copy of the key,
/// so a bad envelope can never cost the pool its wallet.
fn migrate_key_file_to_encrypted(path: &str, key_hex: &str, pass: &str) {
    let body = match encrypt_key_hex(key_hex, pass) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[wallet] WARNING: could not encrypt {path} ({e}); it stays plaintext.");
            return;
        }
    };
    match decrypt_key_hex(&body, pass) {
        Ok(back) if back.trim().eq_ignore_ascii_case(key_hex) => {}
        _ => {
            eprintln!("[wallet] WARNING: the encrypted form of {path} did not verify; leaving it as-is.");
            return;
        }
    }
    if let Err(e) = write_key_file(path, &body) {
        panic!(
            "cannot re-write {path} encrypted and owner-only: {e}\n\
             The key controls ALL pool funds, so the pool refuses to run with it unprotected."
        );
    }
    println!("[wallet] {path} is now ENCRYPTED with {WALLET_PASSWORD_ENV}.");
    println!("[wallet] KEEP THAT PASSPHRASE: without it the file cannot be decrypted and the");
    println!("[wallet] pool's funds are unrecoverable. The previous plaintext copy may still");
    println!("[wallet] exist in backups and snapshots - treat those as sensitive.");
}

/// Re-apply and verify the key file's owner-only permissions, once per path per
/// process. Settlement reloads the wallet every cycle, and spawning icacls each
/// time would be pointless work that a transient hiccup could turn into a
/// skipped payout.
fn secure_existing_key_file(path: &str) {
    if !first_time_for("secured", path) {
        return;
    }
    if let Err(e) = restrict_key_file_permissions(path) {
        panic!(
            "cannot secure wallet file {path}: {e}\n\
             The key controls ALL pool funds, so the pool refuses to run with it unprotected."
        );
    }
}

/// Write the wallet file owner-only via a temp file + atomic rename, so a
/// concurrent reader never sees an empty or half-written key. The file controls
/// ALL pool funds, so securing it is MANDATORY: if the owner-only permissions
/// cannot be applied AND verified this returns Err, and the caller must not keep
/// running with an unprotected key.
fn write_key_file(path: &str, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = format!("{path}.tmp.{}", std::process::id());
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        // Harden the (still empty) temp file BEFORE the secret reaches it. On
        // Windows it is created with the directory's inherited ACL and NTFS
        // carries that ACL across the rename, so hardening only the final path
        // would leave a window in which the key is readable by other accounts.
        #[cfg(windows)]
        if let Err(e) = restrict_key_file_permissions(&tmp) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        writeln!(f, "{body}")?;
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp, path)?;
    restrict_key_file_permissions(path)
}

/// Lock the wallet key down to the current user only, and prove it worked.
/// On Windows the default ACL is inherited and readable by other local accounts;
/// without this the key controlling the pool balance is exposed to any local
/// user or process. Every failure is fatal to the caller: "could not secure the
/// private key" is not a warning for a daemon that moves real money.
fn restrict_key_file_permissions(path: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        // Resolve the principal from the process token, NEVER from USERNAME:
        // that variable is empty under a service or a scheduled task, and
        // `/inheritance:r` with no matching `/grant` leaves an EMPTY DACL that
        // locks the pool out of its own wallet on the very next start.
        let (name, sid) = windows_current_principal()?;
        let out = std::process::Command::new("icacls")
            .arg(path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(format!("*{sid}:F"))
            .output()?;
        if !out.status.success() {
            return Err(std::io::Error::other(format!(
                "icacls failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        // The owner must still be able to READ the key, or every later start
        // dies on "cannot read wallet file".
        drop(std::fs::File::open(path)?);
        windows_verify_owner_only(path, &name, &sid)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode != 0o600 {
            return Err(std::io::Error::other(format!(
                "wallet file mode is {mode:o}, expected 600"
            )));
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        return Err(std::io::Error::other(
            "cannot restrict wallet file permissions on this platform",
        ));
    }
    Ok(())
}

/// The current account's `DOMAIN\name` and SID, read from the process token via
/// `whoami /user`. Granting by SID keeps the ACL correct even where the display
/// name is ambiguous, and it never depends on the USERNAME variable.
#[cfg(windows)]
fn windows_current_principal() -> std::io::Result<(String, String)> {
    let out = std::process::Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other("`whoami /user` failed"));
    }
    let txt = String::from_utf8_lossy(&out.stdout);
    let line = txt.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    // CSV is `"DOMAIN\user","S-1-5-..."`; a Windows account name cannot contain
    // a comma, so a plain split is safe.
    let mut cols = line.split(',').map(|c| c.trim().trim_matches('"'));
    let name = cols.next().unwrap_or("").to_string();
    let sid = cols.next().unwrap_or("").to_string();
    if name.is_empty() || !sid.starts_with("S-1-") {
        return Err(std::io::Error::other(
            "could not resolve the current account SID from `whoami /user`",
        ));
    }
    Ok((name, sid))
}

/// Read the DACL back and refuse to continue if any principal other than the
/// current account is listed. Parsing icacls output is best-effort, so an
/// unreadable listing only warns: the mandatory checks in the caller (icacls
/// exit status plus the file still being readable) already rule out the
/// empty-DACL and silent-failure cases this guards against.
#[cfg(windows)]
fn windows_verify_owner_only(path: &str, name: &str, sid: &str) -> std::io::Result<()> {
    let unverified = |why: &str| {
        eprintln!("[wallet] WARNING: could not verify the ACL of {path} ({why}); check it manually.");
    };
    let Ok(out) = std::process::Command::new("icacls").arg(path).output() else {
        unverified("icacls did not run");
        return Ok(());
    };
    if !out.status.success() {
        unverified("icacls reported an error");
        return Ok(());
    }
    let txt = String::from_utf8_lossy(&out.stdout);
    let mut aces = 0usize;
    for (i, raw) in txt.lines().enumerate() {
        if raw.trim().is_empty() {
            break; // a blank line ends the ACE list
        }
        let entry = if i == 0 {
            // The first line echoes the path we passed, then the first ACE.
            match raw.trim_start().strip_prefix(path) {
                Some(rest) => rest.trim(),
                None => {
                    unverified("unexpected output layout");
                    return Ok(());
                }
            }
        } else {
            raw.trim()
        };
        let Some((principal, _)) = entry.split_once(":(") else {
            continue;
        };
        aces += 1;
        if !principal.eq_ignore_ascii_case(name) && !principal.eq_ignore_ascii_case(sid) {
            return Err(std::io::Error::other(format!(
                "{path} is still accessible to `{principal}`"
            )));
        }
    }
    if aces == 0 {
        unverified("no access entries parsed");
    }
    Ok(())
}

/// Everything the pool needs to build and verify blocks for the current tip.
/// The pool serves one template to all workers; each worker gets its own
/// extranonce (the coinbase `miner_nonce`), which changes the merkle root and
/// therefore gives every worker a private search space.
#[derive(Clone)]
pub struct Template {
    pub height: u64,
    pub prevhash: Hash,
    pub timestamp: u64,
    /// Header `difficulty` field (u32) — must equal what the node recomputes.
    pub difficulty: u32,
    /// The exact PoW target for this block. NOT interchangeable with
    /// u32_to_hash(difficulty): on the from_big path it is more precise.
    pub target: [u8; 32],
    pub coinbase_addr: Address,
}

/// Read the chain tip and build a template for the next block, computing the
/// next difficulty off-node with the same rule the node will validate against.
///
/// Returns `None` on any transient node/HTTP problem instead of panicking, so a
/// caller holding a lock (the pool server) can skip the cycle and retry rather
/// than poisoning its mutex and taking the whole pool down.
pub fn fetch_template(
    client: &reqwest::blocking::Client,
    base: &str,
    coinbase_addr: &str,
    params: &ChainParams,
) -> Option<Template> {
    let coinbase = Address::from_readable(coinbase_addr).ok()?;
    let latest = get_json(client, &format!("{base}/query/latest"));
    let prev_hei = find_u64(&latest, "height")?;
    let height = prev_hei + 1;
    let (prevhash, prev_ts, prev_diff) = if prev_hei == 0 {
        (mint::genesis::genesis_block_hash(), 1549250700u64, 0u32)
    } else {
        let ij = get_json(client, &format!("{base}/query/block/intro?height={prev_hei}"));
        let ph = find_str(&ij, "hash")?;
        (
            Hash::from_hex(ph.as_bytes()).ok()?,
            find_u64(&ij, "timestamp")?,
            find_u64(&ij, "difficulty")? as u32,
        )
    };
    let timestamp = std::cmp::max(curtimes(), prev_ts.saturating_add(1));
    // ASERT anchors on the activation block's timestamp; only needed above it.
    let anchor_time = if params.needs_anchor(height) {
        let aj = get_json(
            client,
            &format!("{base}/query/block/intro?height={}", params.asert_height),
        );
        find_u64(&aj, "timestamp")?
    } else {
        0
    };
    let (diff_num, target) =
        difficulty::next_difficulty(params, height, timestamp, prev_diff, anchor_time);
    Some(Template {
        height,
        prevhash,
        timestamp,
        difficulty: diff_num,
        target,
        coinbase_addr: coinbase,
    })
}

/// Prove the off-node difficulty rule agrees with the node BEFORE mining on it.
///
/// A chain selector carries only a name, while the node reads
/// `difficulty_adjust_blocks` and `each_block_target_time` from its own config
/// file. Mainnet fixes both by consensus, but a testnet configured with any
/// other pair recomputes a different difficulty for every block and rejects
/// everything the pool mines - silently, forever. So recompute the difficulty of
/// the node's OWN tip from its stored data and compare against what it stored:
/// an exact match is the only proof that the parameters in force here are the
/// ones the node validates with.
pub fn verify_chain_params(
    client: &reqwest::blocking::Client,
    base: &str,
    params: &ChainParams,
) -> Result<(), String> {
    let latest = get_json(client, &format!("{base}/query/latest"));
    let Some(tip) = find_u64(&latest, "height") else {
        return Err("could not read the chain tip from the node".to_string());
    };
    if tip == 0 {
        return Ok(()); // empty chain: the node has stored nothing to compare to
    }
    if tip > params.bootstrap_max && tip < params.asert_height {
        return Err(format!(
            "the node's tip {tip} is in the pre-ASERT range this pool does not implement \
             (ASERT anchors at {}); a pool only mines at the tip, so wait for the node to \
             sync past that height - or pass the chain the node is really running",
            params.asert_height
        ));
    }
    let intro = |h: u64| get_json(client, &format!("{base}/query/block/intro?height={h}"));
    let b = intro(tip);
    let (Some(ts), Some(stored)) = (find_u64(&b, "timestamp"), find_u64(&b, "difficulty")) else {
        return Err(format!("could not read block {tip} from the node"));
    };
    let prev_diff = if tip > 1 {
        match find_u64(&intro(tip - 1), "difficulty") {
            Some(d) => d as u32,
            None => return Err(format!("could not read block {} from the node", tip - 1)),
        }
    } else {
        0
    };
    let anchor_time = if params.needs_anchor(tip) {
        match find_u64(&intro(params.asert_height), "timestamp") {
            Some(t) => t,
            None => {
                return Err(format!(
                    "could not read the ASERT anchor block {} from the node",
                    params.asert_height
                ));
            }
        }
    } else {
        0
    };
    let (ours, _) = difficulty::next_difficulty(params, tip, ts, prev_diff, anchor_time);
    if ours as u64 != stored {
        return Err(format!(
            "difficulty rule mismatch at the node's own tip {tip}: this pool computes {ours}, \
             the chain stored {stored}. Every block mined against these parameters would be \
             rejected. For a testnet, pass the node's real config as \
             `testnet:<difficulty_adjust_blocks>:<each_block_target_time>`"
        ));
    }
    Ok(())
}

/// The 16-byte message stamped into every block this pool mines, tagging it as
/// HBIT. Fixed16 needs exactly 16 bytes, so the tag is space-padded.
pub fn coinbase_message() -> Fixed16 {
    Fixed16::from_readable(b"HBIT pool       ").unwrap_or_default()
}

/// The template's coinbase carrying `extranonce` in its miner_nonce field.
pub fn coinbase_with_extranonce(tpl: &Template, extranonce: &[u8; 32]) -> mint::TransactionCoinbase {
    let mut cb =
        mint::create_coinbase_tx(tpl.height, coinbase_message(), tpl.coinbase_addr.clone());
    let en = Hash::from_hex(hex::encode(extranonce).as_bytes()).expect("extranonce");
    cb.extend = mint::CoinbaseExtend::must(mint::CoinbaseExtendDataV1 {
        miner_nonce: en,
        witness_count: Uint1::from(0),
    });
    cb
}

fn build_intro(tpl: &Template, cb: &mint::TransactionCoinbase, nonce: u32) -> BlockIntro {
    BlockIntro {
        head: BlockHead {
            version: Uint1::from(1),
            height: BlockHeight::from(tpl.height),
            timestamp: Timestamp::from(tpl.timestamp),
            prevhash: tpl.prevhash.clone(),
            mrklroot: calculate_mrklroot(&vec![cb.hash_with_fee()]),
            transaction_count: Uint4::from(1u32),
        },
        meta: BlockMeta {
            nonce: Uint4::from(nonce),
            difficulty: Uint4::from(tpl.difficulty),
            witness_stage: Fixed2::default(),
        },
    }
}

/// The 89-byte block header a worker hashes (nonce lives at bytes 79..83).
pub fn intro_bytes(tpl: &Template, cb: &mint::TransactionCoinbase, nonce: u32) -> Vec<u8> {
    build_intro(tpl, cb, nonce).serialize()
}

/// Hex of the serialized coinbase tx — the `coinbase_body` a worker receives.
/// Its optional `extend` block must be present or the worker's own
/// `set_mining_nonce` becomes a silent no-op (all threads would then share one
/// coinbase hash); `create_coinbase_tx` always emits it.
pub fn coinbase_body_hex(cb: &mint::TransactionCoinbase) -> String {
    hex::encode(cb.serialize())
}

/// Serialized full block for a winning (extranonce, nonce).
pub fn assemble_block(tpl: &Template, cb: &mint::TransactionCoinbase, nonce: u32) -> Vec<u8> {
    let mut txs = DynVecTransaction::default();
    txs.push(Box::new(cb.clone())).expect("push coinbase");
    BlockV1 {
        intro: build_intro(tpl, cb, nonce),
        transactions: txs,
    }
    .serialize()
}

/// Submit already-serialized block bytes.
pub fn submit_block_bytes(
    client: &reqwest::blocking::Client,
    base: &str,
    bytes: &[u8],
) -> String {
    post_hex(
        client,
        &format!("{base}/submit/block?hexbody=true"),
        &hex::encode(bytes),
    )
}

/// Assemble a block whose coinbase pays `coinbase_addr`, plus `extra_txs`,
/// CPU-mine it at bootstrap difficulty, and submit via /submit/block.
/// Returns (next_height, submit_response).
pub fn mine_and_submit_block(
    client: &reqwest::blocking::Client,
    base: &str,
    coinbase_addr: &str,
    extra_txs: Vec<Box<dyn Transaction>>,
    params: &ChainParams,
) -> (u64, String) {
    let Some(tpl) = fetch_template(client, base, coinbase_addr, params) else {
        return (
            0,
            "{\"ok\":false,\"err\":\"could not fetch a template from the node\"}".to_string(),
        );
    };
    let cbtx = mint::create_coinbase_tx(tpl.height, Fixed16::default(), tpl.coinbase_addr.clone());

    let mut trshxs: Vec<Hash> = vec![cbtx.hash_with_fee()];
    let mut transactions = DynVecTransaction::default();
    transactions.push(Box::new(cbtx.clone())).expect("push coinbase");
    for tx in extra_txs {
        trshxs.push(tx.hash_with_fee());
        transactions.push(tx).expect("push extra tx");
    }
    let count = trshxs.len() as u32;

    let mut intro = BlockIntro {
        head: BlockHead {
            version: Uint1::from(1),
            height: BlockHeight::from(tpl.height),
            timestamp: Timestamp::from(tpl.timestamp),
            prevhash: tpl.prevhash.clone(),
            mrklroot: calculate_mrklroot(&trshxs),
            transaction_count: Uint4::from(count),
        },
        meta: BlockMeta {
            nonce: Uint4::default(),
            difficulty: Uint4::from(tpl.difficulty),
            witness_stage: Fixed2::default(),
        },
    };

    let mut nonce: u32 = 0;
    loop {
        intro.meta.nonce = Uint4::from(nonce);
        let ph = x16rs::block_hash(tpl.height, &intro.serialize());
        if !hash_bigger_than(&ph, &tpl.target) {
            break;
        }
        nonce = nonce.wrapping_add(1);
        if nonce == 0 {
            // Never roll the timestamp here: under ASERT the difficulty is a
            // function of this block's own timestamp, so changing it would make
            // the header's difficulty field wrong. Ask for a fresh template.
            return (
                tpl.height,
                "{\"ok\":false,\"err\":\"nonce space exhausted; re-fetch template\"}".to_string(),
            );
        }
    }

    let block = BlockV1 { intro, transactions };
    let resp = post_hex(
        client,
        &format!("{base}/submit/block?hexbody=true"),
        &hex::encode(block.serialize()),
    );
    (tpl.height, resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch path under the system temp dir, unique per test and per run.
    fn tmp_path(tag: &str) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!("hbit-pool-test-{}-{tag}", std::process::id()));
        p.to_string_lossy().to_string()
    }

    /// The predicate the settlement guard used before this fix: a hash was kept
    /// only while the node reported it in the mempool. Every test below shows a
    /// case where it says "resolved" and the payout is in fact still undoable.
    fn old_guard_kept_the_hash(j: &Value) -> bool {
        find_u64(j, "ret") == Some(0)
            && j.get("pending").and_then(|v| v.as_bool()).unwrap_or(false)
    }

    #[test]
    fn payout_tx_confirmed_at_depth_one_is_not_final() {
        // The old guard cleared the ledger the moment a payout left the mempool,
        // so a reorg of the payout block reopened the double-pay window.
        let shallow = serde_json::json!({"ret":0,"hash":"aa","confirm":1});
        assert!(!old_guard_kept_the_hash(&shallow));
        assert_eq!(classify_payout_tx(&shallow), PayoutTxState::Confirming(1));
        let deep = serde_json::json!({"ret":0,"hash":"aa","confirm":PAYOUT_MATURITY_DEPTH});
        assert_eq!(
            classify_payout_tx(&deep),
            PayoutTxState::Buried(PAYOUT_MATURITY_DEPTH)
        );
    }

    #[test]
    fn payout_tx_state_fails_safe_on_an_inconclusive_answer() {
        // get_json turns any transport failure into this object. Reading it as
        // "the payout is gone" is what let a timed-out query clear the guard.
        let http = serde_json::json!({"http_error":"operation timed out"});
        assert!(!old_guard_kept_the_hash(&http));
        assert_eq!(classify_payout_tx(&http), PayoutTxState::Unknown);
        let garbage = Value::String("<html>502</html>".to_string());
        assert_eq!(classify_payout_tx(&garbage), PayoutTxState::Unknown);
        // ret=0 with neither `pending` nor `confirm` is a shape we do not know.
        let odd = serde_json::json!({"ret":0,"hash":"aa"});
        assert_eq!(classify_payout_tx(&odd), PayoutTxState::Unknown);
        // Definitive answers stay definitive.
        assert_eq!(
            classify_payout_tx(&serde_json::json!({"ret":0,"pending":true})),
            PayoutTxState::Pending
        );
        assert_eq!(
            classify_payout_tx(&serde_json::json!({"ret":1,"err":"transaction not found"})),
            PayoutTxState::Gone
        );
    }

    #[test]
    fn an_implausible_balance_is_refused_instead_of_saturating() {
        // "1:280" used to saturate to u64::MAX, which distributable_units then
        // handed to split_payout as a payout plan for the whole u64 range.
        assert_eq!(balance_units("1:280"), None);
        assert_eq!(balance_units("99999999999999999999:248"), None);
        assert_eq!(balance_units("not-a-balance"), None);
        assert_eq!(balance_units("x:247"), None);
        // Real answers still value identically to before.
        assert_eq!(balance_units("49:246"), Some(4)); // 4.9 HAC floors to 4
        assert_eq!(balance_units("1:248"), Some(10)); // 1 HAC = 10 units
        assert_eq!(balance_units("1:247"), Some(1));
        assert_eq!(balance_units("5:240"), Some(0)); // finer than 0.1 HAC
        // The node omits the field for an address holding nothing: not an error.
        assert_eq!(balance_units(""), Some(0));
        // Anything past the plausibility ceiling is a corrupt answer, not money.
        assert_eq!(balance_units(&format!("{MAX_PLAUSIBLE_UNITS}:247")), Some(MAX_PLAUSIBLE_UNITS));
        assert_eq!(balance_units(&format!("{}:247", MAX_PLAUSIBLE_UNITS + 1)), None);
    }

    #[test]
    fn distributable_holds_back_immature_income_and_never_wraps() {
        // 100 units in the wallet, 60 of them from a block that is not yet
        // buried: only the matured 40 minus the reserve may be paid.
        assert_eq!(distributable_units(100, 60, 5), Some(35));
        // Nothing matured beyond the reserve -> do not settle at all.
        assert_eq!(distributable_units(100, 96, 5), None);
        assert_eq!(distributable_units(100, 100, 0), None);
        // A nonsense reserve must fail the guard, not wrap it open.
        assert_eq!(distributable_units(100, 0, u64::MAX), None);
        assert_eq!(distributable_units(100, 0, 5), Some(95));
    }

    #[test]
    fn block_reward_units_are_tenths_of_a_hac() {
        // Height 1 pays 1 HAC = 10 units of 0.1 HAC; the schedule steps at 100k.
        assert_eq!(block_reward_units(1), 10);
        assert_eq!(
            block_reward_units(2_500_000),
            mint::genesis::block_reward_number(2_500_000) as u64 * 10
        );
    }

    #[test]
    fn pending_ledger_is_shared_and_preserves_the_rest_of_the_state() {
        let path = tmp_path("ledger.state.json");
        let _ = std::fs::remove_file(&path);
        // The server owns this file and keeps accounting in it.
        atomic_write(
            &path,
            serde_json::json!({
                "order": ["a", "b"],
                "accepted": 7,
                "immature": [{"height": 9, "hash": "ab", "units": 30}],
                "settle_pending_txs": ["deadbeef"],
            })
            .to_string()
            .as_bytes(),
            true,
        )
        .expect("write state");
        assert_eq!(load_pending_payout_txs(&path), vec!["deadbeef".to_string()]);
        assert_eq!(load_immature_units(&path), 30);
        // The payout tool writes the SAME ledger, without losing the accounting.
        save_pending_payout_txs(&path, &["cafe".to_string()]).expect("save ledger");
        assert_eq!(load_pending_payout_txs(&path), vec!["cafe".to_string()]);
        let j: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        assert_eq!(j["accepted"].as_u64(), Some(7));
        assert_eq!(j["order"].as_array().map(|a| a.len()), Some(2));
        assert_eq!(load_immature_units(&path), 30);
        // The share window is readable from the same file, so the payout tool can
        // still settle correctly with the pool server stopped.
        assert_eq!(
            load_pplns_counts(&path),
            vec![("a".to_string(), 1u64), ("b".to_string(), 1u64)]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn settle_lock_is_exclusive_across_holders() {
        let wallet = tmp_path("lock-wallet.key");
        let lock = settle_lock_path(&wallet);
        let _ = std::fs::remove_file(&lock);
        let held = acquire_settle_lock(&wallet).expect("first holder takes the lock");
        assert!(
            acquire_settle_lock(&wallet).is_err(),
            "a second settler must be refused while the first one holds the wallet"
        );
        drop(held);
        let again = acquire_settle_lock(&wallet).expect("lock is free once released");
        drop(again);
        let _ = std::fs::remove_file(&lock);
    }

    #[test]
    fn a_written_key_file_is_locked_down_and_still_readable_by_us() {
        // Securing the key is now fatal-on-failure, so this must actually work on
        // the host: a broken principal lookup or an empty DACL would stop the
        // pool starting at all (and on Windows an empty DACL denies even the
        // owner a read, which is exactly the self-brick this guards against).
        let path = tmp_path("acl-wallet.key");
        let _ = std::fs::remove_file(&path);
        write_key_file(&path, "deadbeef").expect("write and secure the key file");
        assert_eq!(
            std::fs::read_to_string(&path).expect("owner can still read").trim(),
            "deadbeef"
        );
        // Re-applying on the load path must be idempotent, not a failure.
        restrict_key_file_permissions(&path).expect("re-applying the permissions is idempotent");
        assert!(std::fs::File::open(&path).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wallet_envelope_round_trips_and_rejects_a_wrong_passphrase() {
        let key_hex = "11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff";
        let body = encrypt_key_hex(key_hex, "correct horse battery").expect("encrypt");
        assert!(!body.contains(key_hex), "the key must not survive in the clear");
        let back = decrypt_key_hex(&body, "correct horse battery").expect("decrypt");
        assert_eq!(back.trim(), key_hex);
        assert!(decrypt_key_hex(&body, "wrong passphrase").is_err());
    }
}
