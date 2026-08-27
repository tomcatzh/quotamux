# Provider failure and circuit policy

QuotaMux routes provider failures by meaning, not by HTTP status alone. This is
important for subscription APIs where the same status can describe temporary
capacity, an account quota, a bad request, or a permanent account problem.

Provider behavior is selected only by a registered adapter. Official
adapters own their endpoint, protocol contract, URL construction,
authentication, and error classifier. Generic `custom-*` adapters require an
exact configured request URL and use generic HTTP classification; the URL's
hostname or response payload can never activate OpenCode Go, Kimi, or DeepSeek
special handling.

## Circuit concurrency model

Every eligible attempt receives a permit for the target's current circuit
generation. The first provider-side failure accepted in a closed generation
opens the circuit and advances the generation. Late failures and successes from
the old generation are ignored. Concurrent failures therefore cause one
backoff transition rather than multiplying the backoff level by the number of
in-flight requests.

An open or suspended target admits one half-open probe when its retry deadline
is reached. Its permit owns the probe lease:

- success closes and resets the circuit;
- a provider failure advances the normal backoff once;
- a request-specific error or abandoned response releases the probe and makes
  it eligible again after one second, without increasing the backoff level;
- a ten-minute hard lease is persisted and can be reclaimed if asynchronous
  cleanup cannot run;
- startup releases any persisted half-open state immediately, because an
  in-flight request cannot survive a process restart.

This ownership also applies to streaming responses. Dropping a downstream
stream drops its permit, so a cancelled half-open probe cannot leave the target
permanently unavailable.

## Retry timing

`Retry-After` is parsed using HTTP semantics: an integer is seconds and an HTTP
date is an absolute time. Undocumented lookalike headers are ignored.

Provider retry hints are advisory. They are accepted only for transient,
capacity, subscription-quota, streaming, and transport failures, and are capped
by the failure class's maximum retry interval. Authentication, billing, and
configuration failures use QuotaMux's own schedule so a malformed header cannot
create a permanent suspension.

| Failure meaning | Backoff schedule | Maximum accepted retry hint |
| --- | --- | --- |
| transient, stream, transport | 1, 2, 4, 8, 16, 32, 60 seconds | 60 seconds |
| temporary capacity/rate pressure | 5, 30, 300 seconds | 300 seconds |
| subscription usage quota | 60, 300, 900 seconds | 900 seconds |
| auth/configuration/unknown provider 4xx | 5, 30, 60 minutes | ignored |
| billing/balance | 60 minutes | ignored |

Only the effective, capped next-probe deadline is persisted in circuit state.
The raw parsed header may remain in the attempt audit record for diagnosis, but
it is never reapplied after restart. On upgrade, loading an older open or
suspended circuit also clamps any previously persisted deadline that exceeds
the current failure-class ceiling.

The 15-minute quota ceiling is deliberate. Subscription reset windows can be
hours, a week, or a month, but QuotaMux must not blindly send all traffic to a
paid fallback for that entire period. A single lightweight half-open request
periodically checks whether the target is usable again.

## Provider-specific classification

### OpenCode Go

The public Go documentation defines 5-hour, weekly, and monthly dollar-value
limits, but does not publish an error wire contract. The official OpenCode
source fixture currently represents a Go limit as HTTP `429`,
`error.type = "GoUsageLimitError"`, a subscription-quota message,
`metadata.limitName`, and `Retry-After`. QuotaMux classifies the error type and
the confirmed `Subscription quota exceeded` message as `provider_quota`; it
does not infer quota state from `metadata.limitName`. An unrelated `429`
remains `provider_capacity`.

Sources:

- [OpenCode Go usage limits and endpoints](https://dev.opencode.ai/docs/go/)
- [Pinned official Go limit fixture](https://github.com/anomalyco/opencode/blob/0e3474509aa5ad16afcf9c439785514d6443c6af/packages/opencode/test/session/retry.test.ts#L325-L360)
- [Pinned official retry and Go error handling](https://github.com/anomalyco/opencode/blob/0e3474509aa5ad16afcf9c439785514d6443c6af/packages/opencode/src/session/retry.ts#L41-L132)

The source fixture is evidence of current official-client behavior, not a
published service guarantee. Unknown Go errors still fall through to the
generic HTTP policy.

### Kimi Code

Kimi Code is a membership product and is distinct from the Kimi Open Platform.
QuotaMux follows the Kimi Code error reference:

| Condition | Classification |
| --- | --- |
| invalid or expired credential | `provider_auth` |
| policy-terminated account access | `provider_configuration` |
| valid subscription lacks model/context/HighSpeed entitlement | `provider_configuration` |
| membership benefits temporarily cannot be verified (`402`) | `provider_transient` |
| weekly, rolling five-hour, or monthly membership usage limit | `provider_quota` |
| engine overload or concurrent-request pressure | `provider_capacity` |
| request/tool security validation | `client_request` |
| malformed client-generated `bot_id` rejected by Kimi | `client_request` |
| missing, disabled, temporarily disabled, or muted account | `provider_configuration` |
| server/infrastructure failure | `provider_transient` |
| unrecognized HTTP 403 | `provider_ambiguous_rejection` |

An unrecognized Kimi Code `403` is not evidence that the shared credential is
invalid. QuotaMux records the first rejection, waits for 200–500 milliseconds
of jitter, and retries the same target once. If the retry succeeds, routing
continues normally. If it fails again, only the current request falls back; the
failure does not open or advance the target circuit. A half-open probe lease is
released without treating the rejection as recovery evidence. Recognized quota,
permission, account, and request-security errors do not use this retry path.

Sources:

- [Kimi Code error reference](https://www.kimi.com/code/docs/en/kimi-code/error-reference.html)
- [Kimi Code membership limits](https://www.kimi.com/code/docs/en/kimi-code/membership.html)

### Kimi Open Platform

For the pay-as-you-go Open Platform, QuotaMux reads the structured
`error.type`. Overload and rate-limit types are capacity; current-quota
exhaustion is billing/balance; invalid keys are authentication; permission and
resource failures are configuration; request/content-filter errors belong to
the request; and server/unavailable errors are transient.

Source: [Kimi Open Platform error types](https://platform.kimi.ai/docs/api/errors)

### Zhipu Coding Plan

Zhipu returns a business code inside the JSON error envelope in addition to the
HTTP status. QuotaMux uses that code so subscription exhaustion is not confused
with temporary model pressure:

| Condition | Business codes | Classification |
| --- | --- | --- |
| invalid or expired credential | 1000–1005 | `provider_auth` |
| balance, expired plan, or invalid enterprise plan | 1113, 1309, 1314 | `provider_billing` |
| account, model, method, permission, or key-product mismatch | 1110–1112, 1121, 1211–1212, 1220–1222, 1311, 1315 | `provider_configuration` |
| malformed, oversized, or safety-blocked request | 1210, 1213–1215, 1261, 1301 | `client_request` |
| account rate pressure or model overload | 1302, 1305 | `provider_capacity` |
| daily, rolling, weekly, monthly, or fairness usage limit | 1304, 1308, 1310, 1313, 1316–1321 | `provider_quota` |
| account access, API flow, or network failure | 1120, 1200, 1230, 1234 | `provider_transient` |

Source: [Zhipu API error codes](https://docs.bigmodel.cn/cn/api/api-code)

## Generic fallback

Request-specific `4xx` responses such as `400`, `409`, `413`, `414`, `422`, and
`431` do not fall back or affect a target's circuit. Provider authentication,
configuration, capacity, quota, billing, transport, and server failures may
fall back before response commitment. JSON error envelopes delivered with HTTP
200, including the first SSE event, still pass through the selected provider's
classifier. After the first semantic streaming event, QuotaMux never switches
the current response, although a later provider-classified error or genuine
stream failure can affect later requests.

Provider response text is used only transiently for adapter classification.
QuotaMux persists and returns a controlled summary instead of copying upstream
messages, because providers and proxies may echo API keys, bearer tokens,
cookies, account identifiers, or private URLs. Error-body inspection stops at
16 KiB.
