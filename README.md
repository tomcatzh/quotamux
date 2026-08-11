# QuotaMux

QuotaMux is a local, layered gateway for equivalent LLM providers. It exposes
user-defined model names, translates between OpenAI Chat Completions, OpenAI
Responses, and Anthropic Messages, distributes requests across equivalent
credentials, and falls back to later capacity or price layers before a response
is committed.

Key capabilities:

- provider definitions with exact upstream model IDs and multiple API keys;
- public model names and explicit compatibility aliases;
- ordered free/plan/PAYG route layers;
- random or memory-only prompt-prefix-affinity selection inside a layer;
- per-target circuit breaking, streaming-safe fallback, usage, cache, and cost
  metadata;
- protocol translation between all three public API shapes;
- no prompt, response body, API key, or affinity fingerprint persistence.

> [!IMPORTANT]
> QuotaMux does not currently authenticate incoming clients. Keep it on
> `127.0.0.1`, a private network, or behind an authenticated reverse proxy. The
> included Compose file publishes only to `127.0.0.1`.

## Quick start

### Docker Compose

```sh
cp quotamux.example.toml quotamux.toml
# Edit quotamux.toml and fill in every API key referenced by a route.
cargo run --bin quotamux -- --check
docker compose up --build -d
curl http://127.0.0.1:8080/healthz
```

The dashboard and API are then available at <http://127.0.0.1:8080>. Compose
mounts `quotamux.toml` read-only and stores request/circuit metadata in the
`quotamux-data` volume.

Useful container commands:

```sh
docker compose logs -f quotamux
docker compose restart quotamux
docker compose down
```

### Native Rust process

Rust 1.97 or newer is recommended because that is the version used by the
container build.

```sh
cp quotamux.example.toml quotamux.toml
# For a native-only process, prefer listen = "127.0.0.1:8080".
cargo run --bin quotamux -- --check
cargo run --release --bin quotamux
```

The default configuration path is `./quotamux.toml`. It can be changed with
either form:

```sh
cargo run --release --bin quotamux -- --config /path/to/quotamux.toml
QUOTAMUX_CONFIG=/path/to/quotamux.toml cargo run --release --bin quotamux
```

`QUOTAMUX_DATA_DIR` overrides `server.data_dir`. `RUST_LOG` controls logging,
for example `RUST_LOG=quotamux=debug`.

## Configuration model

QuotaMux configuration has three independent levels:

```text
providers and credentials     exact upstream services, keys, models, protocols
            ↓
ordered route layers          selection within a layer, fallback across layers
            ↓
served models                 public names and client-facing protocols
```

The distinction matters:

- A provider model name is the exact identifier accepted by that upstream.
- A served model name belongs to QuotaMux and is what clients send.
- A target is one `(provider, credential, provider model)` tuple.
- Targets in one layer are declared equivalent and may be selected in any
  order.
- Layers are tried in configuration order. Put PAYG in a later layer if it must
  only run after plan capacity is exhausted.

TOML array entries are attached to the most recently declared parent:
`[[providers.credentials]]` and `[[providers.models]]` belong to the preceding
`[[providers]]`; `[[models.layers]]` belongs to the preceding `[[models]]`.

## Complete plan → PAYG example

This is a complete configuration with two OpenCode Go keys in a plan layer and
DeepSeek Official in a later PAYG layer. Requests with no affinity evidence are
randomly distributed between the two Go keys. Requests sharing a warm prompt
prefix prefer the Go key that processed that prefix. DeepSeek is tried only
after both Go targets are unavailable or fail before response commitment.

```toml
config_version = 2

[server]
# Use 0.0.0.0 inside the included container; use 127.0.0.1 for a native process.
listen = "0.0.0.0:8080"
data_dir = "./data"

[affinity]
checkpoint_bytes = 128
max_checkpoints_per_path = 4096
max_candidates_per_prefix = 8
max_leases = 16384
success_ttl_ms = 300000

[[providers]]
id = "go"
kind = "opencode-go"
# endpoint is omitted because this provider kind has an official default.

[[providers.credentials]]
id = "go-plan-a"
api_key = "replace-with-first-go-key"

[[providers.credentials]]
id = "go-plan-b"
api_key = "replace-with-second-go-key"

[[providers.models]]
# Always use the exact upstream model ID.
name = "deepseek-v4-flash"
protocols = ["openai-chat"]

[[providers]]
id = "deepseek"
kind = "deepseek-official"

[[providers.credentials]]
id = "deepseek-payg"
api_key = "replace-with-deepseek-key"

[[providers.models]]
name = "deepseek-v4-flash"
protocols = ["openai-chat", "openai-responses", "anthropic-messages"]

[[models]]
# Public QuotaMux name. It does not need to equal an upstream model ID.
name = "deepseek-v4-flash-0731"
aliases = ["deepseek-v4-flash"]
protocols = ["openai-chat", "openai-responses", "anthropic-messages"]

[[models.layers]]
name = "plan"
strategy = "prompt-prefix-affinity"
targets = [
  { provider = "go", credential = "go-plan-a", model = "deepseek-v4-flash" },
  { provider = "go", credential = "go-plan-b", model = "deepseek-v4-flash" },
]

[[models.layers]]
name = "payg"
strategy = "random"
targets = [
  { provider = "deepseek", credential = "deepseek-payg", model = "deepseek-v4-flash" },
]
```

## Complete Kimi K3 1M example

This configuration exposes one public `kimi-k3` model through all three client
protocols. OpenCode Go and Kimi Code Allegretto are equivalent subscription
targets in the first layer. Kimi's China Open Platform is the single PAYG
fallback in the second layer.

The exact upstream IDs are intentionally different: OpenCode Go and the Open
Platform use `kimi-k3`; Kimi Code API calls use `k3`. Do not send `k3[1m]` as an
API model ID. Kimi documents that form only for a Claude Code environment
variable; Allegretto unlocks the normal `k3` API ID up to 1M context.

```toml
config_version = 2

[server]
listen = "0.0.0.0:8080"
data_dir = "./data"

[affinity]
checkpoint_bytes = 128
max_checkpoints_per_path = 4096
max_candidates_per_prefix = 8
max_leases = 16384
success_ttl_ms = 300000

[[providers]]
id = "opencode-go"
kind = "opencode-go"

[[providers.credentials]]
id = "opencode-go-plan"
api_key = "replace-with-opencode-go-key"

[[providers.models]]
name = "kimi-k3"
protocols = ["openai-chat"]

[[providers]]
id = "kimi-code"
kind = "kimi-code"

[[providers.credentials]]
id = "kimi-code-allegretto"
api_key = "replace-with-kimi-code-key"

[[providers.models]]
name = "k3"
protocols = ["openai-chat"]

[[providers]]
id = "kimi-official"
kind = "kimi-official"

[[providers.credentials]]
id = "kimi-official-payg"
api_key = "replace-with-kimi-open-platform-key"

[[providers.models]]
name = "kimi-k3"
protocols = ["openai-chat"]

[[models]]
name = "kimi-k3"
aliases = ["kimi-k3-1m"]
protocols = ["openai-chat", "openai-responses", "anthropic-messages"]

[[models.layers]]
name = "subscriptions"
strategy = "prompt-prefix-affinity"
targets = [
  { provider = "opencode-go", credential = "opencode-go-plan", model = "kimi-k3" },
  { provider = "kimi-code", credential = "kimi-code-allegretto", model = "k3" },
]

[[models.layers]]
name = "payg"
strategy = "random"
targets = [
  { provider = "kimi-official", credential = "kimi-official-payg", model = "kimi-k3" },
]
```

Provider kind, egress protocol, and exact upstream model ID form the affinity
namespace. Consequently `opencode-go/kimi-k3` and `kimi-code/k3` have isolated
prefix trees and cache evidence. On each request QuotaMux queries every eligible
target in its own namespace and ranks candidates by the longest matched prefix.
The single-target PAYG layer skips hashing and affinity bookkeeping.

Official references:

- [OpenCode Go Kimi K3 model ID and endpoint](https://opencode.ai/docs/go#endpoints)
- [Kimi Code API endpoints and 1M Allegretto entitlement](https://www.kimi.com/code/docs/)
- [Kimi Open Platform K3, 1M context, and recharge requirement](https://platform.kimi.com/docs/guide/kimi-k3-quickstart)

### Codex worker through QuotaMux `/v1`

Codex uses the Responses wire protocol for custom providers. Add this to the
user-level Codex `config.toml`; the `/v1` suffix on `base_url` is required:

```toml
model = "kimi-k3"
model_provider = "quotamux"
model_reasoning_effort = "low"
model_context_window = 1048576
web_search = "disabled"

[model_providers.quotamux]
name = "QuotaMux"
base_url = "http://127.0.0.1:8080/v1"
env_key = "QUOTAMUX_WORKER_API_KEY"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
```

Then define the credential variable before starting Codex:

```sh
export QUOTAMUX_WORKER_API_KEY=local-worker-test
```

QuotaMux does not currently authenticate inbound requests, so this value is a
local placeholder required by Codex's custom-provider configuration; it is not
one of the upstream Kimi keys. Keep `web_search = "disabled"`: Kimi is reached
through Chat Completions and cannot execute the Responses API's hosted web
search tool. Function tools such as Codex's local shell remain enabled and are
translated normally.

Codex 0.145 can warn that `kimi-k3` has no Codex-native model-catalog entry and
fall back to generic metadata. The worker path is nevertheless accepted, and
the explicit context-window setting preserves the intended 1M limit. QuotaMux
continues to expose the standard OpenAI `GET /v1/models` shape rather than a
Codex-private catalog shape.

Validate before every restart:

```sh
cargo run --bin quotamux -- --check
```

Validation rejects empty keys, duplicate IDs, invalid endpoints, unknown
providers/credentials/models, unsupported provider protocol declarations,
empty layers, and duplicate public aliases. Error messages do not contain API
key values.

## Provider configuration reference

Each provider has:

```toml
[[providers]]
id = "stable-user-defined-id"
kind = "provider-kind"
endpoint = "optional-or-required-base-url"

[[providers.credentials]]
id = "stable-key-id"
api_key = "plaintext-secret"

[[providers.models]]
name = "exact-upstream-model-id"
protocols = ["openai-chat"]
```

`id` values are local routing identities. They are visible in metadata and
statistics, so use useful names such as `go-plan-a` or `bailian-payg-cn` but do
not put secrets in them. API keys are written in plaintext in the private TOML;
keep it out of Git and restrict its file permissions:

```sh
chmod 600 quotamux.toml
```

### Production-accepted provider kinds

The currently implemented provider adapters are intentionally narrower than
the provider-kind enum in the configuration schema:

| `kind` | Default endpoint | Current acceptance status |
| --- | --- | --- |
| `opencode-go` | `https://opencode.ai/zen/go/v1` | Real-worker accepted for `deepseek-v4-flash`; real stream and 300,095-input-token acceptance also pass for `kimi-k3`. Protocol remains model-specific. |
| `deepseek-official` | `https://api.deepseek.com` | Implemented as an optional PAYG/fallback adapter, including cost estimation and Anthropic path handling. |
| `kimi-code` | `https://api.kimi.com/coding/v1` | Real Allegretto streaming and 300,095-input-token acceptance pass with exact model `k3`; a live Codex worker completed a two-turn Responses-to-Chat reasoning/tool loop through this adapter. |
| `kimi-official` | `https://api.moonshot.cn/v1` | Real paid-account streaming and 300,095-input-token acceptance pass with exact model `kimi-k3`; layered fallback and provider-specific model rewrites are covered end to end. |

Both Kimi adapters deliberately expose only `openai-chat` upstream for now. A
served model can still expose Chat Completions, Responses, and Anthropic
Messages; QuotaMux translates the latter two to Chat.

For these kinds, `endpoint` is a base URL and QuotaMux appends the protocol
path. An endpoint override is allowed for local tests.

Provider model lists change over time. Configure the exact ID and protocol
shown by the provider rather than guessing from the marketing name:

- [DeepSeek model API](https://api-docs.deepseek.com/api/list-models/)
- [OpenCode Go model IDs and endpoints](https://opencode.ai/docs/go#endpoints)

### Reserved provider kinds — not supported yet

The following names are already reserved in the configuration enum, but their
presence is scaffolding, not a support claim:

- `aliyun-bailian`
- `ollama-cloud`
- `opencode-zen`
- `custom-chat-completions`
- `custom-responses`
- `custom-anthropic`

They currently have no provider-specific end-to-end acceptance suite or real
worker evidence. Do not use them as production routes yet. Each one must receive
its authentication/URL/stream/error compatibility tests and, for official
providers, a real credential smoke test before moving into the implemented
table. The parser may recognize these names while that work is in progress.

### Multiple credentials on one provider

Declare each key under the same provider and reference it as a separate target:

```toml
[[providers]]
id = "go"
kind = "opencode-go"

[[providers.credentials]]
id = "go-team-a"
api_key = "..."

[[providers.credentials]]
id = "go-team-b"
api_key = "..."

[[providers.models]]
name = "deepseek-v4-flash"
protocols = ["openai-chat"]
```

One provider kind may also be declared multiple times when endpoints,
accounts, regions, or cache domains need distinct provider identities.

### Multiple upstream models on one provider

```toml
[[providers.models]]
name = "deepseek-v4-flash"
protocols = ["openai-chat"]

[[providers.models]]
name = "another-upstream-model"
protocols = ["openai-chat"]
```

Only explicitly listed models can be referenced by route targets. QuotaMux does
not silently enable everything returned by an upstream `/models` endpoint.

## Served models and protocol conversion

A served model is the public contract seen by clients:

```toml
[[models]]
name = "public-model-name"
aliases = ["optional-compatible-name"]
protocols = ["openai-chat", "openai-responses", "anthropic-messages"]
```

- `name` is returned by `GET /v1/models` and accepted in requests.
- `aliases` are also accepted and returned by `/v1/models`. Intentional model
  impersonation should be explicit here.
- `protocols` controls the public endpoints through which the model is exposed.

The target provider does not need to expose the same protocol as the client.
For example, a served model may expose all three public protocols while its Go
target only accepts Chat Completions. QuotaMux translates the request, response,
stream, reasoning/thinking, tools, and usage. If a requested feature cannot be
represented without losing meaning, QuotaMux returns a client error before
calling the provider.

Supported public protocols and endpoints:

| Protocol | Endpoint |
| --- | --- |
| `openai-chat` | `POST /v1/chat/completions` |
| `openai-responses` | `POST /v1/responses` |
| `anthropic-messages` | `POST /v1/messages` |

`POST /v1/messages/count_tokens` returns a clearly labelled local estimate; it
does not bill or call an upstream provider.

## Route layers

Each served model has one or more ordered layers:

```toml
[[models.layers]]
name = "plan"
strategy = "random"
targets = [
  { provider = "go", credential = "go-a", model = "deepseek-v4-flash" },
]
```

Rules:

1. Layers are evaluated from top to bottom.
2. Every target in the current layer is tried at most once in selector order.
3. A later layer is reached only after the current layer is exhausted.
4. Client validation errors and client cancellation do not fall back.
5. A stream can fall back before the first semantic event, but never after it is
   committed to the client.
6. Circuits are maintained per target, so one bad key does not disable its
   siblings.

### `random`

- Zero targets are rejected by configuration validation.
- One target is selected directly without random-number work.
- Multiple targets start in random order; failures continue through the rest of
  the same layer before the next layer.

### `prompt-prefix-affinity`

- One target is selected directly without canonicalization, hashing, lookup, or
  affinity bookkeeping.
- Multiple targets begin with random ordering when cold.
- After a successful request, a later request sharing a long canonical prompt
  prefix prefers the warm cache domain with the longest valid match.
- An unrelated prompt or expired lease returns to normal random selection.
- The index, keyed-hash secret, leases, and TTL epochs are process-local memory
  only. Restarting QuotaMux intentionally starts cold.
- Capacity eviction can reduce cache hits but cannot change response semantics.

Affinity tuning:

| Field | Default | Meaning |
| --- | ---: | --- |
| `checkpoint_bytes` | 128 | Distance between canonical-byte prefix checkpoints. |
| `max_checkpoints_per_path` | 4096 | Hard checkpoint limit for one request path. |
| `max_candidates_per_prefix` | 8 | Maximum lease candidates retained for one prefix. |
| `max_leases` | 16384 | Global in-memory lease limit. |
| `success_ttl_ms` | 300000 | Lifetime of successful-request affinity evidence in milliseconds. |

### Mixed providers in one affinity layer

Different provider kinds and credentials may be mixed when they are truly
equivalent in priority, price, output behavior, and capacity policy:

```toml
[[models.layers]]
name = "equivalent-capacity"
strategy = "prompt-prefix-affinity"
targets = [
  { provider = "go", credential = "go-plan-a", model = "deepseek-v4-flash" },
  { provider = "deepseek", credential = "deepseek-payg", model = "deepseek-v4-flash" },
]
```

This does **not** mean “Go first, then DeepSeek.” On a cold prompt either target
may be selected. To guarantee plan-before-PAYG behavior, put DeepSeek in a
separate later layer as shown in the complete example.

### Additional PAYG tiers

Add more layers in increasing cost/capacity order:

```toml
[[models.layers]]
name = "payg-standard"
strategy = "random"
targets = [
  { provider = "bailian", credential = "bailian-standard", model = "model-id" },
]

[[models.layers]]
name = "payg-premium"
strategy = "random"
targets = [
  { provider = "premium", credential = "premium-key", model = "model-id" },
]
```

## Client examples

### OpenAI Chat Completions

```sh
curl http://127.0.0.1:8080/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -H 'X-Relay-Include-Metadata: 1' \
  -d '{
    "model": "deepseek-v4-flash-0731",
    "messages": [{"role": "user", "content": "Reply with OK."}],
    "stream": false
  }'
```

### OpenAI Responses

```sh
curl http://127.0.0.1:8080/v1/responses \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "deepseek-v4-flash-0731",
    "input": "Reply with OK.",
    "reasoning": {"effort": "high"}
  }'
```

### Anthropic Messages

```sh
curl http://127.0.0.1:8080/v1/messages \
  -H 'Content-Type: application/json' \
  -H 'anthropic-version: 2023-06-01' \
  -d '{
    "model": "deepseek-v4-flash-0731",
    "max_tokens": 128,
    "messages": [{"role": "user", "content": "Reply with OK."}]
  }'
```

Clients cannot select a provider or credential. Request body `provider` and
provider-selection headers are rejected.

## Routing metadata and operations API

Add `X-Relay-Include-Metadata: 1` to an inference request to receive routing
headers:

- `X-Relay-Request-Id`
- `X-Relay-Provider`
- `X-Relay-Credential`
- `X-Relay-Route-Layer` and `X-Relay-Route-Layer-Index`
- `X-Relay-Selection-Reason` (`single-target`, `random`, or
  `prompt-prefix-affinity`)
- `X-Relay-Matched-Prefix-Bytes` when affinity matched
- `X-Relay-Upstream-Model`
- `X-Relay-Fallback` and `X-Relay-Fallback-Reason`
- `X-Relay-Ingress-Protocol`, `X-Relay-Egress-Protocol`, and
  `X-Relay-Translated`

Operational endpoints:

| Endpoint | Purpose |
| --- | --- |
| `GET /` | Local dashboard with provider-level totals and recent request routes. |
| `GET /healthz` | Health, models, and uptime. |
| `GET /v1/models` | Public served names and aliases. |
| `GET /api/status` | Served models, targets, safe endpoints, per-target circuits, affinity limits/count, and alerts. |
| `GET /api/stats` | Aggregate requests, errors, fallbacks, provider totals, and provider/upstream-model route totals. |
| `GET /api/requests?limit=100` | Recent request metadata, including served model, provider, and exact upstream model; maximum limit 1000. |
| `GET /api/attempts?limit=100` | Recent per-target attempts; maximum limit 1000. |

The dashboard's **Providers** table is deliberately aggregated to one row per
provider and does not list models. The **Recent requests** table shows the full
route for each newly recorded request as `served model -> provider -> upstream
model`. Records written by an older QuotaMux version show `-` for model fields
that were not captured at the time. Circuit state remains available per target
through `/api/status`; the dashboard does not present a misleading singleton
provider or circuit. Provider balance polling is not currently enabled.

## Storage and privacy

- `server.data_dir/quotamux.redb` stores request/attempt metadata, target circuit
  state, and alerts.
- Prompt bodies, model response bodies, and API keys are not written to that
  database.
- Prompt-prefix-affinity metadata is never written to redb. It is pure memory
  and disappears on restart.
- `quotamux.toml`, `AGENTS.md`, `CLAUDE.md`, `data/`, and build outputs are
  ignored by this repository.

## Tests

Run the complete offline/local suite:

```sh
cargo test --all-targets
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Run the credential-free smoke suite against a live QuotaMux:

```sh
cargo run --bin quotamux-smoke -- --base-url http://127.0.0.1:8080
```

The real affinity probe is ignored by default because it consumes live quota.
It reads the local ignored config, temporarily mixes OpenCode Go and DeepSeek in
one in-memory layer, sends two low-output requests, and prints only sanitized
cache/route evidence:

```sh
cargo test --test real_worker_affinity -- --ignored --nocapture
```

Kimi has a separate ignored real suite. It uses the existing private
`quotamux.toml`, directly smokes all three configured K3 targets, and verifies
the mixed subscription layer's prompt-prefix affinity:

```sh
cargo test --test real_kimi_k3 -- --ignored --nocapture
```

The >256K confirmation is additionally gated because it deliberately sends a
large paid prompt to every target:

```sh
QUOTAMUX_CONFIRM_1M=1 \
  cargo test --test real_kimi_k3 \
  each_kimi_k3_target_accepts_more_than_256k_prompt_tokens \
  -- --ignored --nocapture
```

Captured test counts and real provider cache data are in
[docs/test-evidence.md](docs/test-evidence.md).

The verified Codex worker configuration above uses the official custom-provider
keys `base_url`, `env_key`, and `wire_api = "responses"`, with web search
disabled because that hosted Responses tool is not available on the translated
Chat route. See the
[Codex configuration reference](https://developers.openai.com/codex/config-reference).

## Design documentation

- [Configuration and layered-routing architecture](docs/routing-architecture-v2.md)
- [Prompt-prefix-affinity design](docs/prompt-prefix-affinity.md)
- [Verification evidence](docs/test-evidence.md)

## License

MIT
