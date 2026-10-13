# Federation identity, deduplication, and cycle prevention

## Goal

Preserve chained federation while displaying each session once, attributed to its actual origin rather than the peer that forwarded it. Keep discovery finite, with the primary cycle guard running before a recursive session query.

Example: the local instance queries A and B, while A also queries B. B's session must appear once, attributed to B, with direct and indirect access represented as routes to the same session.

## Original behavior and evidence

- `Host::list_via_llm_sessions` in `src/source/remote/mod.rs` overwrites every received session's `host` with the queried peer's configured name.
- `src/catalog/scan.rs` and the remote background refresh in `src/source/remote/mod.rs` append remote rows without deduplication.
- `SessionSummary.host` currently represents a configuration-local host label. Transcript opening and resume also use it for connection selection.
- The remote transport invokes ordinary `llm-sessions list`, which can recursively discover that instance's configured peers.

An isolated reproduction ran the actual CLI with fixture SSH responses: A returned a session attributed to B, while B returned that same session as local. Running `llm-sessions list --json --limit 0 --archived` produced two rows:

```text
('session-on-B', 'A')
('session-on-B', 'B')
```

The fixture was removed; no implementation changes were made during the investigation.

## 1. Stable origin identity

### Identity belongs to the origin

Persist a UUID on each originating host/account, under the state namespace
(`$XDG_STATE_HOME`, default below):

```text
~/.local/state/llm-sessions/origin-id
```

Example contents:

```text
9c66d120-48bf-44c5-a129-6d8d45c3e2a8
```

This identifies a host/user session namespace, not an observer, SSH alias, IP address, display name, or individual session.

Every access method reads the same marker:

- Local `llm-sessions` on B reads B's marker.
- A accessing B through SSH/shell reads B's marker.
- C accessing B through SSH/shell reads B's marker.
- Mounted-file access must expose B's marker as well as its session stores.
- Forwarders preserve the origin ID received with a session.

Do not derive canonical identity from hostnames, addresses, SSH aliases, or configuration-local labels. `/etc/machine-id` alone is also unsuitable: it does not identify the user/session namespace and may be duplicated by cloned images.

### Lifecycle

- Generate the UUID once during setup on the origin or initialization by its local `llm-sessions`.
- Initialization must be atomic and create-only. Concurrent initializers must converge on the stored identity, never overwrite it.
- Preserve the marker across restarts, upgrades, and restoration of the same instance.
- Independent clones must regenerate the marker; copied IDs would cause false deduplication and false cycle detection.
- Remote listing must not silently create the marker on another host.
- Shell-only, read-only deployments must provision it beforehand.
- If the marker is unavailable, report that canonical identity is unavailable. Do not invent an observer-specific ID and claim reliable deduplication.
- Transcript files alone cannot reliably identify their originating host.

## 2. Session identity and attribution

The globally unique session key is:

```text
(origin_id, source, session_id)
```

Keep the existing agent-assigned session `id` unchanged. Add origin identity alongside it; do not replace the agent ID with a concatenated composite string.

Example identity fields:

```json
{
  "origin_id": "9c66d120-48bf-44c5-a129-6d8d45c3e2a8",
  "source": "omp",
  "id": "existing-agent-session-id"
}
```

Separate three concepts in the model:

1. **Origin identity:** stable persisted ID used for equality and deduplication.
2. **Origin display label:** human-readable attribution; not an identity key.
3. **Access route:** how the current instance reaches the origin/session.

Locally produced sessions acquire the local origin ID. Forwarded sessions retain their origin unchanged at every hop. Receiving B's session through A must not relabel its origin as A.

Host labels may differ between observers. Prefer the destination's configured label for a directly known origin; otherwise preserve a remote-provided origin label. Label selection must not change identity.

## 3. Access routes and operations

These are two routes to one session:

```text
local -> B
local -> A -> B
```

Display B as the origin, optionally showing `via A` for an indirect route. Do not require the final destination to have B configured directly.

- Retain access routes separately from the origin fields.
- Prefer a direct successful route; otherwise prefer a shortest successful route.
- Use a deterministic tie-break for equally short routes.
- Use an origin-qualified session reference for delegated operations so an intermediary can resolve the intended session onward.
- Transcript opening and resume must target the actual origin, including when it is only reachable through an intermediary.
- Do not fall back to interpreting an origin display label as a locally configured SSH destination.
- Do not identify a delegated session by its bare agent session ID when several origins or sources could match.

The existing `session.host`-based connection selection must be migrated, not merely left behind after preserving the incoming host string. CLI transcript opening, TUI transcript fetching, and resume are affected.

## 4. Shared deduplication

Use one merge implementation for CLI scans and TUI background results.

- Merge rows by `(origin_id, source, session_id)`.
- Deduplicate before filtering, limiting, grouping, or counting.
- Retain routes to the merged identity separately.
- Select one complete observation deterministically, preferring the shortest successful route. Do not combine fields from potentially inconsistent snapshots.
- Identical session IDs on different origins or from different sources remain distinct.
- Update affected identity consumers, selection state, and caches to avoid confusing sessions from different origins.
- Local-only operations must remain local-only; a forwarded row must not be treated as local merely because its origin matches a familiar label.

## 5. Caller-side cycle prevention

### Request context

Each discovery request carries:

- A path of visited origin IDs: the ancestors of the receiving instance.
- A remaining hop count to bound traversal depth.

The path is request-local, not persistent instance state. Each parallel branch receives its own immutable path value.

### Primary guard: before querying a peer

Extend the existing remote probe to read the peer's origin marker. Cache that ID alongside the peer's remote home and transport capabilities.

Before requesting sessions from a configured peer, compare its origin ID with the visited path. If it is already present, skip the edge without issuing a recursive session query.

For `A -> B -> A`:

1. A queries B with `visited = [A]`.
2. B extends the path to `[A, B]`.
3. B recognizes its configured peer's origin ID as A.
4. B skips querying A.

For `L -> A -> B -> A`, B must also skip A. Carry all ancestors, not just the original requester or immediate parent.

Peer identity may require an SSH probe when not already known. The guarantee is no recursive session query along a known cyclic edge, not necessarily zero contact with the peer.

### Defensive receiving-side guard

On entry, an instance checks whether its own ID is already in the incoming path. If so, return an empty branch result without local listing or further traversal.

Keep this check even with caller-side prevention: it protects against stale cached peer identity or callers that omit the pre-query guard. Normal operation stops the cycle at the caller.

### Discovery algorithm

```text
discover(incoming_path, remaining_hops):
    own_id = read local origin marker

    if own_id is in incoming_path:
        return empty branch result

    visited = incoming_path + own_id
    result = list local sessions with own_id

    if remaining_hops == 0:
        report truncation if configured peers would need traversal
        return result

    for each configured peer, independently:
        peer_id = obtain peer identity from probe

        if peer_id is in visited:
            continue

        result += peer.discover(visited, remaining_hops - 1)

    return deduplicate(result)
```

Identity-probe failures must be surfaced, not treated as permission for unguarded recursive discovery. Shell-only peers are leaf listings: use their marker for attribution, but do not recurse through their configuration.

### Branch-local paths

In a diamond:

```text
L -> A -> B
L -> C -> B
```

B receives `[L, A]` on one branch and `[L, C]` on the other. Both visits are allowed: neither incoming path contains B. The final merge deduplicates B's sessions.

This deliberately permits repeated discovery across independent routes. It avoids distributed visited-set coordination and prevents one failing route from suppressing another working route. It does not guarantee only one query per origin across the whole graph.

### Depth bound

A visited path prevents cycles but does not bound long chains or the number of distinct paths in a dense graph. Add a finite hop limit as a secondary safety bound.

At the limit, return the current instance's local sessions, stop further traversal, and report truncation. Skipping a cyclic edge is normal traversal, not a failure.

The concrete default hop limit remains an implementation choice; the agreed semantics are bounded depth and explicit truncation reporting.

## 6. Federation protocol compatibility

Introduce a federation-aware invocation that carries the visited path and remaining hop count across SSH/process boundaries. Ordinary remote `list` without this context cannot safely serve as recursive discovery.

Probe capability support before using recursive federation.

- Never fall back to an unguarded recursive `list` on an older remote.
- If a peer lacks federation-context support, use the existing shell transport to list its local stores and report the limitation.
- The shell path still reads the same origin marker.
- Missing origin identity must be reported explicitly; it is not repaired by an observer-local alias.

## 7. Implementation sequence

1. Implement origin-marker reading and atomic local initialization, including shell/mounted-access identity semantics.
2. Extend peer probing with origin identity and federation-protocol capability information.
3. Separate session origin, display attribution, and access routes in the model and wire representation.
4. Implement federation request context, caller-side cycle checks, receiving-side guard, and hop-limit semantics.
5. Introduce the shared identity-aware merge and use it for both CLI and TUI discovery.
6. Migrate transcript opening, resume, and affected identity consumers to origin-qualified references and explicit routes.
7. Verify the scenarios below; update user documentation and release notes as appropriate, and remove temporary reproduction scaffolding.

## 8. Acceptance criteria

### Origin stability

- Local listing on B, shell access from A to B, and shell access from C to B yield the same origin ID.
- Concurrent initialization does not replace an existing marker or expose different identities.
- Restarts retain identity.
- A missing marker in read-only remote access produces an explicit identity diagnostic, not an invented canonical ID.

### Attribution, deduplication, and routing

- B reached directly and through A appears once, attributed to B.
- Different labels or SSH aliases for B still deduplicate.
- Identical session IDs on distinct origins remain separate.
- Identical session IDs from distinct sources remain separate.
- B known only through A remains visible; transcript opening and resume target B through A.
- A failed discovery route does not suppress a working independent route.
- CLI and TUI produce the same merged identities and counts.

### Cycle prevention

- Self-reference `A -> A` issues no recursive session query to A.
- Two-node cycle `A -> B -> A` is stopped by B before it queries A.
- Longer cycle `A -> B -> C -> A` is stopped before querying the ancestor.
- Back-edge to the original requester is skipped.
- Aliases resolving to the same origin ID do not bypass the guard.
- The receiving-side guard stops a repeated-origin request if the caller-side guard is bypassed.
- Diamond discovery permits both branches and merges their shared-origin results.
- At the hop limit, endpoint local sessions remain visible and truncation is reported.
- An older peer never receives an unguarded recursive listing invocation as a compatibility fallback.

## Implemented protocol and verification

- Origin initialization: `llm-sessions origin --init`, atomic create-only
  UUID v4 marker; normal writable local commands also initialize it.
- Protocol v1: `federation capabilities`; guarded `federation list --path
  <JSON origin array> --hops <remaining count>` returns sessions and failures.
- Delegated reads: `federation show --origin <UUID> --source <agent> --id
  <session ID> --route <JSON hop array>`; `federation top` is local-only.
- Routes: ordered origin/name/SSH-or-HTTP/home hops, ranked by hop count then
  lexical order. Merge retains every successful route and selects one whole
  observation. HTTP routes are read-only and carry no credentials.
- Discovery depth: eight SSH/HTTP hops by default; caller-side identity check
  plus receiving-side guard. Older SSH peers remain shell leaves.
- `federation top` is local-only; `llm-sessions top --json` remote rows carry
  both the configured display name and the peer's `origin_id`.
- Configured home: the marker and guarded binary commands share the supplied
  HOME/XDG namespace, including delegated reads and nested resume commands.
- Verification: full `cargo test` passes, including ten real-CLI federation
  scenarios. An isolated CLI/TUI smoke graph exercised duplicate direct and
  gateway routes, a cyclic edge, origin attribution, matching single-session
  counts, and remote transcript rendering with and without a direct route.

## HTTP peers and opt-in gateways

- HTTP peers are configured with `url` and optional `token_env` instead of
  `ssh`; each observer or gateway binds credentials locally by configured URL.
  Forwarded route metadata cannot select environment secrets.
- Authenticated `/v1/federation/capabilities` and `/v1/federation/list`
  implement the origin handshake and request-local path/hop protocol.
  Protocol version 2 permits HTTP hops; version-1 SSH peers are downgraded
  to shell leaves and never receive mixed routes or chained discovery.
- `serve` remains local-only by default. `serve --federate` publishes its
  merged catalog through HTTP and WebSocket; its token authorizes that catalog.
- HTTP detail/transcript reads accept `origin` and `source` qualifiers.
  A fully qualified snapshot miss runs one fresh policy-filtered discovery;
  its errors propagate. Publication source/query restrictions apply to every
  read and discovery response. Gateway caches key on the full identity.
- Resume selects the shortest valid SSH-only route independently of the
  read-preferred route; HTTP-only sessions offer none.
- HTTP peer reads refuse redirects, verify HTTPS certificates against web and
  platform roots, bound response bodies, and redact bearer values before
  truncating diagnostics.
- Real CLI/server regressions cover leaf/gateway scope, SSH-to-HTTP and
  HTTP-to-SSH reads, multi-node mixed-transport cycles, diamond
  deduplication, origin collisions, credential rebinding, authorization and
  publication policy, missing identity, depth, configuration failures,
  immediate reads after discovery, legacy v1 downgrade, resume over an SSH
  alternate, and HTTPS through a path-prefixed TLS proxy (untrusted CA
  rejected). Standalone smoke also exercised a live gateway WebSocket
  snapshot containing both origins without credential metadata.

