# QuotaMux

QuotaMux is a local LLM gateway for quota-aware routing. It exposes stable
model names, distributes requests across equivalent upstream credentials, and
falls back through ordered capacity layers when an upstream cannot serve a
request.

It accepts OpenAI Chat Completions, OpenAI Responses, and Anthropic Messages.
When the selected upstream uses another supported protocol, QuotaMux translates
the request and response at the protocol boundary.

AWS Bedrock 的 EC2 Role、`us-east-1` 与 Global 推理接入见
[接入说明](docs/aws-bedrock.md) 和
[四款 Claude 模型的独立示例](quotamux.bedrock.example.toml)。

> [!IMPORTANT]
> QuotaMux does not authenticate inference clients. Bind it to localhost, keep it
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

Configuration uses readable TOML with `config_version = 4`. Existing version 3
configuration is automatically migrated without changing routing behavior. Array entries belong
to their closest preceding parent: `[[backends.credentials]]` and
`[[backends.models]]` belong to the preceding `[[backends]]`, while
`[[models.layers]]` belongs to the preceding `[[models]]`.

## Quick start

```sh
mkdir -p data/routing
chmod 700 data/routing
cp quotamux.example.toml data/routing/quotamux.toml
# Add the API keys required by your routes.
chmod 600 data/routing/quotamux.toml
cargo run --bin quotamux -- --config data/routing/quotamux.toml --check
docker compose up --build -d
curl http://127.0.0.1:8080/healthz
```

The API and Routing workspace are available at <http://127.0.0.1:8080>. Compose
mounts the private `data/routing` directory for validated configuration saves and
stores the request ledger in a named volume. The initial page is All Statistics
for 1 day; selecting a model also opens its Statistics for 1 day. Accounts is a
separate page with one tab per account, including its shared capacity limits and live occupancy.
Settings is a separate page for global runtime parameters.
Both pages show read-only information until Enable edit reveals their fields;
all pages share the workspace draft and confirmed Save transaction.
Each deletion requires confirmation before changing the draft. Save asks for
confirmation of all pending updates, validates changed upstream bindings,
persists readable TOML and immediately activates new requests. Existing requests
finish on their captured routing revision.

For an existing installation, copy the current `quotamux.toml` into `data/routing`
instead of copying the example. Version 3 migrates to version 4 at startup,
retaining the exact original document in a private `quotamux.toml.v3.bak` file.
The migration preserves model names, aliases, target order, credentials, protocol
and pricing settings. It adds no time windows or capacity limits. Read-only
configurations can still run using the migrated in-memory document; the workspace
shows when migration persistence needs a writable directory. Unknown schemas or
invalid references reject startup before a configuration rewrite.

Run development and E2E on isolated 8081 data. Promotion to production 8080 requires
the automated and browser acceptance gates described in
[Routing workspace](docs/routing-workspace.md).

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
config_version = 4

[server]
listen = "0.0.0.0:8080"
data_dir = "./data"
max_inference_body_bytes = 33554432

[server.timeouts]
upstream_connect_ms = 10000
upstream_read_ms = 90000
upstream_stream_read_ms = 300000
upstream_total_ms = 7200000
downstream_sse_heartbeat_ms = 15000
route_probe_wait_ms = 5000

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

`server.max_inference_body_bytes` is a finite limit on each encoded inference
JSON body, including Base64 image expansion. It defaults to 32 MiB when omitted
for compatibility with existing configuration files. Oversized requests are
rejected with `413 Payload Too Large` before any upstream attempt and are kept
in the request history as unattributed client errors.

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

## DeepSeek V4.1 Flash

QuotaMux supplies Go's required `x-opencode-session` header automatically.
It preserves a nonempty explicit value, otherwise reuses `x-claude-code-session-id`,
`session_id`, or `x-session-id` in that order. If none is available, it creates
one UUID per logical inference request and reuses it across fallback attempts.
Separate unidentified requests get separate IDs; cross-request session cache
optimization requires a stable caller session. Only Go receives this header.

Use public model ID `deepseek-v4.1-flash`. OpenCode Go and Ollama Cloud use
that same upstream ID; the DeepSeek official API uses `deepseek-flash`.
The example adds Go first and official pay-as-you-go fallback. Existing model
names remain separate. Production also includes its existing Ollama account and
uses the established weekday schedule: Ollama then Go, or Go then Ollama during
12:00–18:00 UTC weekdays, with DeepSeek official last.

Rates verified on September 11, 2026 are $0.003 cached input, $0.15 uncached
input, and $0.60 output per million tokens, doubled during each provider's
peak window. Go and DeepSeek peak windows are 01:00–04:00 and 06:00–10:00 UTC
weekdays; Ollama uses 12:00–18:00 UTC weekdays.
See [OpenCode Go](https://opencode.ai/docs/go/),
[Ollama pricing](https://ollama.com/pricing), and
[DeepSeek pricing and retirement notice](https://api-docs.deepseek.com/quick_start/pricing/).
DeepSeek official already redirects legacy Flash IDs to V4.1 Flash and will
redirect V4 Pro from September 14, 2026 at 12:00 Beijing time. Other providers'
legacy model IDs are not assumed to have changed versions.

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
| `zhipu-coding-plan` | `https://open.bigmodel.cn/api` | Chat, Responses, Anthropic |

For `opencode-go`, the adapter contains the official model-to-protocol catalog.
Declare only the exact model ID and omit `protocols`:

```toml
[[backends.models]]
name = "kimi-k3"
```

QuotaMux rejects a Go model that is not in the adapter catalog. The catalog
follows the [OpenCode Go endpoint table](https://dev.opencode.ai/docs/go/#endpoints).

Kimi K3.1 Pro uses the exact upstream model ID `kimi-k3.1-pro` on OpenCode Go.
Declare that ID in `backends.models` without `protocols`, and reference it from
the served model's route. Go uses native Chat Completions for this model;
Responses and Anthropic Messages ingress use the existing protocol translation.
K3.1 Pro requires thinking mode; QuotaMux preserves the caller's settings and
lets the upstream validate them. See the [Go model table](https://opencode.ai/v2/docs/console/go/)
and [Kimi model guide](https://www.kimi.com/code/docs/en/third-party-tools/opencode).

For the other official adapters, declare the native protocols enabled for each
model. The adapter verifies that every declaration is supported by that API.
Kimi Code and the Kimi Open Platform are separate services with separate keys;
their adapters and backends must remain separate. See the
[Kimi Code API overview](https://www.kimi.com/code/docs/) and
[Kimi Platform API overview](https://platform.kimi.com/docs/api/overview).

GLM Coding Plan uses separate official roots for OpenAI Chat, OpenAI
Responses, and Anthropic Messages. Declare all three native protocols on the
backend model so Responses requests use Zhipu's `/api/v1/responses` endpoint
without translation.
Zhipu `glm-5.3-flash` and OpenCode Go `glm-5.3-flash` both have 1M-token
multimodal context windows; do not substitute the text-only `glm-5.3` model.
See the [GLM Coding Plan endpoint guide](https://docs.bigmodel.cn/cn/coding-plan/quick-start),
the [official GLM-5.3-Flash announcement](https://z.ai/blog/glm-5.3-flash), and
the [OpenCode GLM-5.3-Flash model data](https://opencode.ai/data/zhipu/glm-5-3-flash).

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

Add `pricing_schedule` to select rates by the provider's weekly calendar:

```toml
pricing_note = "Published usage rates; subscription allowance value is not a cash invoice."
pricing_source = "https://ollama.com/pricing"
[backends.models.pricing_schedule]
timezone = "UTC"
[[backends.models.pricing_schedule.windows]]
name = "Weekday peak"
days = [1, 2, 3, 4, 5]
start = "12:00"
end = "18:00"
pricing = { cache_hit_input_usd_per_million = 0.014, cache_miss_input_usd_per_million = 0.44, output_usd_per_million = 1.32 }
```

Put these fields in the relevant `[[backends.models]]` table after its default
`pricing`. Windows require default rates and follow the same timezone, weekday,
inclusive-start/exclusive-end and overlap rules as routing schedules. Pricing
and routing have independent calendars in the same configuration document.
Each upstream attempt captures its start-time price, including streams that
finish after a boundary or a configuration save. Historical estimates retain
their recorded values. Providers publishing points or membership allowances
without USD token rates should use notes and leave `pricing` absent.
See [the researched Flash configuration](docs/flash-routing-plan.md) for provider
rates, billing context and the scheduled route.

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
  [error reference](https://www.kimi.com/code/docs/en/kimi-code/error-reference.html). An
  unrecognized Kimi Code `403` is retried once on the same target after
  200–500 milliseconds of jitter, then falls back for only that request without
  opening the shared target circuit.
- Kimi Official uses the structured Kimi Platform `error.type` values described
  in the [platform error reference](https://platform.kimi.com/docs/api/errors).
- GLM Coding Plan uses the official numeric business codes to distinguish
  request errors, rate pressure, subscription quota, billing, and account or
  model configuration failures.
- Custom adapters use generic HTTP classification only.

Request-specific `4xx` failures such as `400` and `422` are returned without
fallback or circuit impact. Provider-side failures open or suspend the exact
target and allow the router to continue. Ambiguous provider rejections are the
exception: they may retry and fall back, but they do not affect circuit health.

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
and process startup releases any half-open state immediately. If all remaining
routes are blocked by an in-flight probe, concurrent requests use one total
`route_probe_wait_ms` budget. Each probe state change triggers another route
evaluation, and the request keeps waiting within the remaining budget while an
unattempted route still has a probe in flight. A concrete target is selected
from the route graph at most once even if it appears in multiple layers (the
documented ambiguous-rejection retry remains internal to that one selection). A
timed-out synthetic `503` includes `Retry-After` rounded up from
`route_probe_wait_ms` (five seconds by default) so callers do not immediately
rejoin the same probe window.

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
| `GET /` | All Statistics by default, model-scoped views, and separate Accounts and Settings pages. |
| `GET /healthz` | Health, served models, and uptime. |
| `GET /api/status` | Configuration summary, target circuits, affinity state, and alerts. |
| `GET /api/stats?window=1d` | Aggregate request, account, token and recorded cost totals for `1h`, `1d`, `1w`, `1m`, or `all`; omission preserves lifetime totals. |
| `PUT /api/routing` | Validate, persist and activate a routing document; requires the current base revision. |
| `GET /api/routing` | Served models, ordered layers, targets, and circuit state. |
| `GET /api/routing/stats?model=NAME&window=1d` | Target/key routing totals for `1h`, `1d`, `1w`, `1m`, or `all`. |
| `GET /api/requests?limit=20&before=CURSOR` | Cursor-paginated logical request records; optional `model=NAME` filters before pagination. |
| `GET /api/attempts?limit=100` | Recent per-target attempts. |

Finite statistics ranges are rolling intervals ending at the last completed
minute. Current account/credential alerts remain on All, independent of the
statistics interval. Model statistics show No final target only for nonzero
requests lacking a recorded final layer/target.

Request and attempt limits have a maximum of 1000. Routing statistics attribute
one logical request to its final recorded target; failed intermediate targets
remain separate attempts. In request records, `fallback` is `null` when no
target was attempted; `fallback_exhausted` is `true` when every configured
route was unavailable or failed.

### Thinking-token estimates

`reasoning_tokens` is always the upstream's own number; `reasoning_reported`
says whether the upstream sent that split at all. Some endpoints stream the
thinking text but never return `completion_tokens_details` — Ollama Cloud's
OpenAI-compatible `/v1/chat/completions` is the configured example. For those
responses QuotaMux records `reasoning_estimated_tokens`, apportioning the exact
`output_tokens` total by the share of generated bytes that were thinking text:

```
reasoning_estimated_tokens = round(output_tokens * reasoning_bytes / generated_bytes)
```

`generated_bytes` counts thinking text, answer text, and generated tool-call
arguments. Thinking bytes are counted while streaming, and only a counter is
kept: no prompt, reasoning, or answer content is stored. The estimate is capped
at `output_tokens`, is written only when the upstream omitted the split and
thinking text was actually seen, and never replaces a reported count. It is a
QuotaMux statistic and is never injected into the relayed response, so a
downstream client still receives exactly the provider's usage fields. History
marks estimates with `≈`; model and account statistics add the estimated share
to the reported one and tag the column when any part of it was estimated.

Because thinking tokens are part of `output_tokens`, totals, cache accounting,
and estimated cost are unaffected. Byte share is an approximation: measured
against direct probes, thinking text runs at roughly 2.8–3.7 bytes per token, so
an estimate can sit a few percent below a provider-reported split.

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
cargo test --locked --all-targets --all-features -- --test-threads=1
```

Run browser E2E (launches an isolated mock-backed 8081 instance):

```sh
cargo build --locked --bins
npm --prefix frontend run test:e2e
```

Run a credential-free smoke test against an isolated live instance:

```sh
cargo run --bin quotamux-smoke -- --base-url http://127.0.0.1:8081
```

The container build uses Rust 1.97, builds the frontend, embeds its compressed
assets in the binary, and runs as an unprivileged user in a read-only scratch
image.

## Design documents

- [Routing workspace, scheduling, live saves and acceptance](docs/routing-workspace.md)

- [Routing architecture](docs/routing-architecture-v3.md)
- [Prompt-prefix affinity](docs/prompt-prefix-affinity.md)
- [Provider failure policy](docs/provider-failure-policy.md)

## License

MIT
