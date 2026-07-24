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

## Connection limits

| Option | Env var | Default |
|---|---|---|
| `--max-conns-per-ip` | `HAC_POOL_MAX_CONNS_PER_IP` | `128` (`0` = unlimited) |

Caps how many **Stratum** connections one source IP may hold at once, so a single
peer cannot pin every slot and lock real miners out. Over the cap the pool logs
`stratum per-IP cap (N) reached; dropping <peer>` and closes the new socket; the
worker sees a dropped connection and reconnects.

Raise it for a **large farm behind one NAT address or one VPN exit**, where every
rig looks like the same IP: the default 128 is generous for a home farm but a
200-rig site needs more. Set it to `0` only on a pool that is not reachable from
the internet. There is a separate hard cap of 1024 Stratum connections in total.

```bash
cargo run --release -p miner-pool -- \
  --upstream 127.0.0.1:8080 \
  --max-conns-per-ip 400
```

## Job freshness and "upstream stale"

`hac-pool` mirrors work from the upstream fullnode every `--poll-ms`
(default 2000). A mirrored job is only handed out while it is **fresh**, where
fresh means younger than `max(poll_ms x 4, 15s)`. At the default poll interval
that TTL is 15 seconds; at `--poll-ms 10000` it is 40 seconds.

The refresh runs on every successful poll even when the height has not changed,
so the TTL only elapses during a real upstream outage. It never elapses just
because the chain is quiet between blocks.

Once the TTL elapses the pool stops serving that job and says so:

- HTTP miner RPC answers `{"err":"upstream stale; work is not being refreshed"}`
  to `/query/miner/pending` and to a long-poll `/query/miner/notice` that times
  out.
- Stratum simply does not push the stale job to a miner.

**What it means for a worker:** the pool is up, but its fullnode is not
answering, so the work it holds is for a height the network has probably already
passed. Mining it would burn electricity on results that can only be rejected, so
the worker is told to wait instead. It is the operator's fullnode that needs
attention, not the worker. The pool logs `upstream job is stale (no refresh for
>Ns)` once when it starts and `upstream job refresh recovered` once when it comes
back, so an outage is easy to tell apart from a quiet chain.

## Found-block submit retries

A found block is the highest-value event in mining and there is no second chance
at it, so submits upstream are not one-shot:

| Setting | Value |
|---|---|
| Submit attempts | up to 5 |
| Backoff between attempts | 250ms, 500ms, 1s, 2s (about a 3.75s total budget) |
| Per-attempt timeout, submit | 60s |
| Per-attempt timeout, job polling | 30s |

Submits get the longer 60s budget because a busy fullnode validates the whole
block before answering, and that happens at exactly the moment a solution
arrives. Using the 30s polling timeout there would discard found blocks.

Only transport failures and upstream 5xx / 408 / 429 replies are retried. An
HTTP 200 body is the node's own verdict and is returned to the worker verbatim
after a single attempt, so a genuine "stale height" rejection is never
re-hammered. Re-sending is safe in any case: the fullnode matches a submit by
height and can only ever include one block per height, so a repeat after a lost
reply is at worst a no-op and can never pay twice.

## Security notes

- Public bind without token is intentional for “free IP pool” but risks abuse.  
- Prefer `--pool-token` on the internet.  
- Upstream fullnode should stay on localhost; only `hac-pool` is public.  
- Keep `--max-conns-per-ip` non-zero on a public bind.  
