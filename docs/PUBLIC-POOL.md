# Public free-IP pool (`hac-pool`)

## What it is

`hac-pool` lets **anyone** run a public mining pool on a free IP:

1. **HTTP miner RPC** (port default `3333`) — compatible with existing `poworker`  
2. **Stratum TCP** (port default `3334`) — minimal JSON-RPC for multi-worker clients  
3. **Upstream** = your fullnode `host:port` miner API  

## All-in-one (miner-panel)

1. Build: `cargo build --release -p miner-pool -p miner-panel` (needs `hac-pool` next to the panel).
2. Open **Settings**.
3. Section **PUBLIC FREE-IP POOL (ALL-IN-ONE)**:
   - Enable public pool controls
   - Upstream fullnode (default `127.0.0.1:8080`)
   - HTTP / Stratum ports
   - Optional token
   - **Start public pool**
4. With “mine through it” checked, Connect becomes `127.0.0.1:HTTP`.
5. **Start Mining** — if pool hosting is enabled and pool is stopped, the panel auto-starts the pool first.

## Run (CLI)

```bash
# fullnode must expose miner API (e.g. listen 8080)
cargo run --release -p miner-pool -- \
  --upstream 127.0.0.1:8080 \
  --http-bind 0.0.0.0:3333 \
  --stratum-bind 0.0.0.0:3334
```

Open free pool (no password):

```bash
# default: empty --pool-token
```

Token-protected:

```bash
cargo run --release -p miner-pool -- \
  --upstream 127.0.0.1:8080 \
  --pool-token "shared-secret"
```

## Workers (poworker)

```ini
; poworker.config.ini
connect = POOL_PUBLIC_IP:3333
; if pool token set:
api_token = shared-secret
```

Firewall: open TCP 3333 (and 3334 for Stratum).

## Stratum (minimal)

Line-delimited JSON-RPC:

- `mining.subscribe`
- `mining.authorize` with password = pool token (or any if open)
- `mining.notify` push: `[job_id, height, block_intro_hex, target]`
- `mining.submit`: `[worker, job_id, block_nonce, coinbase_nonce]`
- `mining.get_job`: full pending JSON (Hacash-native helper)

## Security notes

- Public bind without token is intentional for “free IP pool” but risks abuse.  
- Prefer `--pool-token` on the internet.  
- Upstream fullnode should stay on localhost; only `hac-pool` is public.  
