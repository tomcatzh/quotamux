# QuotaMux routing verification evidence

Date: 2026-08-11 (Asia/Shanghai)

This report records the data produced by the configuration-v2, layered-routing,
protocol-translation, and memory-only prompt-prefix-affinity test runs. API keys,
prompt bodies, and model response bodies are excluded.

## Automated results

| Suite | Result | Concrete coverage |
| --- | ---: | --- |
| Library/unit tests | 51 passed, 0 failed | configuration, adapters, circuits, streaming hash partitions, prefix branches, TTL epochs, capacity bounds, concurrent lookup/update/GC, and cold restart |
| Local gateway end-to-end | 21 passed, 0 failed | all three ingress protocols, streaming and non-streaming translation, attempts, fallback, random layers, affinity, expiry, and single-target bypass |
| Real worker probe | 1 passed, 0 failed | OpenCode Go and DeepSeek configured as equivalent targets in one affinity layer |
| Static checks | passed | `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `git diff --check` |

The real-provider test is ignored during ordinary `cargo test` because it needs
the local ignored configuration and consumes live provider quota. It is run
explicitly with:

```sh
cargo test --test real_worker_affinity -- --ignored --nocapture
```

## Captured routing data

| Case | Observed result |
| --- | --- |
| Random, two same-layer workers | 200 requests: worker A 88, worker B 112; 200 persisted attempt rows, all tagged layer `plan` and selection `random` |
| Mock mixed-provider affinity | The 7,000-character request warmed worker B; the divergent 6,000-character branch returned to worker B with a 2,944-byte checkpoint match and 3 recorded cache-hit tokens |
| Affinity expiry | The same branch returned to normal random selection after the configured 200 ms logical TTL |
| Single-target layer | Selection reason was `single-target`; matched-prefix header was absent and in-memory affinity lease count remained 0 |
| Completed streaming request | Two streams both reached `[DONE]`; the branch request returned to warm worker A through `prompt-prefix-affinity` |

## Real OpenCode Go + DeepSeek evidence

The real probe used the two private credentials already present in the ignored
local configuration. It transformed them in memory into one temporary route
layer and sent two low-output Chat Completions requests with a long common
prefix and different suffixes.

```json
{
  "provider": "opencode-go",
  "matched_prefix_bytes": 5376,
  "first_cache_hit_tokens": 0,
  "first_cache_miss_tokens": 1351,
  "second_cache_hit_tokens": 1280,
  "second_cache_miss_tokens": 72
}
```

The first request was cold. The second request was selected by prefix affinity,
returned to the same real worker, and the provider reported that 1,280 of its
1,352 input tokens were cache hits. This is direct provider data, not an
estimate derived by QuotaMux.

## Privacy and lifecycle checks

- The prefix directory, keyed-hash secret, leases, and epochs exist only in
  process memory and start empty after restart.
- Existing request/attempt records retain only routing metadata and usage; they
  do not contain prompts or affinity fingerprints.
- The real probe prints only provider ID, checkpoint length, and aggregate token
  counts. It does not print credentials, endpoints, prompts, or responses.
