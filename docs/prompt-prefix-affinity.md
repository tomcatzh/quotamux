# Prompt-prefix affinity routing

Status: active design; phase-three implementation contract

Prompt-prefix affinity routes any two requests with an identical effective
prompt prefix to an upstream cache domain that is likely to retain that prefix.
It is not session stickiness: requests may come from unrelated front-end
sessions and still benefit from the same upstream prompt/KV cache.

The directory is an optimization hint only. Stale, missing, or false-positive
metadata may reduce cache efficiency, but must never change model semantics or
authorization.

## Required behavior

Assume target A has processed a 7,000-token prompt and target B later receives a
different 6,000-token prompt whose first 3,000 tokens are identical. The 7,000
frontier is evidence that the shorter 3,000 prefix may also be warm. The index
must therefore keep the shorter prefix discoverable. It is incorrect to retain
only one longest frontier that can be found only by the complete original path.

The system separates:

- `PrefixIndex`: content-addressed checkpoints to candidate cache-domain leases;
- `CacheEvidence`: why a candidate is believed warm and until when.

Deleting dominated TTL evidence inside one path is allowed. Deleting the
short-prefix index entry needed by another branch is not.

## Canonical input

Affinity hashes the exact effective request representation sent to the provider,
not arbitrary raw client JSON. The representation is deterministic and
length-delimited. It includes all cache-relevant values:

- tenant/cache-sharing namespace;
- provider model and revision/template namespace;
- message roles and exact content bytes;
- system/developer instructions;
- tool definitions, tool calls, and tool results;
- multimodal object/content digests;
- every option that changes the provider's effective prompt.

Whitespace, Unicode, and tool JSON are not normalized unless the outbound
provider adapter performs the identical normalization. Hashing token IDs is
best when an exact provider tokenizer and chat template are available;
otherwise QuotaMux hashes canonical effective bytes and treats provider token
counts as evidence that requires a coordinate adapter.

## Checkpoints and hashing

The algorithm is incremental and independent of transport chunking. The initial
implementation uses a 128-bit streaming non-cryptographic hash at sparse
checkpoints rather than storing every character prefix.

Checkpoint boundaries include semantic message/tool boundaries and a fixed
interval over canonical bytes or Unicode scalar values. The interval is a
configuration/tuning parameter, initially chosen to keep metadata bounded. A
checkpoint key contains the namespace, checkpoint ordinal/length, and 128-bit
fingerprint. Length is part of the key even when the hash is 128-bit.

When the caller is untrusted, the implementation must prevent cheap deliberate
collision/route steering. A keyed hash or a per-installation secret finalizer is
preferred if the chosen non-cryptographic primitive is not denial-of-service
resistant.

## Cache-domain leases and frontier epochs

A prefix key points to a small set of lease IDs. A lease describes one cache
domain/path and contains non-dominated warm epochs:

```text
WarmEpoch {
    through_checkpoint,
    expires_at,
    cached_tokens,
    confidence,
}
```

An epoch covers every indexed checkpoint on that same path whose ordinal is no
greater than `through_checkpoint`. A candidate is warm for checkpoint `c` only
when an unexpired epoch exists with `through_checkpoint >= c`.

Epoch A dominates epoch B only when both are true:

```text
A.through_checkpoint >= B.through_checkpoint
A.expires_at >= B.expires_at
```

Dominated epochs may be removed. Non-dominated epochs must remain separate. A
later short-prefix observation must not incorrectly extend a previously longer
frontier, and a later long-prefix observation must retain the longer coverage.

Each prefix retains a bounded candidate set (initially top 4 or 8 overall, or a
small number per cache domain). Pruning may cause a conservative false negative;
it must never fabricate a longer or longer-lived cache claim.

## Provider evidence and TTL semantics

Provider-reported cached tokens are evidence, not a character offset. Mapping
them to a safe checkpoint requires a provider/model tokenizer and chat-template
adapter, or a deliberately conservative approximation. The safe frontier is the
greatest checkpoint whose required token count is no greater than the reported
cached-token count.

Provider cache TTL behavior is explicit:

- `sliding-on-read`: a verified hit may add an epoch ending at
  `now + provider_ttl - safety_margin`;
- `fixed-from-creation`: a hit verifies current presence but does not grant a new
  full TTL;
- `unknown`: use a shorter low-confidence evidence TTL and never present it as a
  hard upstream guarantee.

Logical expiry is immediate (`expires_at <= now` is a miss). Physical deletion
is lazy and batched.

## Lookup and selection

For a layer with more than one eligible target:

1. Canonically encode the request and collect checkpoints in one pass.
2. Search from the deepest checkpoint toward the shallowest.
3. Resolve valid lease candidates and choose the longest warm prefix.
4. Combine affinity with health/circuit eligibility. Version one of the library
   uses longest prefix first and random tie-breaking.
5. If no warm candidate exists, use the layer's normal random selector.
6. Mark the selected path pending with a short lease if configured, preventing a
   burst of identical cold requests from warming different targets.
7. After the upstream result, use actual cached-token evidence when present;
   otherwise add only lower-confidence success evidence.

If a layer has exactly one eligible target, QuotaMux skips canonical encoding,
hashing, index lookup, and affinity bookkeeping for that selection.

## Storage architecture

The in-memory index is authoritative for the online data path:

```text
request -> memory PrefixIndex -> selection
                         |
                         +-> bounded mutation channel
                                   |
                                   +-> single batch writer -> redb
```

redb is a warm-restart persistence/checkpoint layer, not a request-path lookup
database. One writer batches insert/update operations and expiry cleanup into
transactions. TTL refresh may leave stale expiry-index rows; GC validates the
current lease version before deleting the live prefix/lease.

Suggested redb tables:

- prefix key -> bounded lease references;
- lease ID -> cache domain, path/checkpoints, epochs, version;
- expiry bucket + lease ID + version -> empty value.

At startup QuotaMux scans persisted leases, drops expired evidence, rebuilds the
memory index, and tolerates losing all affinity metadata. A worker restart or
cache-domain generation change invalidates prior leases even when their wall
clock TTL has not elapsed.

## Library boundary

The affinity library accepts neutral routing identities and timestamps. It does
not depend on Axum, HTTP protocols, API keys, provider clients, or request/alert
storage. Its public operations are conceptually:

```text
fingerprint(canonical chunks) -> checkpoints
lookup(namespace, checkpoints, eligible domains, now) -> optional match
observe(path, domain, cache evidence, now) -> mutations
expire(now) -> removed counts
snapshot/restore -> persistence records
```

Time, randomness, and persistence are injected so tests are deterministic.

## Test matrix and concrete evidence

The library is accepted only with data-bearing assertions:

- the same bytes split into every possible chunk partition produce identical
  checkpoint keys;
- changing role, field length, whitespace, tool schema, namespace, model, or
  template changes the expected key;
- a 7,000-unit path makes its 3,000 checkpoint discoverable for a different
  6,000-unit branch;
- inserting a short newer epoch does not extend an old long frontier;
- dominated epochs disappear while non-dominated epochs remain;
- lookup returns exact matched checkpoint length and target, not only a boolean;
- expired evidence is ignored before GC and removed after GC;
- candidate pruning never returns a checkpoint not actually covered;
- restart restoration produces the same valid lookup result and drops expired
  entries;
- simultaneous observations and lookups remain race-free and bounded;
- an end-to-end mock-worker run records request count by target, matched prefix
  length, selection reason, and reported cache-hit tokens;
- the real-worker experiment records sanitized JSON/CSV evidence containing
  timestamp, request case ID, target identity, matched prefix length,
  provider-reported input/cache-hit tokens, and success/failure. It records no
  prompt bodies or API keys.
