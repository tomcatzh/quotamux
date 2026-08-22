# QuotaMux

QuotaMux is a local LLM gateway for quota-aware routing. It exposes stable
model names, distributes requests across equivalent upstream credentials, and
falls back through ordered capacity layers when an upstream cannot serve a
request.

It accepts OpenAI Chat Completions, OpenAI Responses, and Anthropic Messages.
When the selected upstream uses another supported protocol, QuotaMux translates
the request and response at the protocol boundary.

> [!IMPORTANT]
> QuotaMux does not authenticate inbound clients. Bind it to localhost, keep it
> on a private network, or place it behind an authenticated reverse proxy. The
> included Compose configuration publishes only on `127.0.0.1`.

## Principles

- **Configuration owns routing.** A client selects a served model and protocol;
  it cannot select a backend, adapter, credential, or route layer.
- **Layers express priority.** Every eligible target in one layer is exhausted
  before QuotaMux enters the next layer.
- **Targets inside one layer are equivalent.** They may be tried in any order.
  Different prices or service priorities belong in different layers.
- **Native protocol first.** QuotaMux forwards natively whenever the selected
  target supports the ingress protocol. Translation is used only when needed.
- **Adapters own provider behavior.** Official endpoints, authentication,
  protocol rules, URL construction, and provider error classification live in
  adapter code, not in user configuration.
- **Custom endpoints stay generic.** A customer URL never inherits the special
  behavior of an official provider adapter.
- **Fail closed at startup.** Unknown adapters, invalid endpoints, unsupported
  model/protocol combinations, and broken route references reject the complete
  configuration.
- **Fallback stops at response commitment.** QuotaMux may switch targets before
  the first semantic response event, never after it.
- **Routing metadata is content-free.** Prompts, responses, API keys, and
  affinity fingerprints are not persisted.

## Configuration model

QuotaMux separates implementation, deployment, and routing:

| Term | Meaning |
| --- | --- |
| provider | The external service or company. Used only as a descriptive term. |
| adapter | QuotaMux code that implements one upstream API contract. |
| backend | One configured upstream instance using an adapter. |
| credential | One independently routed API key on a backend. |
| upstream model | The exact model ID sent to that backend. |
| target | One `(backend, credential, upstream model)` tuple. |
| served model | The public model name accepted from clients. |
| layer | An ordered priority group containing equivalent targets. |

The relationship is:

```text
served model
  └─ ordered layers
       └─ targets
            └─ backend + credential + upstream model
                 └─ adapter
```

Configuration uses TOML and requires `config_version = 3`. Array entries belong
to their closest preceding parent: `[[backends.credentials]]` and
`[[backends.models]]` belong to the preceding `[[backends]]`, while
`[[models.layers]]` belongs to the preceding `[[models]]`.

## Quick start

```sh
cp quotamux.example.toml quotamux.toml
# Add the API keys required by your routes.
cargo run --bin quotamux -- --check
docker compose up --build -d
curl http://127.0.0.1:8080/healthz
```

The API and dashboard are available at <http://127.0.0.1:8080>. Compose mounts
`quotamux.toml` read-only and stores operational data in a named volume.

Useful commands:

```sh
docker compose logs -f quotamux
docker compose restart quotamux
docker compose down
```

For a native process, use a localhost listener:

```sh
cargo run --release --bin quotamux -- --config ./quotamux.toml
```

The default configuration path is `./quotamux.toml`. `QUOTAMUX_CONFIG` sets
another path, `QUOTAMUX_DATA_DIR` overrides `server.data_dir`, and `RUST_LOG`
controls logging.

## Complete example

This configuration tries an OpenCode Go subscription first and DeepSeek
Official pay-as-you-go capacity second. The public model supports all three
ingress protocols even though each target may use a different native protocol.

```toml
config_version = 3

[server]
listen = "0.0.0.0:8080"
data_dir = "./data"

[server.timeouts]
upstream_connect_ms = 10000
upstream_read_ms = 90000
upstream_stream_read_ms = 300000
upstream_total_ms = 7200000
downstream_sse_heartbeat_ms = 15000

[[backends]]
id = "go"
adapter = "opencode-go"

[[backends.credentials]]
id = "go-plan"
api_key = "replace-with-opencode-go-key"

[[backends.models]]
name = "deepseek-v4-flash"

[[backends]]
id = "deepseek"
adapter = "deepseek-official"

[[backends.credentials]]
id = "deepseek-payg"
api_key = "replace-with-deepseek-key"

[[backends.models]]
name = "deepseek-v4-flash"
protocols = ["openai-chat", "openai-responses", "anthropic-messages"]

[[models]]
name = "deepseek-v4-flash"
aliases = ["deepseek-v4-flash-0731"]
protocols = ["openai-chat", "openai-responses", "anthropic-messages"]

[[models.layers]]
name = "subscription"
strategy = "random"
targets = [
  { backend = "go", credential = "go-plan", model = "deepseek-v4-flash" },
]

[[models.layers]]
name = "payg"
strategy = "random"
targets = [
  { backend = "deepseek", credential = "deepseek-payg", model = "deepseek-v4-flash" },
]
```

See [`quotamux.example.toml`](quotamux.example.toml) for a ready-to-copy
configuration.

### DeepSeek V4 Flash Vision Exp

Use the exact public and upstream model ID
`deepseek-v4-flash-vision-exp`. The example configuration routes OpenCode Go
first and DeepSeek Official second, just like V4 Flash. Image inputs work over
Chat Completions, Responses, and Anthropic Messages. When the Go target is
selected, QuotaMux converts Anthropic and Responses user-image blocks to the
OpenAI-compatible Chat format without removing the image.

DeepSeek supports JPEG, PNG, GIF, and WebP inputs through base64 data URLs,
public URLs, or Files API IDs. Images are converted to input tokens and are
therefore included in QuotaMux token and estimated-cost statistics. See the
[DeepSeek vision guide](https://api-docs.deepseek.com/guides/vision/) for the
current size, count, role, and token limits.

OpenCode Go publishes these request-count estimates for its dollar-valued
subscription windows; they are estimates based on typical request shapes, not
fixed per-model request limits:

| Model | 5 hours | Week | Month |
| --- | ---: | ---: | ---: |
| DeepSeek V4 Pro | 1,050 | 2,600 | 5,200 |
| DeepSeek V4 Flash | 7,600 | 18,900 | 37,800 |
| DeepSeek V4 Flash Vision Exp | 3,800 | 9,450 | 18,900 |

The exact model endpoint, prices, and estimates are maintained in the
[OpenCode Go documentation](https://opencode.ai/docs/go/).

## Backends and adapters

A backend declares a local identity, one adapter, one or more credentials, and
the exact upstream models allowed on that instance:

```toml
[[backends]]
id = "backend-id"
adapter = "adapter-id"

[[backends.credentials]]
id = "credential-id"
api_key = "secret"

[[backends.models]]
name = "exact-upstream-model-id"
protocols = ["openai-chat"]
```

Backend and credential IDs are visible in routing metadata. They must be stable
and must not contain secrets.

### Official adapters

Official adapters own their endpoint. Configuring `endpoint` on them is an
error.

| `adapter` | Adapter-owned endpoint | Native egress protocols |
| --- | --- | --- |
| `opencode-go` | `https://opencode.ai/zen/go/v1` | Model-specific: Chat, Responses, or Anthropic |
| `deepseek-official` | `https://api.deepseek.com` | Chat, Responses, Anthropic |
| `kimi-code` | `https://api.kimi.com/coding/v1` | Chat, Anthropic |
| `kimi-official` | `https://api.moonshot.cn/v1` | Chat |

For `opencode-go`, the adapter contains the official model-to-protocol catalog.
Declare only the exact model ID and omit `protocols`:

```toml
[[backends.models]]
name = "kimi-k3"
```

QuotaMux rejects a Go model that is not in the adapter catalog. The catalog
follows the [OpenCode Go endpoint table](https://dev.opencode.ai/docs/go/#endpoints).

For the other official adapters, declare the native protocols enabled for each
model. The adapter verifies that every declaration is supported by that API.
Kimi Code and the Kimi Open Platform are separate services with separate keys;
their adapters and backends must remain separate. See the
[Kimi Code API overview](https://www.kimi.com/code/docs/) and
[Kimi Platform API overview](https://platform.kimi.com/docs/api/overview).

### Custom adapters

Use a custom adapter for a proxy, regional deployment, mock server, or any
customer-controlled endpoint:

| `adapter` | Protocol | Authentication |
| --- | --- | --- |
| `custom-chat-completions` | OpenAI Chat Completions | Bearer token |
| `custom-responses` | OpenAI Responses | Bearer token |
| `custom-anthropic` | Anthropic Messages | `x-api-key` |

Custom adapters require the complete request URL:

```toml
[[backends]]
id = "customer-chat"
adapter = "custom-chat-completions"
endpoint = "https://gateway.example/v1/chat/completions"

[[backends.credentials]]
id = "customer-key"
api_key = "secret"

[[backends.models]]
name = "model-id"
protocols = ["openai-chat"]
```

QuotaMux uses generic protocol, authentication, and HTTP-status behavior for a
custom backend. Its hostname, response body, and model name cannot activate an
official adapter's special handling.

Only the adapters listed above are accepted by startup validation.

### Pricing

Pricing is optional and belongs to one backend/model pair:

```toml
pricing = { cache_hit_input_usd_per_million = 0.0, cache_miss_input_usd_per_million = 0.0, output_usd_per_million = 0.0 }
```

Values are USD per one million tokens and must be finite and non-negative.
QuotaMux combines them with upstream-reported usage. If pricing is absent,
usage is recorded without an estimated cost. No model price is built into the
binary; configure the provider's current prices yourself.

DeepSeek V4 pricing changes by time of day. The example configuration uses the
current off-peak USD rates; peak rates are twice those values, so QuotaMux's
static cost field remains an estimate rather than a billing statement.

## Served models

A served model is the public contract exposed to clients:

```toml
[[models]]
name = "public-model-name"
aliases = ["compatible-name"]
protocols = ["openai-chat", "openai-responses", "anthropic-messages"]
```

- `name` and `aliases` are accepted in inference requests and returned by
  `GET /v1/models`.
- `protocols` controls which public inference endpoints accept the model.
- Every target references a declared backend, credential, and upstream model.
- QuotaMux rewrites the public model name to the selected upstream model ID.

The request's `model` selects this server-configured route graph. No request
field or header selects a backend, credential, adapter, or layer. Extra fields
may be preserved during native forwarding, but they never control QuotaMux
routing.

## Route layers

Each served model has one or more ordered layers:

```toml
[[models.layers]]
name = "subscription"
strategy = "prompt-prefix-affinity"
targets = [
  { backend = "go", credential = "team-a", model = "kimi-k3" },
  { backend = "go", credential = "team-b", model = "kimi-k3" },
]
```

Routing follows these rules:

1. Layers are evaluated in configuration order.
2. Each eligible target in the active layer is tried at most once.
3. A later layer is reached only after the active layer is exhausted.
4. Request errors and client cancellation do not trigger fallback.
5. Provider, transport, capacity, quota, authentication, billing, and
   configuration failures may trigger fallback before response commitment.
6. A streaming response may fall back before its first semantic event. It
   never switches target after that event.
7. Circuit state belongs to an exact target, so one credential does not disable
   another.

### `random`

Targets are tried in random order. A one-target layer bypasses randomization.

### `prompt-prefix-affinity`

QuotaMux prefers the eligible target with the longest known warm canonical
prompt prefix. With no valid match it uses random order. Successful requests
create short-lived cache evidence; expired or evicted evidence changes cache
efficiency only, never request semantics.

Affinity state is bounded, keyed, process-local memory. It contains no prompt
text and is discarded on restart. A one-target layer bypasses canonicalization,
hashing, lookup, and affinity bookkeeping.

Optional tuning:

```toml
[affinity]
checkpoint_bytes = 128
max_checkpoints_per_path = 4096
max_candidates_per_prefix = 8
max_leases = 16384
success_ttl_ms = 300000
```

These values are the defaults. See
[`docs/prompt-prefix-affinity.md`](docs/prompt-prefix-affinity.md) for the
canonicalization and evidence model.

## Failure and circuit behavior

Adapters classify failures by provider meaning, not by status code alone:

- OpenCode Go distinguishes documented subscription exhaustion from ordinary
  rate pressure.
- Kimi Code distinguishes membership quota, capacity, entitlement,
  authentication, and request errors using its
  [error reference](https://www.kimi.com/code/docs/en/kimi-code/error-reference.html).
- Kimi Official uses the structured Kimi Platform `error.type` values described
  in the [platform error reference](https://platform.kimi.com/docs/api/errors).
- Custom adapters use generic HTTP classification only.

Request-specific `4xx` failures such as `400` and `422` are returned without
fallback or circuit impact. Provider-side failures open or suspend the exact
target and allow the router to continue.

Concurrent attempts share a circuit generation. The first accepted failure
advances that generation; late results from the previous generation cannot
raise the backoff level again.

| Failure class | Retry schedule | Maximum `Retry-After` |
| --- | --- | --- |
| transient, stream, transport | 1, 2, 4, 8, 16, 32, 60 seconds | 60 seconds |
| capacity or rate pressure | 5, 30, 300 seconds | 300 seconds |
| subscription quota | 60, 300, 900 seconds | 900 seconds |
| authentication, configuration, unknown provider `4xx` | 5, 30, 60 minutes | ignored |
| billing or balance | 60 minutes | ignored |

`Retry-After` accepts HTTP seconds or an HTTP date. It is advisory, allowed only
for retryable classes, and capped before the effective next-probe deadline is
persisted.

When a deadline expires, one request receives a half-open probe lease. Success
closes the circuit; a provider failure advances the schedule once. Cancellation
or an abandoned stream releases the probe and makes it eligible again after one
second without increasing backoff. A probe lease has a ten-minute hard limit,
and process startup releases any half-open state immediately.

Provider error bodies are inspected only for classification, with a 16 KiB
limit. QuotaMux records and returns a controlled summary rather than copying
upstream text that may contain credentials or account data.

See [`docs/provider-failure-policy.md`](docs/provider-failure-policy.md) for the
complete classification and recovery contract.

## Protocol gateway

| Protocol | Endpoint |
| --- | --- |
| Model catalog | `GET /v1/models` |
| OpenAI Chat Completions | `POST /v1/chat/completions` |
| OpenAI Responses | `POST /v1/responses` |
| Anthropic Messages | `POST /v1/messages` |
| Anthropic token estimate | `POST /v1/messages/count_tokens` |

`count_tokens` is a local estimate. It does not call or bill an upstream.

When ingress and egress protocols match, QuotaMux preserves the native request
apart from the upstream model rewrite and Chat streaming usage option. The
upstream remains responsible for semantic validation.

Cross-protocol translation covers the shared semantics of text, system and
developer instructions, ordinary function tools and results, reasoning where
representable, output format, termination, streaming events, usage, and cache
usage. Fields with no valid destination representation are omitted. QuotaMux
does not serialize unsupported features into prompts or fabricate Anthropic
thinking signatures.

Example:

```sh
curl http://127.0.0.1:8080/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -H 'X-Relay-Include-Metadata: 1' \
  -d '{
    "model": "deepseek-v4-flash",
    "messages": [{"role": "user", "content": "Reply with OK."}],
    "stream": false
  }'
```

## Routing metadata

Routing response headers are opt-in. Add `X-Relay-Include-Metadata: 1` to an
inference request to receive:

- `X-Relay-Request-Id`
- `X-Relay-Backend`
- `X-Relay-Credential`
- `X-Relay-Route-Layer`
- `X-Relay-Route-Layer-Index`
- `X-Relay-Selection-Reason`
- `X-Relay-Matched-Prefix-Bytes` when affinity matched
- `X-Relay-Upstream-Model`
- `X-Relay-Fallback`
- `X-Relay-Fallback-Reason`
- `X-Relay-Ingress-Protocol`
- `X-Relay-Egress-Protocol`
- `X-Relay-Translated`

These are response metadata. They do not provide a client-side routing
interface.

## Operations API

| Endpoint | Purpose |
| --- | --- |
| `GET /` | Local dashboard. |
| `GET /healthz` | Health, served models, and uptime. |
| `GET /api/status` | Configuration summary, target circuits, affinity state, and alerts. |
| `GET /api/stats` | Aggregate request, backend, model, token, and cost totals. |
| `GET /api/routing` | Served models, ordered layers, targets, and circuit state. |
| `GET /api/routing/stats?model=NAME&window=1d` | Target/key routing totals for `1h`, `1d`, `1w`, `1m`, or `all`. |
| `GET /api/requests?limit=20&before=CURSOR` | Cursor-paginated logical request records. |
| `GET /api/attempts?limit=100` | Recent per-target attempts. |

Request and attempt limits have a maximum of 1000. Routing statistics attribute
one logical request to its final recorded target; failed intermediate targets
remain separate attempts. In request records, `fallback` is `null` when no
target was attempted; `fallback_exhausted` is `true` when every configured
route was unavailable or failed.

## Storage and privacy

QuotaMux stores data in `server.data_dir/quotamux.redb`:

- logical request metadata;
- per-target attempt metadata;
- alerts;
- fixed-width statistics rollups;
- per-target circuit state.

Prompt bodies, response bodies, API keys, affinity fingerprints, and affinity
leases are not stored in the database. Affinity remains memory-only.

The configuration file contains plaintext API keys. Keep it out of version
control and restrict access:

```sh
chmod 600 quotamux.toml
```

## Validation and development

Validate configuration without starting the listener:

```sh
cargo run --bin quotamux -- --check
```

Validation covers adapter registration, endpoint ownership, credentials,
model/protocol contracts, aliases, layer structure, target references, pricing,
and affinity bounds. Errors do not include API key values.

Run the local checks:

```sh
npm --prefix frontend ci
npm --prefix frontend run build
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

Run a credential-free smoke test against a live instance:

```sh
cargo run --bin quotamux-smoke -- --base-url http://127.0.0.1:8080
```

The container build uses Rust 1.97, builds the frontend, embeds its compressed
assets in the binary, and runs as an unprivileged user in a read-only scratch
image.

## Design documents

- [Routing architecture](docs/routing-architecture-v3.md)
- [Prompt-prefix affinity](docs/prompt-prefix-affinity.md)
- [Provider failure policy](docs/provider-failure-policy.md)

## License

MIT
