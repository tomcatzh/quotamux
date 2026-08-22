# QuotaMux configuration and routing architecture v3

Status: layered routing and affinity implemented; adapter expansion partially scaffolded

This document records the product decisions for the first general-purpose
QuotaMux configuration and routing system. It replaces the version-one
assumption that exactly one OpenCode Go credential is the primary and exactly
one DeepSeek credential is the fallback.

## Goals

QuotaMux separates configuration into three layers:

1. Backend definitions describe configured upstream instances, their adapters,
   credentials, and exact enabled model identifiers.
2. Served models define the model names exposed by QuotaMux and the ingress
   protocols that clients may use.
3. Route layers connect served models to backend credentials. Layers are
   ordered from the preferred/free/plan capacity to progressively more
   expensive PAYG capacity.

Backend choice remains private. A client chooses a served model and an ingress
protocol, never a backend, adapter, credential, route layer, or upstream model.

## Terminology and identities

- `provider`: the external company or service, used only as descriptive prose
  (for example DeepSeek, Kimi, or OpenCode), not as a configuration identity.
- `adapter`: QuotaMux code that owns one known API's endpoint policy, protocol
  contract, URL construction, authentication, and error classification.
- `backend`: one user-defined upstream instance. It has a stable `id`, selects
  exactly one `adapter`, and enables credentials and exact upstream models.
- `credential`: one independently routable API key within a backend. Two keys
  on the same backend are two routing workers and may be separate cache
  domains.
- `upstream model`: the backend API's exact model ID. QuotaMux does not invent or
  normalize this value.
- `target`: `(backend, credential, upstream model)`.
- `route layer`: a non-empty set of equivalent targets at the same priority and
  price/capacity class.
- `served model`: a QuotaMux-owned public name and its ordered route layers.
- `cache domain`: the smallest upstream scope known to share prompt/KV cache.
  Initially this is conservatively derived from adapter, backend, credential,
  resolved endpoint, upstream model, and protocol. An adapter may later declare a
  wider safe sharing scope.

The distinction between adapter, backend, and credential is deliberate: one
adapter can serve multiple configured backends, one backend can have multiple
keys, and every key must be independently selectable, observable,
circuit-broken, and affinity-routed.

Version 3 is a deliberate hard configuration boundary: `[[providers]]` became
`[[backends]]`, backend `kind` became `adapter`, and route-target `provider`
became `backend`. Old configuration terms are rejected rather than accepted as
aliases. Persisted runtime records accept the former `provider` and
`provider_kind` names on read and serialize the canonical `backend` and
`adapter` names on subsequent writes.

## Backend and adapter layer

Every backend has a stable user-defined `id`, an `adapter`, one or more
credentials, and an explicit list of enabled models. The enabled model name is
always the selected upstream API's standard identifier.

The production-accepted official adapters are currently
`deepseek-official`, `opencode-go`, `kimi-code`, and `kimi-official`. Both Kimi adapters have
provider-specific URL, model identity, error, mock end-to-end, real streaming,
and 300,095-input-token acceptance. Kimi Code supports both native OpenAI Chat
and Anthropic Messages, so Claude Code traffic stays native whenever that
target is selected; it also completed a live two-turn Codex
Responses-to-Chat reasoning/tool loop. Official base endpoints belong to the
adapter and must not appear in customer configuration.

DeepSeek Official supports native OpenAI Chat Completions, OpenAI Responses,
and Anthropic Messages. Those three protocols are enabled explicitly so an
ingress request stays byte-shape-compatible with the official upstream whenever
DeepSeek is selected; translation is reserved for fallback targets that do not
offer the ingress protocol.

Three generic adapters, `custom-chat-completions`, `custom-responses`, and
`custom-anthropic`, require a complete exact endpoint and deliberately use only
generic protocol authentication and HTTP error classification. Endpoint text
never selects an adapter or activates official-provider behavior.

The enum retains `aliyun-bailian`, `ollama-cloud`, and `opencode-zen` only so
old persisted records remain readable. The runtime registry has no adapters for
them, so configuration validation rejects them. Unknown enum strings fail while
parsing.

Each enabled model records the backend protocol contract that actually serves
it. Official adapters whose exact model exposes several API families use a
`protocols` list. OpenCode Go documents exactly one endpoint family for each
model, so its registered adapter owns a strict model-to-protocol catalog and Go
configuration contains only the exact model ID. Unknown IDs, the removed
`endpoint_protocol` field, and customer-supplied Go `protocols` fail validation.
Validation becomes adapter-specific as each reserved adapter is implemented.

An enabled backend model may also declare three USD-per-million-token pricing
rates: cache-hit input, cache-miss input, and output. Cost calculation is
model-agnostic and runs only when both pricing and provider-reported usage are
available. The binary contains no provider or model prices. This keeps price
changes local to configuration and allows the same upstream model to have
different prices on different backends.

These official references define the external identities used by the adapters:

- DeepSeek currently documents `deepseek-v4-flash`, `deepseek-v4-pro`, and the
  multimodal `deepseek-v4-flash-vision-exp`; the vision model accepts Chat,
  Responses, and Anthropic image blocks:
  <https://api-docs.deepseek.com/guides/vision/>
- OpenCode Go publishes a model-by-model endpoint table and `/models` endpoint:
  <https://opencode.ai/docs/go/>
- OpenCode Zen likewise mixes Responses, Anthropic, and Chat Completions models:
  <https://opencode.ai/docs/zen>
- Kimi's China Open Platform uses the exact `kimi-k3` model ID, a 1M context
  window, and the OpenAI-compatible Chat Completions API:
  <https://platform.kimi.com/docs/guide/kimi-k3-quickstart>
- Kimi Code is a separate credential system at
  `https://api.kimi.com/coding/v1`; API calls use model ID `k3`, and
  Allegretto unlocks up to 1M context:
  <https://www.kimi.com/code/docs/>
- Alibaba Cloud Model Studio model IDs and endpoints vary by model and region:
  <https://help.aliyun.com/en/model-studio/models>
- Ollama lists cloud models through `https://ollama.com/api/tags`; its official
  compatibility API documents both Chat Completions and Responses, so an
  enabled cloud model may explicitly declare either supported protocol:
  <https://docs.ollama.com/api/openai-compatibility>

## Served-model layer

A served model defines:

- its public `name`;
- optional explicit `aliases` for compatibility or intentional client
  impersonation;
- the ingress protocols enabled by QuotaMux;
- an ordered, non-empty list of route layers.

Ordinary names should describe the actual model family. An alias that
impersonates another model is supported, but it must be explicit in
configuration rather than silently introduced by adapter mapping.

Supported public ingress protocols are:

- `openai-chat`: `POST /v1/chat/completions`
- `openai-responses`: `POST /v1/responses`
- `anthropic-messages`: `POST /v1/messages`

QuotaMux owns protocol equivalence. A backend's egress protocol must not limit
which configured ingress protocols a served model can expose. Native requests
are forwarded directly after model rewriting. Cross-protocol requests and
responses pass through protocol translators:

```text
client protocol -> canonical request -> backend protocol
backend protocol -> canonical response/stream -> client protocol
```

The translation boundary maps only fields whose meaning and wire shape are
documented on both sides: text messages, ordinary function definitions and
calls/results, supported reasoning-effort values, compatible JSON-output
formats, stream termination, usage, cache-token evidence, and provider errors.
Unknown extensions and source-only structures are omitted. They are never
copied under a guessed field name, flattened across namespaces, or serialized
into prompt text.

Provider-owned opaque state is not synthesized. In particular, translated Chat
reasoning is not emitted as an Anthropic `thinking` block because QuotaMux
cannot produce Anthropic's verifiable signature. Real Anthropic thinking and
redacted-thinking blocks remain transparent only on the native Anthropic path.

## Inbound control surface

The request body `model` field selects the server-configured route graph.
Protocol-defined prompt fields may reorder equivalent targets inside a
prompt-affinity layer, and `stream` selects response transport, but neither can
choose a backend, adapter, credential, or route layer. No client field or header has
that authority.

- `provider`, `credential`, `route_layer`, `X-Provider`, and inbound
  `X-Relay-*` metadata are not inspected. A native request body remains
  transparent to its upstream; a protocol translation ignores unknown fields.
- `X-Relay-Include-Metadata: 1` opts into QuotaMux response diagnostics. The
  returned `X-Relay-*` headers are output metadata, not inputs to routing.
- `anthropic-version` and `anthropic-beta` are Anthropic protocol headers. They
  are forwarded only when the selected egress protocol is Anthropic Messages.
- `X-Claude-Code-Session-Id`, `X-Claude-Code-Agent-Id`, and
  `X-Claude-Code-Parent-Agent-Id` are bounded correlation metadata recorded for
  observability; they never affect selection or circuit state.
- Client authentication headers are never used to choose an upstream and are
  replaced by the credential named in the configured route target.

## Route-layer semantics

Route layers are evaluated in configuration order. Typical configuration uses:

1. a free or subscription-plan layer;
2. a PAYG layer;
3. optional additional PAYG layers with increasing price or capacity.

A layer may contain different backends and multiple credentials of the same
backend. All targets in a layer are declared semantically equivalent for the
served model. QuotaMux is therefore allowed to select any healthy target in the
layer.

Every layer chooses either `random` or `prompt-prefix-affinity`. Both preserve
the same health and fallback semantics:

1. Resolve the served model and ingress protocol.
2. For the first route layer, collect targets that are configured, protocol-
   compatible through an adapter, and not currently suspended by their circuit.
3. If exactly one target is eligible, select it directly and skip RNG/affinity
   work.
4. If multiple targets are eligible, choose a uniformly random start target.
5. On a pre-commit failure that permits fallback, try every remaining eligible
   target in that same layer at most once, in randomized order.
6. Only after the layer is exhausted, continue to the next layer.
7. A client/request validation error never falls back. Once a streaming
   response is semantically committed to the client, it never switches target.
8. Per-target circuit state survives restarts and prevents a failed credential
   from disabling sibling credentials or the whole backend instance.

`prompt-prefix-affinity` starts from the same randomized order, then promotes
the eligible cache domain with the longest currently warm canonical prefix.
When no prefix matches, it is exactly the normal random policy.

## TOML shape

```toml
config_version = 3

[server]
listen = "0.0.0.0:8080"
data_dir = "./data"

[affinity]
checkpoint_bytes = 128
max_checkpoints_per_path = 4096
max_candidates_per_prefix = 8
max_leases = 16384
success_ttl_ms = 300000

[[backends]]
id = "go"
adapter = "opencode-go"

[[backends.credentials]]
id = "go-plan-a"
api_key = "..."

[[backends.credentials]]
id = "go-plan-b"
api_key = "..."

[[backends.models]]
name = "deepseek-v4-flash"
# Static estimates use the current off-peak rates; peak rates are 2x.
pricing = { cache_hit_input_usd_per_million = 0.007, cache_miss_input_usd_per_million = 0.22, output_usd_per_million = 0.66 }

[[backends]]
id = "deepseek"
adapter = "deepseek-official"

[[backends.credentials]]
id = "deepseek-payg"
api_key = "..."

[[backends.models]]
name = "deepseek-v4-flash"
protocols = ["openai-chat", "openai-responses", "anthropic-messages"]
pricing = { cache_hit_input_usd_per_million = 0.007, cache_miss_input_usd_per_million = 0.22, output_usd_per_million = 0.66 }

[[models]]
name = "deepseek-v4-flash-0731"
aliases = ["deepseek-v4-flash"]
protocols = ["openai-chat", "openai-responses", "anthropic-messages"]

[[models.layers]]
name = "plan"
strategy = "random"
targets = [
  { backend = "go", credential = "go-plan-a", model = "deepseek-v4-flash" },
  { backend = "go", credential = "go-plan-b", model = "deepseek-v4-flash" },
]

[[models.layers]]
name = "payg"
strategy = "random"
targets = [
  { backend = "deepseek", credential = "deepseek-payg", model = "deepseek-v4-flash" },
]
```

## Configuration validation contract

Startup fails with a path-specific error when any invariant is violated:

- `config_version` is unsupported;
- backend, credential, served-model, alias, or layer IDs are empty or
  duplicated in their scope;
- a backend has no credentials or no enabled models;
- an API key is empty;
- an official-adapter backend attempts to configure an endpoint;
- a custom-adapter backend has no absolute HTTP(S) endpoint;
- an adapter name is unknown or has no registered implementation;
- an enabled model name is empty or its upstream protocol is unsupported;
- a served model exposes no ingress protocols or has no route layers;
- a layer is empty or uses an unknown strategy;
- a target references a missing backend, credential, or enabled upstream
  model;
- one public model name/alias resolves to more than one served model;
- a target cannot be adapted to every ingress protocol exposed by its served
  model.

Secrets must never appear in validation errors, logs, status responses,
metadata, tests, or committed files.

## Observability and persisted records

Provider enum variants are not sufficient once instances are user-defined.
Request, attempt, alert, status, and statistics records must identify:

- served model;
- route layer name and index;
- backend ID and adapter;
- credential ID;
- upstream model;
- ingress and egress protocol;
- randomized/affinity selection reason;
- whether the request reached a later layer;
- cache-hit/miss evidence reported by the provider.

Existing redb records require backward-compatible deserialization or a clear
schema migration. API keys and prompt/model response bodies remain excluded.

## Delivery and acceptance plan

All three phases below are implemented. The captured unit, end-to-end, and
real-provider results are in [test-evidence.md](test-evidence.md).

### Phase 1: design record

- This document and the separate prompt-prefix-affinity design are present.
- Every ambiguous term has a stable definition.
- Configuration and routing validation rules are executable test cases.

### Phase 2: three-layer runtime with random selection

- Parse and validate version-three configuration.
- Build backend clients and per-target circuits from backend/credential/model
  identities.
- Resolve public models dynamically in `/v1/models` and request validation.
- Select one target without RNG overhead; randomize two or more targets.
- Exhaust a layer before falling back to the next layer.
- Preserve non-streaming and streaming pre-commit fallback rules.
- Translate every configured ingress/egress protocol pairing or reject the
  configuration when equivalence is not implemented.

Evidence required:

- unit tests for all validation invariants and route-order behavior;
- deterministic selector tests using an injected RNG/seed;
- end-to-end mock-backend tests proving same-layer distribution, same-layer
  retry, later-layer fallback, no retry after stream commit, model rewriting,
  protocol conversion, metadata, attempts, and secret redaction;
- captured counts and concrete request/attempt records, not only exit status.

### Phase 3: prompt-prefix affinity

- Implement as a library independent of HTTP/backend transport code.
- Keep the online lookup/index in memory.
- Keep affinity metadata memory-only; a restart intentionally starts cold.
- Bypass hashing/index lookup for a layer with one eligible target.
- Integrate behind the layer-local selector interface.

Evidence required:

- unit/property-style tests for chunk-independent prefix hashing, canonical
  encoding, longest-prefix lookup, branch retention, expiry, epoch dominance,
  collision namespace separation, candidate limits, and cold restart behavior;
- concurrency tests for read/update/GC interaction;
- end-to-end mock-worker tests that prove two prompts sharing a long prefix are
  routed to the warm target while diverging/expired prompts are not;
- a real-worker experiment with two equivalent targets in one layer (for
  example OpenCode Go plus DeepSeek PAYG), recording target selection and actual
  provider-reported cached-token counts without recording prompts or secrets.

## Non-goals for the first implementation

- provider discovery that silently enables every upstream model;
- client-directed backend, adapter, or credential selection;
- cross-router distributed affinity state;
- correctness that depends on cache-directory freshness;
- pricing-aware scoring within a layer;
- switching upstream after a stream has been committed.
