# Golden corpus

Nine adapters parse nine private formats that shift without notice — Codex
already ships legacy and modern rollouts side by side. Handwritten fixtures
only encode what the author *believed* a format to be; these encode what the
agents actually wrote.

## Provenance

Every file here was derived from a real transcript on the author's machine and
then redacted:

- **Structure is byte-faithful.** Field names, nesting, types, and the
  running-total shape of Codex `token_count` and Claude `cost-state` are
  preserved exactly — a redaction that changes shape would test nothing.
- **Payloads are replaced.** Prose becomes a deterministic word sequence,
  absolute paths become `/workspace/...`, UUIDs and session/project/message
  identifiers become fixed placeholders. OpenCode timestamps are shifted by a
  constant offset to retain ordering and migration ties without real dates;
  its provider id is a placeholder.
- **No secrets.** The corpus is grepped for `sk-`, `Bearer `, `ghp_`,
  `password`, JWT prefixes, and long base64 runs before being committed.

## Layout

`home/` mirrors a real home directory, so each adapter can be pointed at it
with `Source::new(home)` exactly as it would be in production:

| Path | Agent |
|---|---|
| `home/.omp/agent/sessions/-workspace/…` | OMP, plus a subagent child and a `draft.txt` that must be ignored |
| `home/.claude/projects/-workspace/…` | Claude Code |
| `home/.codex/sessions/2026/01/01/…` | Codex, modern rollout |
| `home/.codex/sessions/rollout-2025-…` | Codex, legacy flat rollout |
| `home/.cursor/projects/…/agent-transcripts/…` | Cursor CLI, with `repo.json` |
| `home/.local/share/goose/…` | Goose, with `projects.json` |
| `home/.gemini/tmp/goldenhash/logs.json` | Gemini CLI |

The three SQLite-backed agents ship as `.sql` text rather than binary
databases, so a diff shows what changed: `opencode.sql`, `kilo.sql` (the
older schema, genuinely missing columns OpenCode has), and `copilot.sql`. The
test materialises them into a temporary home before running.

`opencode.sql` carries both schema generations, as a store upgraded to
OpenCode v2 does: the v1 session, its `session_v2` mirror without a
`session_message` projection, and one session written by v2.0.11 whose
snapshot paths are relative to the project root.
