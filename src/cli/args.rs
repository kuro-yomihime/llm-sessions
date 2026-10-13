use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use llm_sessions::output::usage::Axis;

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
    /// Refuse every mutation: archiving, indexing, moving tmux focus.
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
    /// Search indexed transcript bodies.
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
    /// Inspect discovered stores and scan costs.
    #[command(display_order = 10)]
    Sources,
    /// Show, add, or remove remote hosts (hosts.json). Bare `llm-sessions hosts`
    /// lists every configured host and probes it, so one command answers
    /// "is my fleet reachable and which transport will it use".
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
    #[arg(long, conflicts_with = "json")]
    pub format: Option<String>,

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
pub struct HostsArgs {
    #[command(subcommand)]
    pub command: Option<HostsCommand>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum HostsCommand {
    /// Add a host, or update an existing one of the same name.
    Add {
        /// Display name: what rows, groups and queries show.
        name: String,
        /// Anything `ssh` accepts — an ~/.ssh/config alias or `user@host`.
        /// Omit for an HTTP peer (`--url`).
        ssh: Option<String>,
        /// Base URL of a peer running `llm-sessions serve`, e.g.
        /// `https://fleet.example.net`. Exactly one of ssh or --url.
        #[arg(long, value_name = "URL", conflicts_with = "ssh")]
        url: Option<String>,
        /// Environment variable holding this peer's bearer token. The token
        /// itself is never written to hosts.json.
        #[arg(long, value_name = "VAR", requires = "url")]
        token_env: Option<String>,
        /// Remote home directory. Omit to ask the host at probe time.
        #[arg(long, value_name = "PATH")]
        home: Option<PathBuf>,
        /// Agent names to scan. Repeatable; omit for all.
        #[arg(long = "source", value_name = "AGENT")]
        sources: Vec<String>,
        /// Transport: auto (default), llm-sessions, or shell. SSH only.
        #[arg(long, value_enum)]
        mode: Option<HostMode>,
    },
    /// Remove a host by name.
    Remove { name: String },
}

/// Clap-facing mirror of [`llm_sessions::source::remote::Mode`], whose
/// serialized spellings remain independent of the command-line interface.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum HostMode {
    Auto,
    LlmSessions,
    Shell,
}

#[derive(Debug, Clone, clap::Args)]
pub struct FilterArgs {
    /// Only sessions from this agent. Repeatable.
    #[arg(long = "source", value_name = "AGENT")]
    pub sources: Vec<String>,
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
    #[arg(long)]
    pub origin: Option<String>,

    /// Restrict resolution to this agent.
    #[arg(long)]
    pub source: Option<String>,

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

    /// Skip sessions older than this: `7d`, `2w`, `today`, RFC 3339.
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
    /v1/stream                     WebSocket: live and recent rows, pushed on change\n\n\
    Read-only by construction: no route archives, indexes, edits hosts, or\n\
    reaches a terminal. Clients send `Authorization: Bearer TOKEN`; /v1/stream\n\
    also takes `?token=TOKEN`. There is no TLS: off loopback, put a TLS\n\
    reverse proxy in front.")]
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
}

/// Nothing that prints args may print the token: one stray `{:?}` on the
/// parsed CLI would otherwise copy the secret into a log. `Command` derives
/// `Debug`, so this is a lying `Debug` on purpose.
#[cfg(feature = "serve")]
impl std::fmt::Debug for ServeArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeArgs")
            .field("bind", &self.bind)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("no_auth", &self.no_auth)
            .field("host_name", &self.host_name)
            .field("poll_secs", &self.poll_secs)
            .field("status_window", &self.status_window)
            .field("sources", &self.sources)
            .field("query", &self.query)
            .finish()
    }
}

#[cfg(feature = "serve")]
fn parse_source(name: &str) -> Result<llm_sessions::SourceId, String> {
    llm_sessions::SourceId::parse(&name.to_lowercase()).ok_or_else(|| {
        let known: Vec<&str> = llm_sessions::SourceId::ALL
            .iter()
            .map(|id| id.name())
            .collect();
        format!("unknown agent; one of: {}", known.join(", "))
    })
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

    /// Only sessions touched since: `7d`, `2w`, `today`, RFC 3339.
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
    #[arg(required = true, num_args = 1..)]
    pub query: Vec<String>,

    /// Maximum hits.
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
        for flag in ["--format={id}", "-0", "--null"] {
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
