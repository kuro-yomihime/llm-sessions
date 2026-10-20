# Remote insert delivery research

Research for MR [!7](https://gitlab.com/wykwit/llm-sessions/-/merge_requests/7), including its request to support OpenClaw and Hermes outside tmux. Findings below distinguish agent APIs from the read-only llm-sessions federation API. Never edit an agent transcript to deliver input.

Research rechecked on 2026-10-20 against the primary documentation and pinned upstream source links below. These describe upstream contracts, not a verified deployment: no selected gateway instance or credentials were supplied here, and no live turn was dispatched. Endpoint reachability, deployed version, pairing grants, and ownership of a selected session must be checked on that instance.

## Harness binding and negotiation

Attach the harness **family once** to the selected session. Configuration binds the exact `(origin_id, source, session_id)` tuple; local and federated rows with the same session ID are not interchangeable. On **every write**, the bound harness resolves its own credentials and negotiates only its configured transports within one timeout budget:

- OpenClaw: authenticated WS handshake and native agent admission first; configured HTTP next if WS is unavailable before dispatch or explicitly unsupported; explicitly enabled co-located CLI last where safe. Remote rows cannot enable the local CLI.
- Hermes API: Runs first; synchronous chat only when Runs is explicitly unsupported (404/405/501) and an authenticated capabilities probe advertises both `features.chat_completions: true` and `features.session_continuity_header: "X-Hermes-Session-Id"`. Recheck this on each fallback; older servers may silently ignore an unknown continuity header. API-bound Hermes never silently switches to its CLI. Unconfigured Hermes retains the existing local/SSH CLI path and its ownership checks.
- Authentication, pairing, scope, policy, ownership and terminal execution refusals are failures, not permission to bypass a transport. A timeout/disconnect after a turn may have been submitted is an **unknown delivery outcome**: stop rather than submit through a second route. Idempotency on one endpoint does not establish cross-transport exactly-once execution.

The UI dispatches through the attached harness, not a global transport/family fallback chain. Credentials and gateway URLs belong in local configuration, not serialized/federated session records. Federation's read API is not an agent admission API.

The Rust entry point is `runtime::writer::attach`, returning a `SessionHarness`; insertion calls its `write_message(message, timeout)` method. `OpenClawHarness` and `HermesHarness` own their instance configuration. The existing `OmpHarness` and source-native command harness retain the local/SSH ownership guards in `runtime::insert` and `source::remote`. Network work runs off the TUI thread through the same delivery reporting lifecycle.

| Bound family | Delivery transports, in order within that instance |
| --- | --- |
| OpenClaw | Gateway WebSocket `agent` RPC → gateway HTTP chat-completions agent turn → explicitly enabled co-located `openclaw agent` CLI |
| Hermes API | HTTP Runs admission → synchronous HTTP chat completions when Runs is explicitly unsupported |
| Hermes native | Existing idle CLI, local or guarded SSH; no cross-over from an API-bound instance |
| OMP | Existing idle CLI or exact-owner tmux pane, local or guarded SSH |
| Other source-native families | Existing source CLI/live-server paths; unchanged support and refusals |

Arrows are safe availability fallback, not retries after an uncertain write. No row can fall through into another family.

### Configuration

`$XDG_CONFIG_HOME/llm-sessions/harnesses.json` (normally `~/.config/llm-sessions/harnesses.json`) uses a `sessions` array. Omitted/null `origin_id` selects a local row; a remote row requires its exact origin UUID. `source` selects the family and the `config` shape; there is no separate family discriminator.

```json
{
  "sessions": [
    {
      "origin_id": null,
      "source": "openclaw",
      "session_id": "11111111-1111-4111-8111-111111111111",
      "config": {
        "ws_url": "wss://openclaw.example.internal",
        "http_url": "https://openclaw.example.internal",
        "token_env": "OPENCLAW_TOKEN",
        "agent_id": "main",
        "session_key": "agent:main:main",
        "device_seed_env": "OPENCLAW_DEVICE_SEED",
        "cli": false
      }
    },
    {
      "origin_id": "22222222-2222-4222-8222-222222222222",
      "source": "hermes",
      "session_id": "api-owned-context-id",
      "config": {
        "base_url": "https://hermes.example.internal",
        "token_env": "HERMES_TOKEN",
        "instructions": "Follow the selected project's instructions."
      }
    }
  ]
}
```

These addresses and IDs are examples, not discovered instances. Set the named environment variables in the llm-sessions process environment. Never paste bearer tokens or private device seeds into session metadata. Only configure an OpenClaw HTTP session key after confirming it resolves to the selected session; the example key is not a universal mapping. `cli: true` is an explicit local-only opt-in and uses the local account's gateway configuration.

### Non-mutating availability checks

| Family/transport | Probe on the configured instance | What it does **not** prove |
| --- | --- | --- |
| OpenClaw WS | Wait for `connect.challenge`; complete authenticated, scoped `connect`; inspect advertised methods and resolve the selected session with supported session RPCs | An open TCP/WS socket does not prove auth, write scope, session existence or admission |
| OpenClaw HTTP | Authenticated `GET /v1/models`, documented alongside the opt-in chat endpoint; inspect status and agent targets | A listed agent is not proof of the selected session-key/UUID mapping; proxies/versions can expose different routes |
| OpenClaw CLI | Check executable/options and the co-located account's gateway connectivity/authentication without submitting a message | A working binary does not prove a particular UUID exists; ordinary CLI permits creation |
| Hermes HTTP | Authenticated `GET /v1/capabilities` and its advertised `endpoints`/`features`; inspect session continuity support | Health/models alone do not prove Runs support or authority over the selected session |

Recheck on each write; do not cache a winning transport globally across instances. Probe failures must retain their authentication/policy meaning. Do not probe by sending an empty/test agent turn, invoking `sessions_send`, or treating an arbitrary route's 404 as proof that every API on that instance is absent.

Probe references: OpenClaw [HTTP model listing](https://docs.openclaw.ai/gateway/openai-http-api), [WS versioning and negotiated limits](https://docs.openclaw.ai/gateway/protocol/versioning), and Hermes [capabilities](https://hermes-agent.nousresearch.com/docs/user-guide/features/api-server#get-v1capabilities). Honor WS `hello-ok.policy.maxPayload`/buffer limits and protocol negotiation; current operator clients require protocol 4, not the node-only protocol-3 compatibility window. Do not equate a timeout with explicit endpoint absence.

## OpenClaw

### Gateway WebSocket

The [gateway protocol](https://docs.openclaw.ai/gateway/protocol) is the native control plane. Wait for `connect.challenge`, then send a correlated `connect` request with protocol version, client identity, operator role/scopes and `auth.token`. Current upstream uses protocol 4. [Handshake](https://docs.openclaw.ai/gateway/protocol/handshake) and [authentication](https://docs.openclaw.ai/gateway/protocol/auth) are separate from admitting an agent turn.

Remote operator clients need a paired Ed25519 device identity, not merely a shared token. Sign the challenge timestamp and nonce with the device key; request operator read/write scope. The narrowly reserved direct-loopback `gateway-client`/`backend` shared-secret path can omit device identity. It is not a general remote authentication bypass. Pairing, scope mismatch, protocol mismatch, bad tokens, stale signatures, closed sockets and deadlines are distinct delivery failures. A connection succeeding does not establish that a turn was accepted.

The [RPC inventory](https://docs.openclaw.ai/gateway/protocol/rpc-methods) includes `sessions.send`, `chat.send` and `agent`. Use the operator agent-turn path with the selected session UUID rather than inventing a session key from its filename. Session keys and UUIDs are different: a key can roll to another UUID after reset. Validate session resolution before sending when a path routes by key. The agent owns serialization and persistence, including when the session is already live.


Upstream source was inspected at `aa7fc04fe0e2c582982bd8b04f15a86bcc6e6d8a`. The [agent schema](https://github.com/openclaw/openclaw/blob/aa7fc04fe0e2c582982bd8b04f15a86bcc6e6d8a/packages/gateway-protocol/src/schema/agent.ts) accepts `{"message":"…","sessionId":"<uuid>","createSessionIfMissing":false,"deliver":false,"idempotencyKey":"<operation id>"}`. `agentId` disambiguates the storage partition. Admission returns `runId` and `status`; `started`/`accepted`/`in_flight` are not model completion. Even an RPC envelope with `ok:true` can carry terminal `status:error` or `timeout`.

The [client enum](https://github.com/openclaw/openclaw/blob/aa7fc04fe0e2c582982bd8b04f15a86bcc6e6d8a/packages/gateway-protocol/src/client-info.ts) uses `id:cli, mode:cli`; `operator` is a role, not a client mode. [Device signing](https://github.com/openclaw/openclaw/blob/aa7fc04fe0e2c582982bd8b04f15a86bcc6e6d8a/packages/gateway-client/src/device-auth.ts) signs literal UTF-8 `v3|deviceId|cli|cli|operator|operator.read,operator.write|signedAt|token|nonce|linux|`. Device ID is lowercase hex SHA-256 of the raw Ed25519 public key; public key and signature are base64url without padding. The configured secret key must be a 32-byte seed, not an expanded 64-byte keypair. Pair this stable identity before use.

Use the challenge's `payload.ts` for `device.signedAt` and sign its nonce; do not reuse a signature across connections. Verify `hello-ok.auth.scopes` rather than trusting requested scopes. Device-token role/scope mismatch (`AUTH_SCOPE_MISMATCH`) differs from bad shared credentials (`AUTH_TOKEN_MISMATCH`) and requires operator action, not token-less reconnect. See the [auth contract](https://docs.openclaw.ai/gateway/protocol/auth).

[`sessions.send`'s pinned schema](https://github.com/openclaw/openclaw/blob/aa7fc04fe0e2c582982bd8b04f15a86bcc6e6d8a/packages/gateway-protocol/src/schema/sessions.ts#L457) instead accepts `{"key":"<canonical key>","message":"…","idempotencyKey":"…"}`; it is distinct from the `sessions_send` agent tool. `sessions.resolve` accepts `sessionId` and returns the canonical `key`. HTTP only routes by key: even an operator-provided mapping can point at a newer window after reset, so review mappings when resetting sessions.
### Gateway HTTP / REST

[POST /v1/chat/completions](https://docs.openclaw.ai/gateway/openai-http-api) is disabled by default; enable `gateway.http.endpoints.chatCompletions.enabled`. Authentication is `Authorization: Bearer <gateway token/password>`. These are powerful operator credentials, not scoped federation read tokens. Keep endpoints private or use TLS.

A request is `{"model":"openclaw/<agentId>","messages":[{"role":"user","content":"…"}],"stream":false}`. Explicit routing uses `x-openclaw-session-key`; without it, requests are stateless unless the OpenAI `user` field is used to derive a key. A UUID is not a valid substitute for a known session key. Do not silently route to the default agent or synthesize a key. HTTP errors include disabled endpoint (404), invalid input/reserved internal keys (400), auth/scope refusal (401/403), archived sessions (409), rate limits (429), backend failures and timeouts. Inspect the JSON response as well as the HTTP status.

Other REST surfaces are not interchangeable delivery APIs. [POST /tools/invoke](https://docs.openclaw.ai/gateway/tools-invoke-http-api) takes `{"tool":"…","args":{},"sessionKey":"…"}` and runs a policy-allowed tool; `sessions_send` is a tool name, not an HTTP route. It is on the default HTTP deny list, and `dryRun` is currently ignored, so tool invocation is not a safe delivery probe. Its 404 can mean policy denial, not an unavailable gateway. `/v1/responses` is separately opt-in. Neither is required for the chat-completions delivery path.

### Reachable-host CLI

[`openclaw agent`](https://docs.openclaw.ai/cli/agent) runs a gateway-backed turn: `openclaw agent --session-id <uuid> --message=<text> --json`. The CLI uses that account's gateway configuration, credentials and paired identity. In general it can be run on the gateway's host (including via an explicitly authenticated SSH command); this harness's configured CLI transport is deliberately **local/co-located only**, enabled by `cli: true` and refused for remote rows. Reachable SSH alone does not establish that the account's CLI controls the selected instance.

When the binding specifies a gateway URL, the harness pins CLI health and agent calls with `OPENCLAW_GATEWAY_URL`; a configured bearer secret is passed through `OPENCLAW_GATEWAY_TOKEN`, never argv. The [shared gateway call resolver](https://github.com/openclaw/openclaw/blob/aa7fc04fe0e2c582982bd8b04f15a86bcc6e6d8a/src/gateway/call.ts), [URL override resolver](https://github.com/openclaw/openclaw/blob/aa7fc04fe0e2c582982bd8b04f15a86bcc6e6d8a/src/gateway/client-bootstrap.ts), and [credential resolver](https://github.com/openclaw/openclaw/blob/aa7fc04fe0e2c582982bd8b04f15a86bcc6e6d8a/src/gateway/credentials.ts) honor those overrides for both commands. Without a URL override, explicit CLI-only bindings use the local account's gateway configuration.

Do not pass `--local` or use `agent exec`: those select embedded execution instead of submitting to the session's gateway. Missing CLI, unsupported options, unreachable gateway, auth/pairing refusal and nonzero exit status fail delivery. Current CLI exits nonzero for failed/cancelled/timed-out turns. A process deadline bounds a stuck CLI; killing the client does not prove the gateway did not admit its work.

Current upstream's ordinary `agent --session-id` CLI intentionally permits named-session creation if that UUID has disappeared, unlike the existing-only native WS request. This difference is a CLI-path limitation, not a guarantee that a stale row resumes its old transcript. The CLI checks the gateway's named-session-creation capability before dispatch.

## Hermes

Primary sources: [API server documentation](https://hermes-agent.nousresearch.com/docs/user-guide/features/api-server), [pinned API adapter](https://github.com/NousResearch/hermes-agent/blob/1f502aded81a07fa4127e6a90e06d04271a5b04a/gateway/platforms/api_server.py). The adapter advertises authenticated `/v1/capabilities`, Runs, model listing, and `X-Hermes-Session-Id` continuity. A deployment may be older; inspect the configured instance rather than assuming all current routes exist. Profile-prefixed base URLs such as `/p/<profile>` require that profile's own API key; the default-profile key must not be assumed valid there.


### Runs dispatch and SSE

`POST /v1/runs` requires `Authorization: Bearer <API_SERVER_KEY>` and JSON `{"input":"…","session_id":"<context session>","instructions":"optional task instructions"}`. Each message creates a **new run referencing context**. It does not type into a live CLI/TUI or modify the selected run. Optional `conversation_history` and `previous_response_id` supply alternative context; do not send competing context forms implicitly. With an existing API-owned session and no explicit history/response chain, the server loads active history when the queued turn executes.

The acknowledgement contains `run_id` and `status` (normally HTTP 202 / `started`). This proves admission, not model completion. Poll `GET /v1/runs/{run_id}` or subscribe to `GET /v1/runs/{run_id}/events` with the same bearer token. SSE carries lifecycle (`run.completed`, `run.failed`, `run.cancelled`, `run.interrupted`), token and tool events. Ignore `: keepalive` comments. An assistant finalization event alone is not proof of successful execution. Detached event buffers expire; reconnecting to events is not another dispatch.

Current upstream protects foreign-surface sessions: naming a CLI/TUI/Desktop/ACP/messaging-owned session returns HTTP 409 `permission_denied` without admitting a run. Do not bypass this with direct SQLite writes or a parallel embedded agent. Deployment versions may differ. Other failures include malformed input (400), missing/incorrect bearer token (401), unknown contexts, conflicting idempotency identity (409), concurrency/rate limits (429), runtime draining/storage unavailable (503), transport errors and deadlines. Server-side `Idempotency-Key` support exists for runs, but does not provide cross-protocol exactly-once delivery.

### Synchronous chat completions

Before the fallback POST, the harness sends authenticated `GET /v1/capabilities` and requires `{"features":{"chat_completions":true,"session_continuity_header":"X-Hermes-Session-Id"}}`. Missing capability metadata fails closed: merely sending an unrecognized header to a legacy server could create a different conversation. Profile URL prefixes and the original overall timeout are retained for this probe and the chat request.

`POST /v1/chat/completions` accepts `{"model":"hermes-agent","messages":[{"role":"user","content":"…"}],"stream":false}` with the same bearer token. Without an explicit continuity mechanism the complete context belongs in `messages`; it is not an arbitrary transcript append API. Current upstream supports `X-Hermes-Session-Id: <selected session_id>` for API-owned session continuity and refuses foreign-surface sessions. Send configured `instructions` as a system message on this path. Check `choices[].finish_reason`: `error` and `cancelled` are not success. Runs are preferable because their admission acknowledgement does not hold a connection for a complete tool-using turn. The harness may select synchronous chat only after an explicit Runs endpoint-unsupported response (404/405/501), never after an auth/ownership refusal, uncertain POST outcome, or runtime failure. Keep the same selected context; do not drop the header to obtain a successful new conversation.


Hermes source was inspected at `1f502aded81a07fa4127e6a90e06d04271a5b04a`. Its [Runs handler](https://github.com/NousResearch/hermes-agent/blob/1f502aded81a07fa4127e6a90e06d04271a5b04a/gateway/platforms/api_server_runs.py) emits SSE `data:` JSON containing `event`, `run_id`, `timestamp`, and `seq`, with `id: <seq>` for resumption; lifecycle names are in JSON, not necessarily named SSE `event:` lines. Reconnect using `Last-Event-ID`; closure is not a completion signal. `previous_response_id` means a Responses-store ID, never a run ID or SQLite session ID.

Runs idempotency is endpoint/profile/credential-scoped: current documentation specifies `Idempotency-Key` of 1–255 visible ASCII characters, HTTP 202 plus `Idempotency-Replayed: true` for an exact replay, and HTTP 409 `idempotency_key_conflict` for a changed JSON payload. Retention is 24 hours after the last status update. Do not retry under a fresh key after an uncertain outcome. A canonical authority restart can expose `status: unknown`, which is not successful completion and blocks later work pending explicit operator reconciliation. These are server-version-dependent guarantees, not a license for transport fallback.

`X-Hermes-Session-Key` is a separate long-term-memory/conversation identity header; it is not a replacement for the selected transcript's `session_id` or `X-Hermes-Session-Id`. The harness does not manufacture a memory key from a transcript filename.

## Verification

`cargo test` and `cargo test --no-default-features` pass. Each leaves the existing opt-in real-store OpenClaw test ignored. `cargo clippy --all-targets -- -D warnings`, `cargo fmt --check`, and a default-feature `cargo build` pass.

An isolated actual-TUI smoke run exercised OpenClaw WS-unavailable → authenticated HTTP, co-located CLI health → agent dispatch, Hermes Runs admission, Hermes Runs-unsupported → capabilities → continuity-bound synchronous chat, and the unchanged exact-owner OMP path through a real tmux pane. The pane received bracketed literal paste followed by Enter; shell-like message text remained literal. API servers and the agent CLI were fixtures, not live model deployments. No production credentials, paired remote gateway, or live agent turn was used.