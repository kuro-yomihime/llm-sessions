//! Command dispatch: shared scan plumbing here, one file per command group.

mod args;
mod federation;
mod hosts;
mod index;
mod list;
mod report;
mod session;
mod top;
mod tui;

use std::path::Path;

use clap::Parser;
use llm_sessions::catalog::loader::{Cached, Loader, Remote};
use llm_sessions::error::Error;
use llm_sessions::runtime::live::Fleet;
use llm_sessions::{Result, Scan, home};
use time::OffsetDateTime;

use self::args::{Cli, Command, HostsCommand};

pub fn run() -> Result<()> {
    let home = home()?;
    let cli = Cli::parse();
    if !cli.read_only
        && !matches!(
            &cli.command,
            Some(
                Command::Federation { .. }
                    | Command::Origin { .. }
                    | Command::Completions(_)
                    | Command::Man
            )
        )
        && let Err(error) = llm_sessions::federation::ensure_origin(&home)
    {
        eprintln!("llm-sessions: origin identity unavailable: {error}");
    }
    match cli.command {
        None => tui::run(&home, cli.read_only),
        Some(Command::List(args)) => list::run(&home, &args),
        Some(Command::Federation { command }) => federation::run(&home, &command),
        Some(Command::Origin { init }) => {
            if init && cli.read_only {
                return Err(Error::ReadOnly {
                    what: "initializing origin identity",
                });
            }
            let origin = if init {
                llm_sessions::federation::ensure_origin(&home)?
            } else {
                llm_sessions::federation::read_origin(&home)?
            };
            emit(&origin);
            Ok(())
        }
        Some(Command::Workspaces(args)) => list::workspaces(&home, &args),
        Some(Command::Show(args)) => session::show(&home, &args),
        Some(Command::Sources(args)) => hosts::sources(&home, args.json),
        Some(Command::Hosts(args)) => match args.command {
            None => hosts::list(&home, args.json),
            Some(
                HostsCommand::Add(_) | HostsCommand::Remove { .. } | HostsCommand::Refresh { .. },
            ) if cli.read_only => Err(Error::ReadOnly {
                what: "editing hosts.json",
            }),
            Some(HostsCommand::Add(args)) => hosts::add(&home, args),
            Some(HostsCommand::Remove { name }) => hosts::remove(&home, &name),
            Some(HostsCommand::Refresh { name, init, force }) => {
                hosts::refresh(&home, &name, init, force)
            }
        },
        Some(Command::Index(args)) if cli.read_only && !args.stats => {
            Err(Error::ReadOnly { what: "indexing" })
        }
        Some(Command::Index(args)) => index::build(&home, &args),
        Some(Command::Search(args)) => index::search(&home, &args),
        Some(Command::Digest(args)) => report::digest(&home, &args),
        Some(Command::Top(args)) => top::run(&home, &args),
        Some(Command::Usage(args)) => report::usage(&home, &args),
        #[cfg(feature = "serve")]
        Some(Command::Serve(args)) => llm_sessions::serve::run(
            &home,
            llm_sessions::serve::Config {
                bind: args.bind,
                token: args.token,
                no_auth: args.no_auth,
                host_name: args.host_name,
                poll: std::time::Duration::from_secs(args.poll_secs),
                status_window: std::time::Duration::from_hours(args.status_window),
                sources: args.sources,
                query: args.query,
                federate: args.federate,
                cors_origins: args.cors_origins,
                #[cfg(feature = "dashboard")]
                dashboard: args.dashboard,
            },
        ),
        Some(Command::Doctor(args)) => report::doctor(&home, &args),
        Some(Command::Completions(args)) => {
            let mut command = <Cli as clap::CommandFactory>::command();
            clap_complete::generate(
                args.shell,
                &mut command,
                "llm-sessions",
                &mut std::io::stdout(),
            );
            Ok(())
        }
        Some(Command::Man) => {
            let command = <Cli as clap::CommandFactory>::command();
            clap_mangen::Man::new(command)
                .render(&mut std::io::stdout())
                .map_err(llm_sessions::error::io("stdout"))
        }
        Some(Command::Archive(args)) if cli.read_only => Err(Error::ReadOnly {
            what: if args.undo {
                "unarchiving"
            } else {
                "archiving"
            },
        }),
        Some(Command::Archive(args)) => session::archive(&home, &args),
    }
}

/// How much a subcommand needs to know about running processes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Liveliness {
    /// Renders `●`, a pane, or working/waiting. Worth the probe.
    Needed,
    /// Never shown. Measured: `llm-sessions sources` 0.061 s against `llm-sessions list`
    /// 0.332 s — the whole difference is the probe, so commands that do not
    /// render liveness must not pay for it.
    Skip,
}

/// Everything one scan produced, for a subcommand to take what it needs.
struct Loaded {
    /// The scan's inputs: adapters for opening and archiving what it
    /// listed, the overlay, every configured host.
    loader: Loader,
    /// Running agent processes; empty under [`Liveliness::Skip`].
    fleet: Fleet,
    /// The listed sessions and every failure met along the way.
    scan: Scan,
}

/// One scan, shared by every subcommand.
///
/// [`Fleet::probe`] blocks a quarter second so the very first output can
/// already say working or waiting; nothing after that ever blocks for it.
fn scan_all(home: &Path, liveliness: Liveliness) -> Loaded {
    scan_all_with(home, liveliness, Cached::Borrow, Remote::Inline)
}

fn scan_all_with(home: &Path, liveliness: Liveliness, cached: Cached, remote: Remote) -> Loaded {
    let loader = Loader::new(home);
    let fleet = match liveliness {
        Liveliness::Needed => Fleet::probe(),
        Liveliness::Skip => Fleet::default(),
    };
    let attribution = (liveliness == Liveliness::Needed).then_some(&fleet);
    let scan = loader.scan(attribution, cached, remote);
    Loaded {
        loader,
        fleet,
        scan,
    }
}

/// Parse a `--since` value against the current time.
fn parse_since(spec: &str) -> Result<OffsetDateTime> {
    llm_sessions::clock::parse_since(spec, OffsetDateTime::now_utc()).ok_or_else(|| {
        Error::BadArgument {
            what: "--since",
            got: spec.to_owned(),
        }
    })
}

/// Print a line, treating a closed pipe as a clean end.
///
/// `llm-sessions list | head` is the obvious thing to type, and `println!` panics on
/// `EPIPE`. Exiting quietly is what every other command-line tool does.
fn emit(line: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    if let Err(e) = writeln!(out, "{line}")
        && e.kind() == std::io::ErrorKind::BrokenPipe
    {
        std::process::exit(0);
    }
}

fn report_failures(scan: &Scan) {
    for message in &scan.failures {
        eprintln!("llm-sessions: {message}");
    }
}
