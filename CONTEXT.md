# CONTEXT.md — domain glossary

Terms used by the code and by architecture reviews. Keep names stable; add a
line here when a module is named after a concept not yet listed.

- **Session** — one conversation an agent ran, normalized from any store into a
  `SessionSummary`. Identified by its origin, source, and agent-assigned ID;
  grouped by the workspace it ran in.
- **Origin** — the host/account session namespace where a session lives.
  Its stable identity is shared by every observer and access method.
- **Origin display label** — human-readable attribution for an origin;
  different observers may use different labels for the same origin.
- **Access route** — an ordered path through remote hosts to a session's
  origin. Multiple routes can reach the same session.
- **Discovery path** — origins already visited by one traversal branch.
  A repeated origin is a cycle; a separate branch can visit it independently.
- **Transcript** — the opened body of a session: turns of blocks (text,
  thinking, tool use/result, changed files, unknown raw JSON).
- **Workspace** — the repository a session's recorded cwd canonicalizes to;
  linked worktrees roll up to the main one.
- **Source** — an agent store (omp, claude, codex, …). Each is read through a
  source adapter implementing the `Source` interface: `list` (tier-1, no body
  parsing) and `open` (tier-2, full transcript).
- **Registry** — the source identifiers, spellings and display order in
  `SourceId::ALL`. `source::all` follows that order with an exhaustive adapter
  constructor match; adding an identifier requires its constructor too.
- **Reader** (`output::reader`) — the pure transcript rendering engine: `render` (transcript +
  fold flags + offset in, laid-out `RenderedLine`s out) and `fit` (lines +
  width in, wrapped lines out). Owns `LineKind`, `Rail`, `RenderedLine`,
  tables, wrapping. Reads no UI or App state.
- **Measure** — the width/viewport numbers the reader wraps against.
  `tui::ui::panes` computes the body rectangle; App applies its dimensions
  before immutable drawing uses that same rectangle.
- **Record helpers** — shared unknown-record preservation and content
  flattening in `source`. Role mapping and block pairing stay in the adapters
  because their formats differ.
- **Process facts** (`runtime::procfs`) — identity, state, parent and open
  descriptors of a pid, read from an injectable `/proc` root. An unreadable
  inventory is not an empty one. Fleet liveness and doctor consume these facts.
- **Report rows** (`output::report`) — a `SessionSummary` as text a person
  reads: the `list` line and the `{field}` template, shared by every
  printing subcommand.
- **Loader** (`catalog::loader`) — the inputs of a scan (local adapters,
  archive overlay, configured hosts) assembled once, plus the lookups every
  frontend runs on its result: opening a listed session with tier-1 title and
  workspace, id/prefix resolution, `body:` hits. The CLI scans once; `serve`
  keeps one and rescans on a timer.
- **Fleet / liveness** — which sessions are live right now: exact (fd held
  open), inferred (right agent + cwd), or recent-write.
- **Status** (`runtime::status`) — the derived five-state view of a session
  (running, idle, waiting for approval or input, failed, done): liveness and
  CPU activity joined with the transcript tail, debounced by `settle`. Needs
  a tier-2 read, so it is derived for watched sessions, never for a listing.
- **Serve** (`serve`, feature `serve`) — the read-only HTTP/WebSocket API:
  a poller owns a `Loader`, rescans on a timer and publishes immutable
  snapshots; handlers only read them. GET routes and one WebSocket, nothing
  else, so read-only holds by construction rather than by a flag.
- **Overlay** — `llm-sessions`-owned store of archive state for agents without
  native archiving.
- **Index** — the disposable FTS5 body-search cache plus derived usage/prompt
  cache. Never required for browsing. `storage::index::UsageCache` is the one
  statement of "tier-1 usage, else the cached body-derived usage".
- **Prompt mode** — one of the modal input states (filter, search, bookmark,
  fold, resize, confirm, insert). Key meanings live in `tui::keymap`'s single
  dispatch; state transitions live in App.
- **Insert mode** (`runtime::insert`) — one typed message handed to the
  session's agent through its CLI, run off the UI thread. One writer per
  conversation: a session with a process attached (exact or inferred) goes
  through `Source::live_insert_command` or is refused; only an unattended one
  is resumed with `Source::insert_command`. The agent writes the turn;
  `llm-sessions` only reports the exit.
- **Query** (`catalog::query`) — the filter grammar and visibility policy shared
  by `/`, `s`, and `list -q`, independent of TUI grouping and navigation.
