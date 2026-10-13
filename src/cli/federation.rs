//! Read-only federation protocol. Discovery context crosses every SSH hop.

use std::path::{Path, PathBuf};

use llm_sessions::catalog::loader::{guarded_scan, open};
use llm_sessions::error::Error;
use llm_sessions::federation::{
    Capabilities, FederationScan, Guard, read_origin, valid_origin, valid_route,
};
use llm_sessions::model::{AccessRoute, RouteHop, normalize_origin};
use llm_sessions::runtime::live::Fleet;
use llm_sessions::source::{self, remote};
use llm_sessions::{Result, SessionSummary, SourceId, scan_with};

use super::args::FederationCommand;
use super::emit;

pub(super) fn run(home: &Path, command: &FederationCommand) -> Result<()> {
    match command {
        FederationCommand::Capabilities => {
            emit(&serde_json::to_string(&Capabilities::own(home)?)?);
        }
        FederationCommand::List { path, hops } => {
            let guard = Guard::new(path, *hops).map_err(|_| Error::BadArgument {
                what: "federation path",
                got: path.clone(),
            })?;
            let origin = read_origin(home)?;
            if guard.visited.contains(&origin) {
                emit(&serde_json::to_string(&FederationScan::default())?);
                return Ok(());
            }
            let sources = source::all(home);
            let (config, failure) = remote::Config::load(home);
            let hosts: Vec<_> = config.hosts.into_iter().map(remote::Host::new).collect();
            let mut scan = guarded_scan(home, &sources, &hosts, &origin, &guard);
            if let Some(failure) = failure {
                scan.failures.push(failure);
            }
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
            let origin = normalize_origin(origin);
            for hop in &mut hops {
                hop.origin_id = normalize_origin(&hop.origin_id);
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
                let scan = scan_with(&sources, None, None);
                let session = scan
                    .sessions
                    .iter()
                    .find(|session| session.source == source && session.id == *id)
                    .ok_or_else(|| Error::MissingRow {
                        db: PathBuf::from("<local source>"),
                        id: id.clone(),
                    })?;
                open(home, &sources, session)?
            };
            emit(&serde_json::to_string(&transcript)?);
        }
        FederationCommand::Top => {
            // A leaf operation: ordinary `top` recursively contacts peers.
            let fleet = Fleet::probe();
            let sources = source::all(home);
            let scan = scan_with(&sources, None, Some(&fleet));
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

/// A route this machine received, as the `show` endpoint may travel it.
///
/// [`valid_route`] states the protocol's own rule; the receiving endpoint
/// adds what only it knows: the route must never pass back through this
/// machine, and an empty route is the local read of this machine's own row.
fn validate_route(own: &str, target: &str, hops: &[RouteHop]) -> Result<()> {
    let valid = valid_origin(target)
        && !hops.iter().any(|hop| hop.origin_id == own)
        && if hops.is_empty() {
            own == target
        } else {
            valid_route(hops, target)
        };
    if !valid {
        return Err(Error::BadArgument {
            what: "federation route",
            got: "route must be acyclic and end at the requested origin".into(),
        });
    }
    Ok(())
}
