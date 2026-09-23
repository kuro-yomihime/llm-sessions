//! Command dispatch: shared scan plumbing here, one file per command group.

mod args;
mod hosts;
mod index;
mod list;
mod report;
mod session;
mod top;
mod tui;

use std::path::Path;

use clap::Parser;
use llm_sessions::error::Error;
use llm_sessions::runtime::live::Fleet;
use llm_sessions::source::remote;
use llm_sessions::storage::index::Index;
use llm_sessions::storage::overlay::Overlay;
use llm_sessions::{Result, Scan, home, scan_with, source};
use time::OffsetDateTime;

use self::args::{Cli, Command, HostsCommand};

pub fn run() -> Result<()> {
    let home = home()?;
    let cli = Cli::parse();
    match cli.command {
        None => tui::run(&home, cli.read_only),
        Some(Command::List(args)) => list::run(&home, &args),
        Some(Command::Workspaces(args)) => list::workspaces(&home, &args),
        Some(Command::Show(args)) => session::show(&home, &args),
        Some(Command::Sources) => {
            hosts::sources(&home);
            Ok(())
        }
        Some(Command::Hosts(args)) => match args.command {
            None => {
                hosts::list(&home);
                Ok(())
            }
            Some(HostsCommand::Add { .. } | HostsCommand::Remove { .. }) if cli.read_only => {
                Err(Error::ReadOnly {
                    what: "editing hosts.json",
                })
            }
            Some(HostsCommand::Add {
                name,
                ssh,
                home: remote_home,
                sources: wanted,
                mode,
            }) => hosts::add(&home, name, ssh, remote_home, wanted, mode),
            Some(HostsCommand::Remove { name }) => hosts::remove(&home, &name),
        },
        Some(Command::Index(args)) if cli.read_only && !args.stats => {
            Err(Error::ReadOnly { what: "indexing" })
        }
        Some(Command::Index(args)) => index::build(&home, &args),
        Some(Command::Search(args)) => index::search(&home, &args),
        Some(Command::Digest(args)) => report::digest(&home, &args),
        Some(Command::Top(args)) => top::run(&home, &args),
        Some(Command::Usage(args)) => report::usage(&home, &args),
        Some(Command::Doctor) => {
            report::doctor(&home);
            Ok(())
        }
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

/// The overlay is optional: a read-only or unwritable state directory must
/// degrade to "no archive state", never to a failed launch.
fn overlay_for(home: &Path) -> Option<Overlay> {
    Overlay::open(&Overlay::default_path(home)).ok()
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
    /// Every local adapter, for opening and archiving what the scan listed.
    sources: Vec<Box<dyn source::Source>>,
    /// Local archive state, when the state directory is usable.
    overlay: Option<Overlay>,
    /// Running agent processes; empty under [`Liveliness::Skip`].
    fleet: Fleet,
    /// Every configured remote host, whether or not this scan contacted it.
    hosts: Vec<remote::Host>,
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

/// Whether a scan may fill blank tier-1 usage from the index cache.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cached {
    /// Presentation: show the best number available.
    Borrow,
    /// Index refresh: the cache is the output, so reading it back in would
    /// make a stale value immortal. Codex's corrected token split survived
    /// three full rebuilds this way before the loop was cut.
    Ignore,
}

/// Whether this scan waits on SSH or defers remote rows to the TUI cache.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Remote {
    /// CLI: one blocking fetch per host, rows inline. A hung host costs at
    /// most its connect timeout, then becomes a failure line.
    Inline,
    /// TUI: local rows only. `remote::Cache` refreshes off-thread so no
    /// frame ever blocks on a socket.
    Cached,
}

fn scan_all_with(
    home: &Path,
    liveliness: Liveliness,
    cached: Cached,
    remote_mode: Remote,
) -> Loaded {
    let sources = source::all(home);
    let overlay = overlay_for(home);
    let fleet = match liveliness {
        Liveliness::Needed => Fleet::probe(),
        Liveliness::Skip => Fleet::default(),
    };
    let (config, load_failure) = remote::Config::load(home);
    let hosts: Vec<remote::Host> = config.hosts.into_iter().map(remote::Host::new).collect();
    let scanned: &[remote::Host] = match remote_mode {
        Remote::Inline => &hosts,
        Remote::Cached => &[],
    };
    let attribution = (liveliness == Liveliness::Needed).then_some(&fleet);
    let mut scan = scan_with(&sources, overlay.as_ref(), attribution, scanned);
    if let Some(failure) = load_failure {
        scan.failures.push(failure);
    }
    if cached == Cached::Borrow {
        borrow_cached_usage(home, &mut scan);
    }
    Loaded {
        sources,
        overlay,
        fleet,
        hosts,
        scan,
    }
}

/// OMP and Claude only record spend inside the transcript, so tier 1 cannot
/// price them. `llm-sessions index` already parsed those bodies; borrow its answer
/// rather than reparsing 280 MB to fill a column.
fn borrow_cached_usage(home: &Path, scan: &mut Scan) {
    let Ok(index) = Index::open(&Index::default_path(home)) else {
        return;
    };
    let Ok(cached) = index.usage() else { return };
    cached.fill(&mut scan.sessions);
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
