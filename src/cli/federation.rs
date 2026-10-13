//! Read-only federation protocol. Discovery context crosses every SSH hop.

use std::path::{Path, PathBuf};

use llm_sessions::catalog::loader::{borrow_cached_usage, extend_from_origin};
use llm_sessions::catalog::merge;
use llm_sessions::error::Error;
use llm_sessions::federation::{FederationScan, read_origin, valid_origin};
use llm_sessions::model::{AccessRoute, RouteHop};
use llm_sessions::runtime::live::Fleet;
use llm_sessions::source::{self, remote};
use llm_sessions::storage::overlay::Overlay;
use llm_sessions::{Result, SessionSummary, SourceId, scan_with};

use super::args::FederationCommand;
use super::emit;

pub(super) fn run(home: &Path, command: &FederationCommand) -> Result<()> {
    match command {
        FederationCommand::Capabilities => {
            emit(&serde_json::to_string(&serde_json::json!({
                "version": llm_sessions::federation::PROTOCOL_VERSION,
                "origin_id": read_origin(home)?,
            }))?);
        }
        FederationCommand::List { path, hops } => {
            let mut visited: Vec<String> = serde_json::from_str(path)?;
            if visited.len() > llm_sessions::federation::DEFAULT_HOPS as usize
                || *hops > llm_sessions::federation::DEFAULT_HOPS
                || visited.iter().any(|id| !valid_origin(id))
            {
                return Err(Error::BadArgument {
                    what: "federation path",
                    got: path.clone(),
                });
            }
            for id in &mut visited {
                id.make_ascii_lowercase();
            }
            let origin = read_origin(home)?;
            if visited.contains(&origin) {
                emit(&serde_json::to_string(&FederationScan::default())?);
                return Ok(());
            }
            let sources = source::all(home);
            let overlay = Overlay::open(&Overlay::default_path(home)).ok();
            let fleet = Fleet::probe();
            let mut scan = scan_with(&sources, overlay.as_ref(), Some(&fleet), &[]);
            let (config, failure) = remote::Config::load(home);
            scan.failures.extend(failure);
            let hosts: Vec<_> = config.hosts.into_iter().map(remote::Host::new).collect();
            extend_from_origin(&mut scan, &sources, &hosts, origin, visited, *hops);
            scan.sessions = merge::merge(scan.sessions);
            borrow_cached_usage(home, &mut scan);
            emit(&serde_json::to_string(&FederationScan {
                sessions: scan.sessions,
                failures: scan.failures,
            })?);
        }
        FederationCommand::Show {
            origin,
            source,
            id,
            route,
        } => {
            let source = SourceId::parse(source).ok_or_else(|| Error::BadArgument {
                what: "source",
                got: source.clone(),
            })?;
            let mut hops: Vec<RouteHop> = serde_json::from_str(route)?;
            // A route may name this machine's own token bindings; a peer
            // forwarding one may not. Credential references never cross the
            // wire: local configuration re-binds them at open time.
            for hop in &mut hops {
                hop.token_env = None;
            }
            let own = read_origin(home)?;
            let origin = origin.to_ascii_lowercase();
            for hop in &mut hops {
                hop.origin_id.make_ascii_lowercase();
            }
            validate_route(&own, &origin, &hops)?;
            let transcript = if let Some(first) = hops.first() {
                let host = remote::Host::from_route_hop(first, home);
                let mut session = SessionSummary::new(
                    id.clone(),
                    source,
                    time::OffsetDateTime::UNIX_EPOCH,
                    llm_sessions::Locator::File {
                        path: PathBuf::new(),
                    },
                );
                session.origin_id = Some(origin.clone());
                session.origin_label = Some(hops.last().expect("nonempty route").name.clone());
                session.routes = vec![AccessRoute { hops }];
                host.open(&session, home)?
            } else {
                let sources: Vec<_> = source::all(home)
                    .into_iter()
                    .filter(|adapter| adapter.id() == source)
                    .collect();
                let adapter = source::find(&sources, source).ok_or(Error::Unsupported {
                    agent: source,
                    what: "reading transcripts",
                })?;
                let scan = scan_with(&sources, None, None, &[]);
                let session = scan
                    .sessions
                    .iter()
                    .find(|session| session.source == source && session.id == *id)
                    .ok_or_else(|| Error::MissingRow {
                        db: PathBuf::from("<local source>"),
                        id: id.clone(),
                    })?;
                let mut transcript = adapter.open(&session.locator)?;
                transcript.title = transcript.title.or_else(|| session.title.clone());
                transcript.workspace = session.workspace.clone().or(transcript.workspace);
                transcript
            };
            emit(&serde_json::to_string(&transcript)?);
        }
        FederationCommand::Top => {
            // A leaf operation: ordinary `top` recursively contacts peers.
            let fleet = Fleet::probe();
            let sources = source::all(home);
            let scan = scan_with(&sources, None, Some(&fleet), &[]);
            let rows: Vec<_> = fleet
                .processes()
                .iter()
                .map(|process| {
                    let session = scan
                        .sessions
                        .iter()
                        .find(|s| s.liveness.pid() == Some(process.pid));
                    serde_json::json!({
                        "pid": process.pid,
                        "agent": process.agent,
                        "cwd": process.cwd,
                        "activity": match process.activity {
                            llm_sessions::runtime::live::Activity::Working => "working",
                            llm_sessions::runtime::live::Activity::Waiting => "waiting",
                            llm_sessions::runtime::live::Activity::Unknown => "unknown",
                        },
                        "pane": process.pane.as_ref().map(|pane| &pane.address),
                        "session": session.map(|s| &s.id),
                        "title": session.and_then(|s| s.title.as_ref()),
                        "exact": session.is_some_and(|s| matches!(s.liveness, llm_sessions::model::Liveness::Live { .. })),
                    })
                })
                .collect();
            emit(&serde_json::to_string(&rows)?);
        }
    }
    Ok(())
}

fn validate_route(own: &str, target: &str, hops: &[RouteHop]) -> Result<()> {
    let mut seen = std::collections::HashSet::from([own]);
    let valid = valid_origin(target)
        && hops.len() <= llm_sessions::federation::DEFAULT_HOPS as usize
        && hops.iter().all(|hop| {
            valid_origin(&hop.origin_id) && hop.reachable() && seen.insert(hop.origin_id.as_str())
        })
        && hops
            .last()
            .map_or(own == target, |hop| hop.origin_id == target);
    if !valid {
        return Err(Error::BadArgument {
            what: "federation route",
            got: "route must be acyclic and end at the requested origin".into(),
        });
    }
    Ok(())
}
