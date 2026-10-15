# llm-sessions

> Workspace-first explorer for local LLM coding-agent sessions.

Reads every coding agent's history and answers one question: **what happened in _this project_?**

```
$ llm-sessions workspaces
 102  ~/code/web-store         claude omp opencode
  43  ~/code/pricing-service   codex omp opencode
 101  ~/code/llm-usage-monitor codex opencode
```

Run with no arguments to browse workspaces → sessions → transcripts, all
agents merged into one list.

## Screenshots

![Workspace view: workspaces, sessions with a live subagent tree, transcript](docs/assets/overview.png)

<details>
<summary>More screenshots</summary>

| Timeline view `6`, insert mode `i` | Full-text search `s` |
|---|---|
| ![Timeline of every agent's sessions with an insert-mode prompt](docs/assets/timeline-insert.png) | ![Sessions narrowed to body matches for "pricing"](docs/assets/search.png) |
| **Fullscreen reader `Ctrl-W o`** | **Help `?`** |
| ![Fullscreen transcript reader](docs/assets/reader.png) | ![Help overlay listing every key](docs/assets/help.png) |

</details>


## Install

```sh
cargo install llm-sessions   # or `cargo install --path .` from a checkout
```

Requires Rust 1.96+ (pinned in `rust-toolchain.toml`). Linux only: liveness
reads `/proc`. `--no-default-features` builds an SSH-only CLI/TUI without the
server or HTTP client. Enable `serve` or `http` independently with `--features`;
both are enabled by default.

## TUI basics

- Arrows or `j` `k` move, `Enter` opens, `Esc` goes back, `Tab` switches panes.
- `/` filters, `s` full-text search (needs `llm-sessions index`), `x` clears.
- `i` insert mode: send one message to the selected session's agent via its
  headless CLI.
- `?` lists every key.

## Sources

| Agent | Store | Archive |
|---|---|---|
| OMP | `~/.omp/agent/sessions/<bucket>/*.jsonl` | overlay |
| Claude Code | `~/.claude/projects/<dir>/*.jsonl` | overlay |
| Codex | `~/.codex/sessions/**/rollout-*.jsonl` + `state_5.sqlite` | native |
| OpenCode (v1, v2) | `~/.local/share/opencode/opencode.db` | native |
| Copilot CLI | `~/.copilot/session-store.db` + `session-state/` | overlay |
| Kilo | `~/.local/share/kilo/kilo.db` | native |
| Goose | `~/.local/share/goose/sessions/*.jsonl` + `projects.json` | overlay |
| Cursor CLI | `~/.cursor/projects/*/agent-transcripts/**/*.jsonl` | overlay |
| Gemini CLI | `~/.gemini/tmp/<hash>/chats/*.json` | overlay |
| Hermes | `~/.hermes/state.db` | native |
| OpenClaw | `~/.openclaw/agents/*/sessions/*.jsonl` + `sessions.json` | overlay |

Missing agents are skipped; a failing source degrades to a status-bar
warning. Not supported: Cursor GUI stores, Antigravity, Devin.

## Remote hosts

Configure in `$XDG_CONFIG_HOME/llm-sessions/hosts.json`:

```json
{ "hosts": [
  { "name": "nx-1", "ssh": "nx-1.example.net", "home": "/var/lib/hermes", "sources": ["hermes"] },
  { "name": "http-worker", "url": "https://worker.example.net", "token_env": "WORKER_TOKEN", "sources": ["omp"] }
] }
```

Remote sessions appear beside local ones (`--host` filter, `host:` in
queries, host view `5`). A federation-capable `llm-sessions` on a peer also
discovers that peer's configured hosts. Otherwise a generated `sh` script
lists only its local stores (`mode`: `"auto"`, `"llm-sessions"`, or `"shell"`).
Older binaries are shell-only leaves, with a diagnostic; they never receive
an unguarded recursive `list`. Connections use `BatchMode=yes`, so
passphrase-protected hosts fail fast.
`--host local` and `host:local` select genuinely local access, not forwarded
sessions that happen to have no display label.

```sh
llm-sessions hosts                                            # list + probe
llm-sessions hosts add nx-1 nx-1.example.net --source hermes  # repeatable --source
llm-sessions hosts refresh nx-1                             # enroll/reset UUID
llm-sessions hosts remove nx-1
```

HTTP peers use `url` instead of `ssh`; `token_env` names a local environment
variable holding the peer's bearer token:

```sh
export WORKER_TOKEN='<server token>'
llm-sessions hosts add http-worker --url https://worker.example.net --token-env WORKER_TOKEN
llm-sessions hosts refresh http-worker
```

HTTP peers support discovery and transcript reads, not terminal resume,
insert delivery, or process `top`. Mixed SSH/HTTP routes use the same origin
identity and merge rules. HTTP clients verify HTTPS certificates against both
bundled web roots and the platform trust store (`SSL_CERT_FILE` /
`SSL_CERT_DIR` work for a private CA), refuse redirects, and use finite
connection/request timeouts. Base URLs may include a reverse-proxy path
prefix, but not credentials, queries, or fragments.
Token values and credential references are never forwarded in session routes;
each gateway resolves its own peer credentials from its local configuration.

The federation protocol is version 2: it allows HTTP hops in routes. A
version-1 peer (an earlier SSH-only release) is downgraded to a shell leaf
with a diagnostic: its local sessions still list, but it is never asked to
discover further or to forward a route. Upgrade such peers for chained
discovery.

Initialize each originating account once, on that machine:

```sh
llm-sessions origin --init       # create-only; prints the persisted UUID
llm-sessions origin              # read the existing identity
```

Normal local commands initialize it automatically unless `--read-only` is
set. Remote requests never initialize it. The marker lives at
`$XDG_STATE_HOME/llm-sessions/origin-id` (default
`~/.local/state/llm-sessions/origin-id`), resolved the same way locally, by
the SSH probe, and by guarded commands on the peer. Shell-only hosts must
provision a UUID there beforehand; missing or malformed identity produces a
diagnostic rather than an observer-specific substitute. A configured `home`
selects that namespace for the marker, stores, and guarded commands,
including its HOME/XDG environment.

Sessions are deduplicated by `(origin_id, source, id)` before counts or
filters. The agent-assigned `id` stays unchanged. A session reached directly
and through a gateway appears once under its origin, not the gateway. JSON
includes `origin_id` and `routes`; display names are not identity keys.
The shortest successful route wins, with a lexical tie-break. Transcript
opening also works when an origin is reachable only through a gateway.
Resume uses the shortest valid SSH-only route, independently of the
preferred read route, so adding an HTTP path never removes a working SSH
resume; an HTTP-only session has none. Disambiguate colliding IDs when opening:

```sh
llm-sessions show shared --origin <origin-uuid> --source omp
```

Discovery skips an already-visited origin before querying its sessions.
Paths are branch-local: independent routes may query the same origin and
their results merge. Eight SSH/HTTP hops is both the default budget and the
maximum. At zero remaining hops, endpoint local sessions remain visible;
identity-only probes distinguish already-visited peers from unexplored ones.
Truncation is reported only when an unexplored peer is cut off, not for cyclic
edges. Probe failures remain visible in the discovery failures.

Preserve the marker across upgrades and restores of the same instance.
Independent clones must get a new UUID; copied identities cause false
deduplication and false cycle detection.

Unpinned or UUID-mismatched peers are excluded with a warning. Enroll or reset:

```sh
llm-sessions hosts refresh worker
```

No prompt: saves the peer's current UUID and prints old → new; failed reads
leave the pin unchanged. Verify the destination first—this explicitly resets
trust. Forwarded origins remain trusted gateway claims.

### Serve

`llm-sessions serve` publishes this machine's sessions to clients that hold a
token and have no shell on it:

```sh
LLMS_TOKEN=$(openssl rand -hex 32) llm-sessions serve --bind 0.0.0.0:8787
curl -H "Authorization: Bearer $LLMS_TOKEN" 'http://host:8787/v1/sessions?q=live:yes'
```

| Route (all `GET`) | Returns |
|---|---|
| `/v1/healthz` | `{"ok","version","host"}`, no token |
| `/v1/sessions?q=<query>&limit=N` | `{"serverTime","sessions":[row…]}`: `list --json` rows, same query language, `limit` 50 by default, 0 for all |
| `/v1/sessions/<id>` | one row; a unique id prefix works, as with `show` |
| `/v1/sessions/<id>/transcript` | exactly what `show <id> --json` prints |
| `/v1/federation/capabilities` | authenticated protocol handshake: `{"version":2,"origin_id":"<UUID>"}` |
| `/v1/federation/list?path=<JSON array>&hops=N` | request-local guarded discovery: `{"sessions":[row…],"failures":[…]}` |
| `/v1/stream` | WebSocket: `{"type":"snapshot",…}` on connect and when a watched row changes, `{"type":"ping"}` every 20 s |

Detail and transcript routes accept `?origin=<UUID>&source=<agent>` to
resolve IDs shared by different origins or agents. Ambiguous unqualified
IDs return HTTP 400 rather than selecting one. A fully qualified read the
poll snapshot has not seen yet triggers one fresh, policy-filtered discovery,
so a row returned by `/v1/federation/list` is readable immediately; hidden
rows stay 404 and discovery failures are returned, not masked as 404.
Federation capabilities/list require the same bearer authentication as
sessions; missing canonical origin identity returns HTTP 503. Provision the
marker before read-only startup.

A row is a `list --json` row plus `status`: `{"status","waitingReason",
"statusSince"}` with status `running`, `idle`, `waiting` (`approval` or
`input`), `failed` or `done`, derived from liveness and the transcript tail
and debounced over two polls. Only watched sessions carry one in listings and
the stream: live, or written within `--status-window` hours (default 1).
`/v1/sessions/<id>` derives it for any session. Status is a heuristic; an
agent waiting on a tool it is running reads `waiting/approval`.

Read-only by construction: there are only `GET` routes and the WebSocket, so
nothing archives, indexes, edits hosts or reaches a terminal, and no resume
command is sent. Clients authenticate with `Authorization: Bearer <token>`;
`/v1/stream` also takes `?token=` for clients that cannot set headers. The
default bind is `127.0.0.1:8787`; every bind needs `--token` (or `LLMS_TOKEN`)
of at least 16 characters — use high entropy, `openssl rand -hex 32`. A
browser page can reach a loopback server through DNS rebinding and unguarded
WebSocket connects, so `--no-auth` (loopback only) answers just requests
whose `Host` header names the loopback. There is no TLS: off loopback, put a
TLS reverse proxy in front. `--source` and `-q` restrict
what is published at all; `--poll-secs` (default 3) sets the rescan interval.

A web page on another origin needs `--cors-origin <scheme://host[:port]>`
(repeatable, exact match, no `*`). A listed origin's preflight is answered
without the token; every request after it still needs the bearer header, so
the page holds the token. `--cors-origin` is refused with `--no-auth`.

Local-only is the default, even when `hosts.json` contains peers. Enable a
federated gateway explicitly:

```sh
llm-sessions serve --federate --token "$GATEWAY_TOKEN"
```

Gateway mode publishes the merged local/remote catalog through the regular
HTTP and WebSocket routes. Its token grants access to that catalog, including
sessions reached through other hosts. `--source` and the base `-q` query
restrict every published session, including qualified transcript reads and
federation discovery. `body:` is local-index-only and excludes remote rows.
Each federation request carries its own visited-origin path and hop budget;
it does not reuse the gateway's cached full catalog. Status and transcript
caches use origin-qualified identity, and local process activity is never
attributed to a remote process with the same PID.

## Commands

```sh
llm-sessions                     # interactive explorer
llm-sessions list                # newest 50 sessions (--json, -q '<query>', --format, -0)
llm-sessions workspaces          # project totals, live first
llm-sessions show <id>           # transcript; --thinking --tools, --format md -o out.md
llm-sessions index               # build/update the optional text index
llm-sessions search <words>      # full-text search
llm-sessions digest --since 24h  # recent activity by project
llm-sessions top                 # one row per running agent
llm-sessions serve --token <t>   # read-only HTTP/WebSocket API (see below)
llm-sessions usage --by workspace --since 7d   # also: model, agent, day
llm-sessions archive <id>        # --undo restores
llm-sessions sources             # detected stores and scan costs
llm-sessions doctor              # diagnostics; never applies fixes (--json)
llm-sessions completions bash
llm-sessions man
```

`--read-only` refuses archiving, indexing, origin initialization, host-config
edits, tmux focus changes, and saving bookmarks.

## Keys

| Key | Action |
|---|---|
| `j` `k` `↓` `↑`, `PgDn` `PgUp`, `g` `G` | move, page, top/bottom |
| `Enter` | open next pane; on a transcript link, read the subagent |
| `→` `l` / `←` `h` `Esc` | focus pane right / left |
| `]` `[` / `1`…`6` | cycle / jump to views: workspace, agent, branch, fleet, host, timeline |
| `/` `s` `x` `Ctrl-R` | filter / search / clear / rescan |
| `f` | follow: tail the session live |
| `J` | jump tmux focus to the selected live session |
| `z a` `z c` `z o` / `z M` `z R` | fold one session's descendants / all folds |
| `a` `e` | toggle archived / only live sessions |
| `Space` `v` `V` / `d` `u` | select one / all visible / group; archive / restore (asks first) |
| `t` `o` `c` | unfold thinking / tool output / changed files |
| `y y` / `y A` / `y r` / `y i` / `y p` | yank the assistant reply under the reader's cursor (the latest one from the lists) / the whole transcript (as folded on screen) / the resume command / the session id / its transcript file or database path to the clipboard |
| `Ctrl-W o` `F11` | fullscreen reader (`Ctrl-W ←` `→` resize, `=` reset) |
| `r` | quit, printing the resume command |
| `i` | insert: send one message to the selected session |
| `m` `'` `` ` `` | set / jump to bookmarks |
| `?` `F1` | help |
| `q` / `Q` `Ctrl-C` | back / quit |

`r` prints a shell-quoted resume command — paste it into your shell; don't
run it via `$(llm-sessions)`, which won't interpret the quoting.

Clipboard copying requires an installed helper: `wl-copy` (Wayland),
`xclip` or `xsel` (X11), `pbcopy` (macOS), or `clip.exe` (WSL).
Wayland/X11 helpers also require access to the display (`WAYLAND_DISPLAY` /
`DISPLAY`); headless or SSH sessions may have no usable clipboard.
Copying runs in the background; progress and failures appear in the status bar.

`i` runs the agent's one-shot command in the session's directory off the UI
thread (killed after 30 min; the message is one argv element, never shell
text). Each conversation keeps one writer:

- **OMP, Hermes** resume only an idle session
  (`omp --print --resume <id> -- <msg>`,
  `hermes chat --quiet --resume <id> --query=<msg>`); a session with any
  process attached is refused — use `J` to jump to its pane.
- **OpenCode** with a live client sends to the one loopback server that
  client is connected to (read from its sockets in `/proc`); no server or
  several is refused. An idle session gets plain `opencode run --session <id>`.

Subagents, remote rows, and `--read-only` are refused too.

## Liveness

| | Meaning |
|---|---|
| `●` | **exact** — a process holds the session file open (`/proc/<pid>/fd`) |
| `○` | **inferred** — right agent, cwd rolls up to the workspace |
| `·` | written recently, nothing attached |
| ` ` | stale |

Live rows also show the tmux pane and working/waiting state. `J` moves tmux
focus there; `Enter` opens the transcript. A process holding a subagent
transcript open marks its parent live too.

## Query language

One grammar across `/`, `s`, and `list -q`:

```
source:omp  agent:codex  ws:dstm  branch:feat/x  model:glm
id:01a0  after:7d  before:today  cost:>1
live:yes  archived:only  body:naming  bare words = free text
```

`body:` needs the index; `archived:` overrides the `a` toggle. Unknown keys
are treated as free text — a typo narrows results, never fails.

## Design notes

- **No index for browsing.** A full cross-agent scan takes ~80 ms, so the
  TUI rescans every 2 s. The FTS5 index exists only for body search and
  spend caching.
- **Workspaces roll up.** cwds canonicalize to their repository; linked
  worktrees map to the main one. Ranking is liveness first, then recency
  with a 7-day half-life.
- **Derived names are honest.** Missing or duplicated titles get a label
  from the first user prompt, rendered in italic.
- **Subagents and forks are trees.** Children nest under their parent;
  `Enter` on a delegating tool call follows into the child's transcript.
- **Times are shown on your clock.** Stores record UTC (`--json` emits it);
  displayed timestamps use the machine's offset.
- **Paths are never decoded.** They come from recorded fields; where none
  exists, the decoded guess is marked `~`.
- **Unknown records survive.** Unrecognized record types become folded
  raw-JSON blocks; a session never fails to open.
- **Agent stores are read-only.** The only writes are archiving (native
  where the agent models it, an overlay db otherwise) and `llm-sessions`'s
  own databases. Credential files are never opened.

### Source layout

- `src/catalog/` — scan assembly, origin-qualified merge, query grammar, labels, workspace identity.
- `src/source/` — agent adapters, schema decoding, `remote/` SSH and HTTP transports.
- `src/storage/` — index, archive overlay, bookmarks, SQLite access.
- `src/runtime/` — process facts, liveness, tmux, resume, diagnostics.
- `src/output/` — reader, exports, list rows, digests, usage reports.
- `src/tui/` — `app/` state and effects, `view.rs`, `ui.rs`, `keymap.rs`.
- `src/cli/` — argument definitions, one file per command group.
- `src/federation.rs` — origin identity, protocol capabilities, guarded discovery context.
- `src/serve/` — authenticated HTTP/WebSocket API, local snapshots and opt-in federation gateway.

Development: toolchain pinned (1.96.0); CI runs fmt, pedantic clippy,
rustdoc with `-D warnings`, tests, and a non-blocking `cargo audit`.

## Storage `llm-sessions` owns

| Path | What |
|---|---|
| `$XDG_STATE_HOME/llm-sessions/overlay.db` | archive state for agents without it |
| `$XDG_CACHE_HOME/llm-sessions/index.db` | FTS5 body index, spend cache; delete any time |
| `$XDG_STATE_HOME/llm-sessions/origin-id` | persistent host/account federation UUID |

The databases are disposable; the origin marker is not. The index is large
(~690 MB per ~5 GB of bodies) and only written by `llm-sessions index`;
`--since`/`--max-bytes` bound it, `--max-body` truncates oversized tool output,
`--vacuum` reclaims pages.

## License

MIT
