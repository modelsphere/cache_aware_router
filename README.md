# CART — Cache-Aware Router

A small reverse proxy that sits in front of a pool of LLM inference servers
(vLLM, SGLang, or anything speaking the OpenAI HTTP API) and decides **which
replica should serve each request**.

The goal is to make the backends' prefix caches actually pay off. Every modern
inference server keeps a radix/prefix cache of KV blocks, so a request whose
prompt shares a long prefix with an earlier one can skip re-computing that
prefix — but only if it lands on the **same replica** that served the earlier
request. A plain round-robin or least-connections load balancer scatters related
requests across the pool and throws most of that reuse away.

CART keeps its own radix tree of "which prefix was last sent where", and routes
a request to the replica that already holds the longest matching prefix — unless
that replica is too busy, in which case load wins over cache affinity.

```
              ┌─────────────────────────────────────┐
   client ───▶│  CART                               │
              │   radix tree: prefix ──▶ replica    │
              │   per-replica in-flight load        │
              │   health checks + circuit breaker   │
              └───┬─────────────┬─────────────┬─────┘
                  ▼             ▼             ▼
             replica-1     replica-2     replica-3
            (vLLM/SGLang)
```

## How a request is routed

For each request CART extracts the prompt text (see
[Text extraction](#text-extraction)) and then:

1. **Filter to usable replicas** — healthy, circuit breaker closed, and
   in-flight load below `max_load`. None left ⇒ `503`.
2. **Find the least-loaded replica** among those (ties broken randomly).
3. **Prefix-match the prompt** against the radix tree.
4. **Route to the cached replica if the match is good enough** — either the
   matched fraction reaches `cache.threshold`, or the matched length reaches
   `cache.match_abs_threshold` (an absolute character count, which catches long
   shared system prompts that are only a small fraction of a long request).
5. **…unless that replica is overloaded.** If the cached replica's load exceeds
   the least-loaded one by *both* `balance_abs_threshold` (absolute) *and*
   `balance_rel_threshold` (relative), cache affinity is dropped and the request
   goes to the least-loaded replica. Requiring both conditions avoids reacting to
   noise in a lightly-loaded pool.
6. **Otherwise fall back to least-loaded**, and record the new prefix in the tree
   so the next similar request follows it.

Every decision is logged with the reason (`cache_hit`, `hit_overloaded`,
`empty_text`, …), the matched ratio and the decision latency, which makes it easy
to tell "cache affinity isn't working" from "the pool is just imbalanced".

### Load accounting

Load is the number of **in-flight requests** CART has forwarded to a replica,
tracked with an RAII guard so it is decremented even if the client disconnects
mid-stream. Two knobs shape it per replica:

- `max_load` — hard admission cap; at the cap the replica is skipped entirely.
- `load_penalty` — a constant added to the replica's load *for comparison
  purposes only*. Use it to bias traffic away from a weaker or shared node
  without taking it out of the pool.

### Text extraction

The prefix key is built from the request body, per endpoint:

| Endpoint | Key |
|---|---|
| `POST /v1/chat/completions` | the `messages` array, serialized in order |
| `POST /v1/completions` | the `prompt` field |
| `POST /v1/messages` (Anthropic-style) | `system` + the `messages` array |

Multimodal parts are handled too: remote media URLs can be rejected up front via
`proxy.remote_media_url_policy` (set it to a 4xx/5xx status to refuse requests
that would make the backend fetch from the internet).

## Quick start

### Build

```bash
cargo build --release
./target/release/cache-aware-router --version
```

Requires Rust 1.88 or newer.

### Configure

Start from `config.example.yaml`; only `workers` is required.

```yaml
server:
  host: "0.0.0.0"
  port: 6700

workers:
  - url: "http://node1:8050"
    max_load: 20
  - url: "http://node2:8050"
    max_load: 20

cache:
  threshold: 0.3              # route by cache when ≥30% of the prompt matches
  match_abs_threshold: 8192   # …or when ≥8192 chars match, whatever the ratio
```

### Run

```bash
cache-aware-router -c config.yaml
```

Validate a config without starting the server:

```bash
cache-aware-router -c config.yaml --config-check
```

`--config` is **repeatable** and the files are layered in the order given, last
one wins:

```bash
cache-aware-router -c base.yaml -c tuning.yaml -c workers.yaml
```

- mappings merge key by key, so an overlay only needs the keys it changes;
- lists and scalars are replaced wholesale — an overlay naming `workers`
  replaces the whole list rather than appending to it;
- an empty or comment-only file changes nothing.

This is what lets a generated file (the worker list, say) be kept separate from
hand-written tuning, without either side having to rewrite the other.

### Docker

```bash
docker build -t cache-aware-router:dev .
docker run --rm -p 6700:6700 \
  -v "$PWD/config.yaml:/workspace/configs/config.yaml" \
  cache-aware-router:dev
```

Both base images are build args, so a build that cannot reach Docker Hub can
point them at its own registry:

```bash
docker build \
  --build-arg BUILDER_IMAGE=my-registry/rust:1.88-bookworm \
  --build-arg RUNTIME_IMAGE=my-registry/debian:12-slim \
  --build-arg CARGO_REGISTRY="sparse+https://rsproxy.cn/index/" .
```

The runtime stage installs no packages, so a bare runtime image with no package
feed works. TLS goes through rustls, so the binary links nothing beyond libc and
friends; the only thing copied out of the builder is the system trust store,
which `rustls-native-certs` reads at startup.

The entrypoint requires `ulimit -n` ≥ 65535 and refuses to start below that — a
router holding thousands of concurrent streams runs out of file descriptors long
before it runs out of CPU.

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/chat/completions` | proxied, cache-aware routing |
| POST | `/v1/completions` | proxied, cache-aware routing |
| POST | `/v1/messages` | proxied, cache-aware routing |
| GET | `/v1/models` | proxied, then **cached for 10 minutes** |
| GET | `/health` | healthy replica count; `503` when none are healthy |
| GET | `/workers` | per-replica load, health, circuit-breaker state |
| * | anything else | passed through to a replica unchanged |

Streaming (`text/event-stream`) is passed through without buffering, so
time-to-first-token is unaffected. Set `proxy.add_routed_peer_header: true` to
have CART stamp the chosen replica into an `x-routed-peer` response header —
useful when you are debugging why a request went where it did.

> ⚠️ `/v1/models` answers from a 10-minute in-process cache, so it keeps
> returning `200` for up to 10 minutes after every backend has gone away. Do not
> use it as a liveness probe for CART or as an upstream health check — use
> `/health`, which reflects the live replica count, or `/workers`.

## Reliability

- **Health checks** poll `health.endpoint` on every replica every
  `health.interval_secs`; `failure_threshold` consecutive failures mark it
  unhealthy, `success_threshold` successes bring it back.
- **Circuit breaker** per replica: after `failure_threshold` failures it opens,
  stops sending traffic for `timeout_secs`, then half-opens and needs
  `success_threshold` successes to close again.
- **Retries** on retryable statuses and connection errors, with exponential
  backoff plus jitter (`proxy.max_retries`, `initial_backoff_ms`,
  `backoff_multiplier`, `jitter_factor`). A retry excludes the replica that just
  failed.
- **`connect_timeout_secs`** (default 2s) bounds only the TCP/TLS handshake, so a
  dead or blackholed backend fails fast instead of hanging for ~30s. Long
  streaming responses are unaffected — they are bounded by
  `request_timeout_secs`.

## Configuration reload

Send `SIGHUP` and CART re-reads its config files. The reload is deliberately
narrow: **only the `workers` list may change.** If anything else differs
(`server`, `cache`, `health`, `proxy`, …) the reload is rejected and the running
config is kept, because those settings are baked into live objects — the
listening socket, the radix tree, the health-check task — and swapping them under
traffic would be a restart in disguise.

A reload rebuilds the worker set and starts a fresh radix tree, so the prefix
cache is cold immediately afterwards.

Note that CART **refuses to start with an empty `workers` list**. When the list
is generated by an external controller, make sure the file is populated before
the process starts, or it will exit and (under a supervisor) crash-loop until it
is.

## Tuning the tree

The radix tree stores prompt prefixes, so it grows with traffic. Three settings
bound it:

- `max_tree_size` — node ceiling; a background pass evicts least-recently-used
  entries above it every `eviction_interval_secs`.
- `eviction_interval_secs: 0` disables eviction entirely (don't, outside tests).
- `daily_cleanup_hour_utc` — hour of day to drop the whole tree and start clean.
  Set it to your traffic trough if you would rather reset than carry stale
  entries; `-1` disables it.

Eviction takes an exclusive lock, so inserts skip (and log) rather than block if
a pass runs long.

## Kubernetes

CART is packaged as a Helm chart, published from the
[helm-charts](https://github.com/modelsphere/helm-charts) repository
rather than from here:

```bash
helm repo add modelsphere https://modelsphere.github.io/helm-charts
helm install my-cart modelsphere/cart
```

The chart's defaults assume a controller is managing the worker list and
reloading CART for you:

- `waitForWorkers: true` holds the pod in `Init` until the config has workers;
- `reload.enabled: true` adds a sidecar that sends `SIGHUP` on config change;
- `ha.enabled: true` adds a leader-election sidecar so only one replica takes
  traffic -- CART's cache is local, and an active-active pair splits it in half.

For a standalone install with a hand-written worker list, set all three to
`false` and put `workers` directly in `baseConfig`.

## Limitations

- **No metrics endpoint yet.** The `metrics` module is a stub: the call sites are
  wired throughout the code, but the bodies are no-ops. Observability today comes
  from the structured decision logs and `GET /workers`.
- **The tree tracks characters, not tokens.** Thresholds are in characters, which
  is a good proxy but not exact; backends whose cache granularity is large (e.g.
  paged attention with a large page size, or decode-context parallelism) may not
  register a hit even when CART routed correctly.
- **Routing state is per-process.** Scaling CART horizontally splits the cache;
  run one active instance per backend pool (see `ha.enabled` above).

## Development

```bash
cargo test           # 107 tests, no external services needed
cargo fmt --check
cargo clippy
```

The release profile intentionally keeps `debug-assertions` and `overflow-checks`
on: this is a proxy on the critical path of every request, and a silent wrap-around
in load accounting would be much more expensive than the checks.

## License

Apache License 2.0 — see [LICENSE](LICENSE).
