use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use llm_sessions::output::usage::Axis;
use llm_sessions::source::remote::Mode;

#[derive(Debug, Parser)]
#[command(
    name = "llm-sessions",
    version,
    about = "Explore local LLM agent sessions, keyed by workspace",
    after_help = "Getting started:\n\
                  llm-sessions                      Browse workspaces, sessions, and transcripts\n\
                  llm-sessions list --live          Find sessions with an attached agent\n\
                  llm-sessions show <id>            Read a transcript\n\
                  llm-sessions index                Index bodies, then `search <query>`\n\n\
                  The TUI renders on stderr. Its stdout is a shell-quoted resume command\n\
                  for inspection; copy it into your shell after reviewing it. Do not run\n\
                  `$(llm-sessions)`: command substitution does not safely parse shell quoting."
)]
pub struct Cli {
    /// Refuse every mutation: archiving, indexing, host edits, origin init,
    /// bookmarks, sending messages, moving tmux focus.
    #[arg(long, global = true)]
    pub read_only: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// List sessions, newest first.
    #[command(display_order = 1)]
    List(ListArgs),
    /// Browse workspace totals across matching sessions.
    #[command(display_order = 2)]
    Workspaces(WorkspacesArgs),
    /// Read a session's normalized transcript.
    #[command(display_order = 3)]
    Show(ShowArgs),
    /// Search indexed transcript bodies. The index holds this machine's
    /// sessions only; filters are `list`'s, so `--host` can select only `local`.
    #[command(display_order = 4)]
    Search(SearchArgs),
    /// Summarize recent sessions by workspace.
    #[command(display_order = 5)]
    Digest(DigestArgs),
    /// Show running agents and their activity.
    #[command(display_order = 6)]
    Top(TopArgs),
    /// Summarize recorded spend.
    #[command(display_order = 7)]
    Usage(UsageArgs),
    /// Serve this machine's sessions over a read-only, token-authenticated
    /// HTTP and WebSocket API.
    #[cfg(feature = "serve")]
    #[command(display_order = 8)]
    Serve(ServeArgs),
    /// Build or update the full-text search index. Indexes local sessions
    /// only: fetching remote bodies would mean one ssh per session.
    #[command(display_order = 9)]
    Index(IndexArgs),
    /// Inspect this machine's discovered stores and scan costs. Remote
    /// machines are `llm-sessions hosts`.
    #[command(display_order = 10)]
    Sources(SourcesArgs),
    /// Show, add, remove, or refresh remote hosts (hosts.json). Bare
    /// `llm-sessions hosts` lists every configured host with its pinned
    /// origin and probes it, so one command answers "is my fleet reachable
    /// and which transport will it use".
    #[command(display_order = 10)]
    Hosts(HostsArgs),
    /// Report orphaned processes, dead paths, and stale caches.
    #[command(display_order = 11)]
    Doctor(DoctorArgs),
    /// Archive or unarchive sessions by id.
    #[command(display_order = 12)]
    Archive(ArchiveArgs),
    /// Print a shell completion script.
    #[command(display_order = 13)]
    Completions(CompletionsArgs),
    /// Print the roff man page.
    #[command(display_order = 14)]
    Man,
    /// Print this instance's stable federation identity.
    Origin {
        /// Create the marker if it does not exist.
        #[arg(long)]
        init: bool,
    },
    /// Internal, guarded federation transport.
    #[command(hide = true)]
    Federation {
        #[command(subcommand)]
        command: FederationCommand,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum FederationCommand {
    Capabilities,
    List {
        #[arg(long)]
        path: String,
        #[arg(long)]
        hops: u32,
    },
    Show {
        #[arg(long)]
        origin: String,
        #[arg(long)]
        source: String,
        #[arg(long)]
        id: String,
        #[arg(long)]
        route: String,
    },
    Top,
}

#[derive(Debug, Clone, clap::Args)]
pub struct ListArgs {
    /// Emit JSON instead of a table.
    #[arg(long)]
    pub json: bool,

    #[command(flatten)]
    pub filters: FilterArgs,

    /// Maximum rows. 0 means no limit.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,

    /// Row template, e.g. '{liveness}\t{id}\t{workspace}\t{title}'.
    /// Fields: liveness pid pane activity source host id title label workspace
    /// branch model cost tokens files updated path.
    #[arg(long, value_name = "TEMPLATE", conflicts_with = "json")]
    pub template: Option<String>,

    /// Separate rows with NUL instead of newline, for `xargs -0`.
    #[arg(long, short = '0')]
    pub null: bool,
}

#[derive(Debug, Clone, clap::Args)]
pub struct WorkspacesArgs {
    /// Emit workspace, count, live, agents, and cost as JSON.
    #[arg(long)]
    pub json: bool,

    #[command(flatten)]
    pub filters: FilterArgs,

    /// Maximum workspace groups, after counting all matches. 0 means no limit.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
}

#[derive(Debug, Clone, clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct HostsArgs {
    /// Emit name, destination, sources, pinned origin, and status as JSON.
    #[arg(long)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Option<HostsCommand>,
}

#[derive(Debug, Clone, clap::Args)]
pub struct SourcesArgs {
    /// Emit agent, sessions (or error), seconds, and roots as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Subcommand)]
pub enum HostsCommand {
    /// Add a host, or update an existing one of the same name.
    /// Enroll unpinned peers automatically; existing pins require a matching
    /// identity. Use hosts refresh to deliberately replace a pin.
    Add(AddHostArgs),
    /// Remove a host by name.
    Remove { name: String },
    /// Enroll or replace a peer's origin UUID through its configured connection.
    Refresh {
        /// Host to refresh; omit with --all.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        name: Option<String>,
        /// Refresh every configured host. Failures are reported and skipped;
        /// successful pins are still saved.
        #[arg(long)]
        all: bool,
        /// First create the peer's origin-id marker if it does not exist —
        /// on the peer, over ssh, no llm-sessions needed there. A malformed
        /// marker is regenerated.
        #[arg(long)]
        init: bool,
        /// With --init, replace a valid existing marker too, rekeying the peer.
        #[arg(long, requires = "init")]
        force: bool,
    },
}

#[derive(Debug, Clone, clap::Args)]
pub struct AddHostArgs {
    /// Display name: what rows, groups and queries show.
    pub name: String,
    /// Anything `ssh` accepts — an ~/.ssh/config alias or `user@host`.
    /// Omit for an HTTP peer (`--url`).
    pub ssh: Option<String>,
    /// Base URL of a peer running `llm-sessions serve`, e.g.
    /// `https://fleet.example.net`. Exactly one of ssh or --url.
    #[arg(long, value_name = "URL", conflicts_with = "ssh")]
    pub url: Option<String>,
    /// Environment variable holding this peer's bearer token. The token
    /// itself is never written to hosts.json.
    #[arg(long, value_name = "VAR", requires = "url")]
    pub token_env: Option<String>,
    /// Remote home directory. Omit to ask the host at probe time.
    #[arg(long, value_name = "PATH")]
    pub home: Option<PathBuf>,
    /// Only scan these agents. Repeatable; omit for all.
    #[arg(long = "source", value_name = "AGENT", value_parser = parse_source)]
    pub sources: Vec<llm_sessions::SourceId>,
    /// Transport: auto (default), llm-sessions, or shell. SSH only.
    #[arg(long, value_enum)]
    pub mode: Option<Mode>,
}

#[derive(Debug, Clone, clap::Args)]
pub struct FilterArgs {
    /// Only sessions from this agent. Repeatable.
    #[arg(long = "source", value_name = "AGENT", value_parser = parse_source)]
    pub sources: Vec<llm_sessions::SourceId>,
    /// Only sessions with this origin display label. Repeatable.
    /// Direct origins use their hosts.json name; `local` selects this machine.
    #[arg(long = "host", value_name = "NAME")]
    pub hosts: Vec<String>,

    /// Only sessions whose workspace is at or below this path.
    #[arg(long, value_name = "PATH")]
    pub workspace: Option<PathBuf>,

    /// Include archived sessions.
    #[arg(long)]
    pub archived: bool,

    /// Only sessions an agent process is attached to right now.
    #[arg(long)]
    pub live: bool,

    /// The same query language the TUI uses:
    /// `source:omp ws:dstm after:7d cost:>1 live:yes`, bare words are text.
    #[arg(long, short = 'q')]
    pub query: Option<String>,
}

#[derive(Debug, Clone, clap::Args)]
pub struct ShowArgs {
    /// Session id, or a unique prefix of one.
    pub id: String,
    /// Restrict resolution to this stable origin ID.
    #[arg(long, value_name = "UUID", value_parser = parse_origin_id)]
    pub origin: Option<String>,

    /// Restrict resolution to this agent.
    #[arg(long, value_name = "AGENT", value_parser = parse_source)]
    pub source: Option<llm_sessions::SourceId>,

    /// Emit the transcript as JSON.
    #[arg(long, conflicts_with = "format")]
    pub json: bool,

    /// Show thinking blocks in full instead of folding them.
    #[arg(long)]
    pub thinking: bool,

    /// Show tool output in full instead of folding it.
    #[arg(long)]
    pub tools: bool,

    /// Render a shareable report instead of terminal text.
    #[arg(long, value_enum)]
    pub format: Option<ShowFormat>,

    /// Write here instead of stdout.
    #[arg(long, short = 'o')]
    pub out: Option<PathBuf>,
}

/// Output representation for `llm-sessions show`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ShowFormat {
    /// Terminal text (default).
    Text,
    /// Markdown report.
    #[value(alias = "markdown")]
    Md,
    /// Self-contained HTML page.
    Html,
}

#[derive(Debug, Clone, clap::Args)]
pub struct IndexArgs {
    /// Discard the existing index and reindex everything.
    #[arg(long)]
    pub rebuild: bool,

    /// Skip sessions older than this: `today`, `yesterday`, `24h`, `7d`,
    /// `2w`, or an RFC 3339 timestamp.
    #[arg(long)]
    pub since: Option<String>,

    /// Truncate each indexed body to this many bytes. A 165 KB tool output
    /// is not a search target.
    #[arg(long, value_name = "BYTES")]
    pub max_body: Option<usize>,

    /// Drop the oldest documents until the index fits in this many bytes.
    #[arg(long, value_name = "BYTES")]
    pub max_bytes: Option<u64>,

    /// Reclaim free pages. The one operation that takes real write time.
    #[arg(long)]
    pub vacuum: bool,

    /// Print size and coverage, then exit.
    #[arg(long)]
    pub stats: bool,
}

#[derive(Debug, Clone, clap::Args)]
pub struct CompletionsArgs {
    /// Shell to generate for.
    #[arg(value_enum)]
    pub shell: clap_complete::Shell,
}

#[derive(Debug, Clone, clap::Args)]
pub struct TopArgs {
    /// Emit JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

/// Upper bound for --status-window: far below where `Duration::from_hours`
/// panics on overflow, and past any session anyone will wait on.
#[cfg(feature = "serve")]
const MAX_STATUS_WINDOW_HOURS: u64 = 100 * 365 * 24;

/// Manual `Debug`, below: the derived one would print the token.
#[cfg(feature = "serve")]
#[derive(Clone, clap::Args)]
#[command(after_long_help = "Routes, all GET:\n  \
    /v1/healthz                    liveness probe, no token\n  \
    /v1/sessions?q=QUERY&limit=N   rows as `list --json`, plus a derived `status`\n  \
    /v1/sessions/ID                one row; ID may be a unique prefix\n  \
    /v1/sessions/ID/transcript     the bytes of `show ID --json`\n  \
    /v1/federation/capabilities    this server's stable origin identity\n  \
    /v1/federation/list?path=P&hops=H   guarded federation discovery (see --federate)\n  \
    /v1/stream                     WebSocket: live and recent rows, pushed on change\n\n\
    Read-only by construction: no route archives, indexes, edits hosts, or\n\
    reaches a terminal. Clients send `Authorization: Bearer TOKEN`; /v1/stream\n\
    also takes `?token=TOKEN`. There is no TLS: off loopback, put a TLS\n\
    reverse proxy in front.\n\n\
    Browser pages on another origin need --cors-origin; preflights from a\n\
    listed origin are answered without the token, every GET still needs it.\n\n\
    Federation is opt-in: without --federate the server lists this machine\n\
    only and never contacts a peer, even with hosts configured.")]
pub struct ServeArgs {
    /// Address to listen on. Only a loopback address may use --no-auth.
    #[arg(long, env = "LLMS_BIND", default_value = "127.0.0.1:8787")]
    pub bind: std::net::SocketAddr,

    /// Shared secret clients must present. Required unless --no-auth; at
    /// least 16 characters.
    #[arg(long, env = "LLMS_TOKEN", hide_env_values = true)]
    pub token: Option<String>,

    /// Serve without a token. Refused unless --bind is a loopback address.
    #[arg(long, conflicts_with = "token")]
    pub no_auth: bool,

    /// Host name /v1/healthz reports [default: system hostname, lowercased].
    #[arg(long, value_name = "NAME")]
    pub host_name: Option<String>,

    /// Seconds between rescans.
    #[arg(long, value_name = "N", default_value_t = 3,
          value_parser = clap::value_parser!(u64).range(1..))]
    pub poll_secs: u64,

    /// Sessions written within this many hours get a derived status even
    /// with no agent attached. Live sessions always do.
    #[arg(long, value_name = "HOURS", default_value_t = 1,
          value_parser = clap::value_parser!(u64).range(1..=MAX_STATUS_WINDOW_HOURS))]
    pub status_window: u64,

    /// Only sessions from this agent. Repeatable.
    #[arg(long = "source", value_name = "AGENT", value_parser = parse_source)]
    pub sources: Vec<llm_sessions::SourceId>,

    /// Base query every published row must match, in the `list -q` grammar.
    #[arg(long, short = 'q')]
    pub query: Option<String>,

    /// Gateway mode: also list the peers in hosts.json and serve their
    /// sessions, origin-qualified. Without it, serve publishes this
    /// machine only and never contacts a peer.
    #[arg(long)]
    pub federate: bool,

    /// Let browser pages on this origin (scheme, host, optional port) call
    /// the API. Repeatable; needs --token; no wildcard.
    #[arg(long = "cors-origin", value_name = "ORIGIN",
          value_parser = llm_sessions::serve::parse_origin, conflicts_with = "no_auth")]
    pub cors_origins: Vec<String>,

    /// Also serve the web dashboard at `/`. The page holds no data; open
    /// it as /#token=TOKEN and it calls the API with that token.
    #[cfg(feature = "dashboard")]
    #[arg(long)]
    pub dashboard: bool,
}

/// Nothing that prints args may print the token: one stray `{:?}` on the
/// parsed CLI would otherwise copy the secret into a log. `Command` derives
/// `Debug`, so this is a lying `Debug` on purpose.
#[cfg(feature = "serve")]
impl std::fmt::Debug for ServeArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("ServeArgs");
        debug
            .field("bind", &self.bind)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("no_auth", &self.no_auth)
            .field("host_name", &self.host_name)
            .field("poll_secs", &self.poll_secs)
            .field("status_window", &self.status_window)
            .field("sources", &self.sources)
            .field("query", &self.query)
            .field("federate", &self.federate)
            .field("cors_origins", &self.cors_origins);
        #[cfg(feature = "dashboard")]
        debug.field("dashboard", &self.dashboard);
        debug.finish()
    }
}

/// An agent name, checked when arguments are parsed so an unknown one is
/// refused with the list of known names instead of silently matching nothing.
fn parse_source(name: &str) -> Result<llm_sessions::SourceId, String> {
    llm_sessions::SourceId::parse(&name.to_lowercase()).ok_or_else(|| {
        let known: Vec<&str> = llm_sessions::SourceId::ALL
            .iter()
            .map(|id| id.name())
            .collect();
        format!("unknown agent; one of: {}", known.join(", "))
    })
}

/// A stable origin ID, in the canonical lowercase form rows carry.
fn parse_origin_id(origin: &str) -> Result<String, String> {
    if llm_sessions::federation::valid_origin(origin) {
        Ok(llm_sessions::model::normalize_origin(origin))
    } else {
        Err("not an origin UUID (see `llm-sessions origin`)".into())
    }
}

#[derive(Debug, Clone, clap::Args)]
pub struct DoctorArgs {
    /// Emit findings as JSON: severity, check, detail, fix (null when none).
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, clap::Args)]
pub struct UsageArgs {
    /// Axis to aggregate on.
    #[arg(long, default_value_t = Axis::Workspace)]
    pub by: Axis,

    /// Only sessions touched since: `today`, `yesterday`, `24h`, `7d`, `2w`,
    /// or an RFC 3339 timestamp.
    #[arg(long)]
    pub since: Option<String>,

    /// Maximum rows. 0 means no limit.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,

    /// Emit JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, clap::Args)]
pub struct SearchArgs {
    /// Words to look for in session bodies.
    #[arg(value_name = "WORDS", required = true, num_args = 1..)]
    pub words: Vec<String>,

    // Same filters as `list`, applied to the hits.
    #[command(flatten)]
    pub filters: FilterArgs,

    /// Maximum hits, after filtering. 0 means no limit.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,

    /// Emit JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, clap::Args)]
pub struct ArchiveArgs {
    /// Session ids, or unique prefixes.
    #[arg(required = true, num_args = 1..)]
    pub ids: Vec<String>,

    /// Restore instead of archiving.
    #[arg(long)]
    pub undo: bool,
}

#[derive(Debug, Clone, clap::Args)]
pub struct DigestArgs {
    /// How far back to look: `today`, `yesterday`, `24h`, `7d`, `2w`, or an
    /// RFC 3339 timestamp.
    #[arg(long, default_value = "yesterday")]
    pub since: String,

    /// Emit JSON instead of text.
    #[arg(long)]
    pub json: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspaces_rejects_session_row_output_flags() {
        for flag in ["--template={id}", "-0", "--null"] {
            let error = Cli::try_parse_from(["llm-sessions", "workspaces", flag]).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
            // These remain supported by list, where they describe session rows.
            assert!(matches!(
                Cli::try_parse_from(["llm-sessions", "list", flag])
                    .unwrap()
                    .command,
                Some(Command::List(_))
            ));
        }
    }

    #[test]
    fn agent_and_origin_filters_are_checked_and_canonical_at_parse_time() {
        for command in [
            &["llm-sessions", "list", "--source", "nope"][..],
            &["llm-sessions", "search", "x", "--source", "nope"],
            &["llm-sessions", "show", "x", "--source", "nope"],
            &[
                "llm-sessions",
                "hosts",
                "add",
                "h",
                "h.example",
                "--source",
                "nope",
            ],
            &["llm-sessions", "show", "x", "--origin", "not-a-uuid"],
        ] {
            let error = Cli::try_parse_from(command).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::ValueValidation,
                "{command:?}"
            );
        }
        // Rows carry lowercase origins; an uppercase paste still resolves.
        let parsed = Cli::try_parse_from([
            "llm-sessions",
            "show",
            "x",
            "--origin",
            "00000000-0000-4000-8000-0000000000AA",
            "--source",
            "OMP",
        ])
        .unwrap();
        let Some(Command::Show(args)) = parsed.command else {
            panic!("show did not parse");
        };
        assert_eq!(
            args.origin.as_deref(),
            Some("00000000-0000-4000-8000-0000000000aa")
        );
        assert_eq!(args.source, Some(llm_sessions::SourceId::Omp));
    }

    /// One stray `{:?}` on the parsed CLI must not copy the token into a
    /// log, so `Debug` is a lie for that one field.
    #[cfg(feature = "serve")]
    #[test]
    fn serve_args_debug_never_prints_the_token() {
        let args = ServeArgs {
            bind: "127.0.0.1:8787".parse().unwrap(),
            token: Some("s3cret-do-not-print".into()),
            no_auth: false,
            host_name: None,
            poll_secs: 3,
            status_window: 1,
            sources: Vec::new(),
            query: None,
            federate: false,
            cors_origins: Vec::new(),
            #[cfg(feature = "dashboard")]
            dashboard: false,
        };
        let printed = format!("{args:?}");
        assert!(!printed.contains("s3cret"), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
    }

    /// `--status-window` used to panic at startup on huge values, when
    /// `Duration::from_hours` overflowed; now the parser refuses.
    #[cfg(feature = "serve")]
    #[test]
    fn status_window_takes_a_range_not_a_panic() {
        for bad in ["0", "99999999999"] {
            let error =
                Cli::try_parse_from(["llm-sessions", "serve", "--status-window", bad]).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::ValueValidation,
                "{bad}"
            );
        }
        let parsed =
            Cli::try_parse_from(["llm-sessions", "serve", "--status-window", "87600"]).unwrap();
        match parsed.command {
            Some(Command::Serve(args)) => {
                assert_eq!(args.status_window, 87_600);
                // The ceiling stays convertible to a Duration.
                let _ = std::time::Duration::from_hours(MAX_STATUS_WINDOW_HOURS);
            }
            _ => panic!("the serve command did not parse"),
        }
    }
}
