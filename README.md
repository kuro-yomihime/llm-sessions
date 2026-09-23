# llm-sessions

> Workspace-first explorer for local LLM coding-agent sessions.

Every coding agent buries its history in its own format. `llm-sessions`
reads them all and answers one question: **what happened in _this project_?**

```
$ llm-sessions workspaces
 102  ~/code/web-store         claude omp opencode
  43  ~/code/pricing-service   codex omp opencode
 101  ~/code/llm-usage-monitor codex opencode
```

Run it with no arguments to browse workspaces → sessions → transcripts,
every agent merged into one list. The layout follows the terminal: one pane
under 100 columns, two under 140, three beyond.

## Install

```sh
cargo install llm-sessions   # or `cargo install --path .` from a checkout
```

Requires Rust 1.96 or newer — the toolchain CI pins in
`rust-toolchain.toml`; older compilers are refused up front. Linux only,
deliberately: liveness reads `/proc`, and nothing else is tested.
`alias llms=llm-sessions` if you want it shorter.

## TUI basics

- Arrows or `j` `k` move, `Enter` opens, `Esc` goes back, `Tab` switches panes.
- `/` filters by title, path, or agent; `s` full-text search (needs
  `llm-sessions index`); `x` clears.
- `Ctrl-W o` fullscreen reader; `?` lists every key.

## Sources

| Agent | Store | Archive |
|---|---|---|
| OMP | `~/.omp/agent/sessions/<bucket>/*.jsonl` | overlay |
| Claude Code | `~/.claude/projects/<dir>/*.jsonl` | overlay |
| Codex | `~/.codex/sessions/**/rollout-*.jsonl` + `state_5.sqlite` | native |
| OpenCode (v1 and v2) | `~/.local/share/opencode/opencode.db` (`session` and `session_v2` tables) | native |
| Copilot CLI | `~/.copilot/session-store.db` + `session-state/` | overlay |
| Kilo | `~/.local/share/kilo/kilo.db` | native |
| Goose | `~/.local/share/goose/sessions/*.jsonl` + `projects.json` | overlay |
| Cursor CLI | `~/.cursor/projects/*/agent-transcripts/**/*.jsonl` | overlay |
| Gemini CLI | `~/.gemini/tmp/<hash>/chats/*.json` | overlay |
| Hermes | `~/.hermes/state.db` | native |
| OpenClaw | `~/.openclaw/agents/*/sessions/*.jsonl` + `sessions.json` | overlay |

Missing agents are skipped silently; a failing source degrades to a
status-bar warning, never an empty list. Not supported: Cursor **GUI**
stores, Antigravity, Devin.

## Remote hosts

Configure in `$XDG_CONFIG_HOME/llm-sessions/hosts.json`:

```json
{ "hosts": [
  { "name": "nx-1", "ssh": "nx-1.example.net", "home": "/var/lib/hermes", "sources": ["hermes"] },
  { "name": "nx-2", "ssh": "dev@nx-2.example.net" }
] }
```

Remote sessions appear beside local ones (`--host` filter, `host:` in
queries, host view `5`). With `llm-sessions` installed on the host, its
JSON output is used — full fidelity, liveness included; otherwise a
generated `sh` script lists stores over a single SSH connection (`mode`:
`"auto"`, `"llm-sessions"`, or `"shell"`). No store ever crosses the wire.
Connections use `BatchMode=yes`, so passphrase-protected hosts fail fast
instead of hanging.

```sh
llm-sessions hosts                                            # list + probe every host
llm-sessions hosts add nx-1 nx-1.example.net --source hermes  # repeatable --source
llm-sessions hosts add nx-2 dev@nx-2.example.net --home /var/lib/hermes --mode shell
llm-sessions hosts remove nx-1
```

## Commands

```sh
llm-sessions                     # interactive explorer
llm-sessions list                # newest 50 sessions
llm-sessions list --workspace ~/code/thing --source omp --archived --live
llm-sessions workspaces          # project totals, live first
llm-sessions show <id>           # transcript; --thinking --tools for folded details
llm-sessions show <id> --format md -o out.md   # Markdown or HTML report
llm-sessions index               # build/update the optional text index
llm-sessions search <words>      # full-text search
llm-sessions digest --since 24h  # recent activity by project
llm-sessions top                 # one row per running agent
llm-sessions usage --by workspace --since 7d   # also: model, agent, day
llm-sessions archive <id>        # --undo restores
llm-sessions sources             # detected stores and scan costs
llm-sessions doctor              # diagnostics; never applies fixes
llm-sessions index --stats | --vacuum | --max-body N --max-bytes N
llm-sessions completions bash
llm-sessions man
```

Scripting: `list --json --limit 0` (0 = unlimited), `list -q '<query>'`,
`list --format '{id}' -0` (NUL-separated ids). `--read-only` refuses
archiving, indexing, tmux focus changes, and saving bookmarks.

## Keys

| Key | Action |
|---|---|
| `j` `k` `↓` `↑`, `PgDn` `PgUp`, `g` `G` | move, page, top/bottom |
| `Enter` | open next pane; on a transcript link, read the subagent |
| `→` `l` / `←` `h` `Esc` | focus pane right / left |
| `]` `[` / `1`…`6` | cycle / jump to views: workspace, agent, branch, fleet, host, timeline |
| `/` `s` `x` `Ctrl-R` | filter / full-text search / clear / rescan |
| `f` | follow: tail the session live |
| `J` | jump tmux focus to the selected live session |
| `z a` `z c` `z o` / `z M` `z R` | fold one session's descendants / all folds |
| `a` `e` | toggle archived / only live sessions |
| `Space` `v` `V` / `d` `u` | select one / all visible / group; archive / restore (asks first) |
| `t` `o` `c` | unfold thinking / tool output / changed files |
| `Ctrl-W o` `F11` | fullscreen reader (`Ctrl-W ←` `→` resize, `=` reset) |
| `r` | quit, printing the resume command |
| `m` `'` `` ` `` | set / jump to bookmarks |
| `?` `F1` | help |
| `q` / `Q` `Ctrl-C` | back / quit |

`r` prints a shell-quoted resume command to stdout — inspect it, then paste
it into your shell. Don't run it via `$(llm-sessions)`: command substitution
doesn't interpret the quoting.

## Liveness

| | Meaning |
|---|---|
| `●` | **exact** — a process holds the session file open (`/proc/<pid>/fd`) |
| `○` | **inferred** — right agent, cwd rolls up to the workspace; pid shown with `?` when ambiguous |
| `·` | written in the last few minutes, nothing attached |
| ` ` | stale |

Live rows also show the tmux pane and working/waiting state (from CPU-time
deltas). `J` moves tmux focus there; `Enter` opens the transcript instead.
Every check degrades gracefully without `/proc` or tmux. A process holding
a subagent transcript open marks its parent live too.

## Query language

One grammar across `/`, `s`, and `list -q`:

```
source:omp  agent:codex  ws:dstm  branch:feat/x  model:glm
id:01a0  after:7d  before:today  cost:>1
live:yes  archived:only  body:naming  bare words = free text
```

`body:` needs `llm-sessions index`; `archived:` overrides the `a` toggle.
`cost:` compares within a nano-dollar, so a summed float like
`0.30000000000000004` matches `cost:0.3`. Unknown keys are treated as free
text — a typo narrows results, never fails.

## Design notes

- **No index for browsing.** A full cross-agent scan of ~2,000 sessions
  takes ~80 ms, so the TUI simply rescans every 2 s. The FTS5 index exists
  only for body search and spend caching.
- **Workspaces roll up.** cwds canonicalize to their repository; linked
  worktrees map to the main one. Ranking is liveness first, then recency
  with a 7-day half-life — raw counts would let `/tmp` win.
- **Derived names are honest.** Missing or duplicated titles get a label
  from the first user prompt, rendered in italic so it can't pass for an
  agent-written title.
- **Subagents and forks are trees.** Children nest under their parent;
  `Enter` on a delegating tool call follows into the child's transcript.
- **Changed files are answerable.** The transcript opens with a foldable
  changed-file list; the status line shows `N files +A −D`.
- **Times are shown on your clock.** Stores record UTC and `--json` still
  emits it; every timestamp a person reads is converted to the machine's
  offset. Today's sessions show a time, this week's a weekday, older ones
  a date.
- **Paths are never decoded.** They come from recorded fields (`cwd`,
  `session.directory`, …); where none exists, the decoded guess is
  marked `~`.
- **Unknown records survive.** Unrecognized record types become folded
  raw-JSON blocks; a session never fails to open.
- **Agent stores are read-only.** The only writes are archiving (one
  `UPDATE` where the agent models it natively, an overlay db otherwise)
  and `llm-sessions`'s own databases. Credential files are never opened.

### Source layout

- `src/catalog/` — scan, query grammar and visibility policy, labels,
  workspace identity.
- `src/source/` — agent adapters, shared schema decoding, and `remote/`
  (`transport.rs` runs SSH, `parse.rs` decodes listings).
- `src/storage/` — index (`UsageCache` is the one tier-1-vs-cached usage
  rule), archive overlay, bookmarks, SQLite access.
- `src/runtime/` — process facts (`procfs`), liveness, tmux, resume
  commands, diagnostics.
- `src/output/` — `reader/` (`wrap.rs` wrapping, `table.rs` tables),
  exports, list rows, digests, usage reports.
- `src/tui/` — `app/` (`mod.rs` state and dispatch, `effects.rs` side
  effects), `view.rs` grouping, `ui.rs` layout and drawing, `keymap.rs`,
  `session.rs` terminal loop.
- `src/cli/` — argument definitions plus one file per command group.

The root holds the library facade, binary entry point, shared model,
errors, and clock. Frontends share catalog filtering; output rendering
doesn't depend on TUI state. Layout measures the same transcript rectangle
that drawing uses.

Development: the toolchain is pinned (1.96.0); CI runs fmt, pedantic
clippy, rustdoc with `-D warnings`, tests, and a non-blocking
`cargo audit`.

## Storage `llm-sessions` owns

| Path | What |
|---|---|
| `$XDG_STATE_HOME/llm-sessions/overlay.db` | archive state for agents without it |
| `$XDG_CACHE_HOME/llm-sessions/index.db` | FTS5 body index, spend cache; delete any time |

Both are disposable. The index is large (~690 MB per ~5 GB of bodies) and
only written by `llm-sessions index`; `--since`/`--max-bytes` bound what it
keeps, `--max-body` truncates oversized tool output, `--vacuum` reclaims
pages.

## License

MIT
