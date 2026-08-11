# QuotaMux routing verification evidence

Date: 2026-08-11 (Asia/Shanghai)

This report records the data produced by the configuration-v2, layered-routing,
protocol-translation, and memory-only prompt-prefix-affinity test runs. API keys,
prompt bodies, and model response bodies are excluded.

## Automated results

| Suite | Result | Concrete coverage |
| --- | ---: | --- |
| Library/unit tests | 56 passed, 0 failed | configuration, provider URL/error adapters, legacy audit-record compatibility, circuits, streaming hash partitions, prefix branches, namespace isolation, TTL epochs, capacity bounds, concurrent lookup/update/GC, and cold restart |
| Local gateway end-to-end | 23 passed, 0 failed | all three ingress protocols, streaming and non-streaming translation, attempts, fallback, random layers, affinity, expiry, single-target bypass, and Kimi's three-provider/two-layer route |
| Real worker probe | 1 passed, 0 failed | OpenCode Go and DeepSeek configured as equivalent targets in one affinity layer |
| Real Kimi K3 acceptance | 3 passed, 0 failed | three direct streams, mixed subscription affinity, and explicitly gated 300,095-input-token requests to all three targets |
| Codex CLI worker | passed, 2 gateway turns | Codex 0.145 through `http://127.0.0.1:8080/v1`, Responses-to-Chat streaming, shell tool execution, reasoning history, provider cache usage, and same-target prefix affinity |
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
| Kimi two-layer fallback | Both subscription targets were attempted before exactly one Kimi Official PAYG attempt; upstream model rewrites were `kimi-k3`, `k3`, and `kimi-k3` respectively |
| Kimi mixed-model affinity | Two divergent-prefix requests returned to `opencode-go-kimi`; the candidates used isolated `opencode-go/kimi-k3` and `kimi-code/k3` namespaces |
| Responses model identity | A real non-streaming `/v1/responses` call returned public model `kimi-k3`, a 17-token reasoning item, exact text `MODEL_FIX_OK`, and internally used the OpenCode Go Chat route |
| Codex multi-turn affinity | The first worker turn selected Kimi Code randomly; the tool-result turn returned to it through `prompt-prefix-affinity` with a 47,616-byte match |

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

## Kimi real acceptance evidence

The Kimi-specific real suite in `tests/real_kimi_k3.rs` was run against the
three private targets in the local ignored configuration:

| Target | Exact upstream model | Real stream | Large-input result |
| --- | --- | ---: | ---: |
| OpenCode Go | `kimi-k3` | HTTP 200 and `[DONE]` | HTTP 200, 300,095 input tokens |
| Kimi Code Allegretto | `k3` | HTTP 200 and `[DONE]` | HTTP 200, 300,095 input tokens |
| Kimi China Open Platform | `kimi-k3` | HTTP 200 and `[DONE]` | HTTP 200, 300,095 input tokens |

The large-input test is gated behind `QUOTAMUX_CONFIRM_1M=1`. Every provider
reported more than 256K input tokens, so these results exercise the requested
1M-capable routes rather than merely relying on the configured model names.

The standalone mixed-subscription affinity test returned the divergent request
to OpenCode Go with an 8,064-byte matched prefix. That response did not report
cache-hit tokens, so it proves QuotaMux target affinity but is not presented as
proof of an upstream KV-cache hit. The Codex worker run below supplies direct
provider-reported cache evidence on Kimi Code.

## Live Codex worker and reasoning-conversion evidence

Codex 0.145 was configured with `base_url = "http://127.0.0.1:8080/v1"`,
`wire_api = "responses"`, a 1,048,576-token context window, low reasoning, and
`web_search = "disabled"`. It was instructed to execute `pwd` exactly once and
then return a fixed marker. The command completed with exit code 0 and Codex
returned `WORKER_OK` plus the correct temporary-directory basename.

QuotaMux recorded the two streaming turns as follows:

| Turn | Selection | Matched prefix | Provider input | Cache hit | Output | Reasoning |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| Initial request/tool call | `random` | none | 53,048 | 1,792 | 52 | 1 |
| Tool result/final answer | `prompt-prefix-affinity` | 47,616 bytes | 53,175 | 52,992 | 30 | 1 |

Both turns had ingress `openai-responses`, egress `openai-chat`,
`translated = true`, provider `kimi-code`, credential
`kimi-code-allegretto`, upstream model `k3`, layer `subscriptions`, HTTP 200,
and provider-reported usage. Codex independently summarized 106,223 input
tokens, 54,784 cached input tokens, 82 output tokens, and 2 reasoning tokens;
all four totals exactly equal the sums of the two QuotaMux attempt records.
This verifies request conversion, streaming response conversion, reasoning
accounting, tool-call history, and real cache-aware routing in one worker loop.

Codex's default cached web-search tool must be disabled for this route because
the Kimi Chat endpoint cannot execute a hosted Responses web-search tool.
Function tools such as the local shell remain available and translated.

## Privacy and lifecycle checks

- The prefix directory, keyed-hash secret, leases, and epochs exist only in
  process memory and start empty after restart.
- Existing request/attempt records retain only routing metadata and usage; they
  do not contain prompts or affinity fingerprints.
- The real probe prints only provider ID, checkpoint length, and aggregate token
  counts. It does not print credentials, endpoints, prompts, or responses.
