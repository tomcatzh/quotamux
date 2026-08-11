# QuotaMux configuration and routing architecture v2

Status: layered routing and affinity implemented; provider expansion partially scaffolded

This document records the product decisions for the first general-purpose
QuotaMux configuration and routing system. It replaces the version-one
assumption that exactly one OpenCode Go credential is the primary and exactly
one DeepSeek credential is the fallback.

## Goals

QuotaMux separates configuration into three layers:

1. Provider definitions describe real upstream services, credentials, and the
   exact upstream model identifiers enabled on each service.
2. Served models define the model names exposed by QuotaMux and the ingress
   protocols that clients may use.
3. Route layers connect served models to provider credentials. Layers are
   ordered from the preferred/free/plan capacity to progressively more
   expensive PAYG capacity.

Provider choice remains private. A client chooses a served model and an ingress
protocol, never a provider, credential, route layer, or upstream model.

## Terminology and identities

- `provider`: a configured upstream service such as DeepSeek Official,
  Kimi Official, Alibaba Cloud Model Studio, Ollama Cloud, OpenCode Zen,
  OpenCode Go, or a custom compatible endpoint.
- `credential`: one independently routable API key within a provider. Two keys
  of the same provider are two routing workers and may be separate cache
  domains.
- `upstream model`: the provider's exact model ID. QuotaMux does not invent or
  normalize this value.
- `target`: `(provider, credential, upstream model)`.
- `route layer`: a non-empty set of equivalent targets at the same priority and
  price/capacity class.
- `served model`: a QuotaMux-owned public name and its ordered route layers.
- `cache domain`: the smallest upstream scope known to share prompt/KV cache.
  Initially this is conservatively derived from provider, credential, endpoint,
  upstream model, and protocol. A provider-specific adapter may later declare a
  wider safe sharing scope.

The distinction between provider and credential is deliberate: one provider
type can have multiple keys, and every key must be independently selectable,
observable, circuit-broken, and affinity-routed.

## Provider layer

Every provider has a stable user-defined `id`, a provider `kind`, one or more
credentials, and an explicit list of enabled models. The enabled model name is
always the provider's standard API identifier.

The implemented provider adapters are currently `deepseek-official` and
`opencode-go`. Their official base endpoints may be omitted, and endpoint
overrides remain possible for local and integration tests.

The configuration enum also reserves `kimi-official`, `aliyun-bailian`,
`ollama-cloud`, `opencode-zen`, `custom-chat-completions`, `custom-responses`,
and `custom-anthropic`. These entries currently provide schema/default-endpoint
scaffolding only. They are not supported provider adapters until they have
provider-specific authentication, URL, stream, error, mock end-to-end, and
(where applicable) real credential tests.

Each enabled model records every provider protocol that actually serves it.
This is model-specific: for example, OpenCode currently documents different
endpoints for Responses, Anthropic Messages, and Chat Completions models. The
configuration is explicit even when QuotaMux can infer a default. Validation
becomes provider-specific as each reserved adapter is implemented.

These official references informed the schema and the planned adapters; they
are not acceptance evidence for the reserved provider kinds:

- DeepSeek exposes model IDs from `/models` and currently documents
  `deepseek-v4-flash` and `deepseek-v4-pro`:
  <https://api-docs.deepseek.com/api/list-models>
- OpenCode Go publishes a model-by-model endpoint table and `/models` endpoint:
  <https://opencode.ai/docs/go>
- OpenCode Zen likewise mixes Responses, Anthropic, and Chat Completions models:
  <https://opencode.ai/docs/zen>
- Kimi's official quick start uses the exact `kimi-k3` model ID and the
  OpenAI-compatible Chat Completions API:
  <https://platform.kimi.com/docs/overview>
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
configuration rather than silently introduced by provider mapping.

Supported public ingress protocols are:

- `openai-chat`: `POST /v1/chat/completions`
- `openai-responses`: `POST /v1/responses`
- `anthropic-messages`: `POST /v1/messages`

QuotaMux owns protocol equivalence. A provider's egress protocol must not limit
which configured ingress protocols a served model can expose. Requests and
responses pass through a canonical representation, with adapters on both sides:

```text
client protocol -> canonical request -> provider protocol
provider protocol -> canonical response/stream -> client protocol
```

The canonical boundary must preserve message roles, tool definitions, tool
calls/results, reasoning/thinking content, JSON response intent, stop controls,
stream termination, usage, cache token evidence, and provider errors whenever
the source and destination protocols have an equivalent representation.
Unsupported lossy features must fail validation before an upstream request;
they must never be silently dropped.

## Route-layer semantics

Route layers are evaluated in configuration order. Typical configuration uses:

1. a free or subscription-plan layer;
2. a PAYG layer;
3. optional additional PAYG layers with increasing price or capacity.

A layer may contain different providers and multiple credentials of the same
provider. All targets in a layer are declared semantically equivalent for the
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
   from disabling sibling credentials or the whole provider type.

`prompt-prefix-affinity` starts from the same randomized order, then promotes
the eligible cache domain with the longest currently warm canonical prefix.
When no prefix matches, it is exactly the normal random policy.

## TOML shape

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
id = "go"
kind = "opencode-go"

[[providers.credentials]]
id = "go-plan-a"
api_key = "..."

[[providers.credentials]]
id = "go-plan-b"
api_key = "..."

[[providers.models]]
name = "deepseek-v4-flash"
protocols = ["openai-chat"]

[[providers]]
id = "deepseek"
kind = "deepseek-official"

[[providers.credentials]]
id = "deepseek-payg"
api_key = "..."

[[providers.models]]
name = "deepseek-v4-flash"
protocols = ["openai-chat", "openai-responses", "anthropic-messages"]

[[models]]
name = "deepseek-v4-flash-0731"
aliases = ["deepseek-v4-flash"]
protocols = ["openai-chat", "openai-responses", "anthropic-messages"]

[[models.layers]]
name = "plan"
strategy = "random"
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

## Configuration validation contract

Startup fails with a path-specific error when any invariant is violated:

- `config_version` is unsupported;
- provider, credential, served-model, alias, or layer IDs are empty or
  duplicated in their scope;
- a provider has no credentials or no enabled models;
- an API key is empty;
- a custom/region-specific provider has no absolute HTTP(S) endpoint;
- an enabled model name is empty or its upstream protocol is unsupported;
- a served model exposes no ingress protocols or has no route layers;
- a layer is empty or uses an unknown strategy;
- a target references a missing provider, credential, or enabled upstream
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
- provider ID and kind;
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

- Parse and validate version-two configuration.
- Build provider clients and per-target circuits from provider/credential/model
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
- end-to-end mock-provider tests proving same-layer distribution, same-layer
  retry, later-layer fallback, no retry after stream commit, model rewriting,
  protocol conversion, metadata, attempts, and secret redaction;
- captured counts and concrete request/attempt records, not only exit status.

### Phase 3: prompt-prefix affinity

- Implement as a library independent of HTTP/provider code.
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
- client-directed provider or credential selection;
- cross-router distributed affinity state;
- correctness that depends on cache-directory freshness;
- pricing-aware scoring within a layer;
- switching upstream after a stream has been committed.
