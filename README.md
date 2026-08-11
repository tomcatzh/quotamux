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

Supported provider kinds:

| `kind` | Default endpoint | Protocol rule |
| --- | --- | --- |
| `deepseek-official` | `https://api.deepseek.com` | Declare the protocols supported by the enabled model. |
| `kimi-official` | `https://api.moonshot.cn/v1` | `openai-chat` only. |
| `aliyun-bailian` | none; `endpoint` required | Declare protocols supported by the configured compatible endpoint. |
| `ollama-cloud` | `https://ollama.com/v1` | Commonly `openai-chat` and/or `openai-responses`. |
| `opencode-zen` | `https://opencode.ai/zen/v1` | Protocol is model-specific. |
| `opencode-go` | `https://opencode.ai/zen/go/v1` | Protocol is model-specific. |
| `custom-chat-completions` | none; full `endpoint` required | `openai-chat` only. |
| `custom-responses` | none; full `endpoint` required | `openai-responses` only. |
| `custom-anthropic` | none; full `endpoint` required | `anthropic-messages` only. |

For official/non-custom kinds, `endpoint` is a base URL and QuotaMux appends
the protocol path. An endpoint override is allowed. For custom kinds,
`endpoint` is the complete request URL and is used exactly as written.

Provider model lists change over time. Configure the exact ID and protocol
shown by the provider rather than guessing from the marketing name. Useful
official references:

- [DeepSeek model API](https://api-docs.deepseek.com/api/list-models/)
- [OpenCode Go model IDs and endpoints](https://opencode.ai/docs/go#endpoints)
- [OpenCode Zen model IDs and endpoints](https://opencode.ai/docs/zen#models)
- [Kimi platform documentation](https://platform.kimi.com/docs/overview)
- [Alibaba Cloud Model Studio models](https://help.aliyun.com/en/model-studio/models)
- [Ollama OpenAI compatibility](https://docs.ollama.com/api/openai-compatibility)

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
name = "gpt-5.6-luna"
protocols = ["openai-responses"]
```

Only explicitly listed models can be referenced by route targets. QuotaMux does
not silently enable everything returned by an upstream `/models` endpoint.

### Custom endpoint examples

Custom endpoints use the full final request URL:

```toml
[[providers]]
id = "local-chat"
kind = "custom-chat-completions"
endpoint = "http://127.0.0.1:11434/v1/chat/completions"

[[providers.credentials]]
id = "local-chat-key"
api_key = "unused-but-must-be-non-empty"

[[providers.models]]
name = "my-local-model"
protocols = ["openai-chat"]

[[providers]]
id = "company-responses"
kind = "custom-responses"
endpoint = "https://llm.example.com/v1/responses"

[[providers.credentials]]
id = "company-key"
api_key = "..."

[[providers.models]]
name = "company-model-v2"
protocols = ["openai-responses"]

[[providers]]
id = "company-anthropic"
kind = "custom-anthropic"
endpoint = "https://llm.example.com/v1/messages"

[[providers.credentials]]
id = "company-anthropic-key"
api_key = "..."

[[providers.models]]
name = "company-claude-compatible"
protocols = ["anthropic-messages"]
```

QuotaMux sends bearer authentication for Chat/Responses custom endpoints and
`x-api-key` plus `anthropic-version` for `custom-anthropic`.

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
| `GET /` | Local dashboard. |
| `GET /healthz` | Health, models, and uptime. |
| `GET /v1/models` | Public served names and aliases. |
| `GET /api/status` | Targets, safe endpoints, circuits, affinity limits/count, balance, and alerts. |
| `GET /api/stats` | Aggregate requests, errors, fallbacks, provider cache tokens, and cost. |
| `GET /api/requests?limit=100` | Recent request metadata; maximum limit 1000. |
| `GET /api/attempts?limit=100` | Recent per-target attempts; maximum limit 1000. |

## Storage and privacy

- `server.data_dir/quotamux.redb` stores request/attempt metadata, target circuit
  state, balance snapshots, and alerts.
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

Captured test counts and real provider cache data are in
[docs/test-evidence.md](docs/test-evidence.md).

## Design documentation

- [Configuration and layered-routing architecture](docs/routing-architecture-v2.md)
- [Prompt-prefix-affinity design](docs/prompt-prefix-affinity.md)
- [Verification evidence](docs/test-evidence.md)

## License

MIT
