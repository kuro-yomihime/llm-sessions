//! Where sessions come from: this machine's adapters (`sources`) and remote
//! machines (`hosts`).

use std::path::Path;
use std::time::Instant;

use llm_sessions::Result;
use llm_sessions::catalog::workspace;
use llm_sessions::error::Error;
use llm_sessions::source::{self, remote};

use super::emit;

/// One line per local store: sessions found (or the error), what the scan
/// cost, and its roots. Remote machines are `hosts`, which probes them.
pub(super) fn sources(home: &Path, json: bool) -> Result<()> {
    let mut rows = Vec::new();
    for src in source::all(home) {
        let started = Instant::now();
        let outcome = src.list();
        let elapsed = started.elapsed();
        if json {
            let (sessions, error) = match &outcome {
                Ok(found) => (Some(found.len()), None),
                Err(e) => (None, Some(e.to_string())),
            };
            rows.push(serde_json::json!({
                "agent": src.id().name(),
                "sessions": sessions,
                "error": error,
                "seconds": elapsed.as_secs_f64(),
                "roots": src.roots().iter().map(|root| serde_json::json!({
                    "path": root,
                    "exists": root.exists(),
                })).collect::<Vec<_>>(),
            }));
            continue;
        }
        let status = match &outcome {
            Ok(found) => format!("{} sessions", found.len()),
            Err(e) => format!("error: {e}"),
        };
        let roots: Vec<String> = src
            .roots()
            .iter()
            .map(|r| {
                let mark = if r.exists() { "" } else { " (absent)" };
                format!("{}{mark}", workspace::display(r, home))
            })
            .collect();
        emit(&format!(
            "{:<10} {:>16}  {:>9.1?}  {}",
            src.id().to_string(),
            status,
            elapsed,
            roots.join(", ")
        ));
    }
    if json {
        emit(&serde_json::to_string_pretty(&rows)?);
    }
    Ok(())
}

/// Show every configured host and probe it: name, destination, scanned
/// agents, pinned origin, transport.
pub(super) fn list(home: &Path, json: bool) -> Result<()> {
    let (config, failure) = remote::Config::load(home);
    if config.hosts.is_empty() && !json {
        let path = remote::Config::default_path(home);
        emit(&format!(
            "no remote hosts; add one with `llm-sessions hosts add <name> <ssh>` or `hosts add <name> --url <URL>` ({})",
            workspace::display(&path, home)
        ));
    }
    let mut rows = Vec::new();
    for entry in &config.hosts {
        let origin = entry.origin_id.clone();
        let host = remote::Host::new(entry.clone());
        let status = host.status();
        if json {
            rows.push(serde_json::json!({
                "name": host.name(),
                "destination": host.destination(),
                "sources": host.configured_sources(),
                "origin_id": origin,
                "status": status,
            }));
            continue;
        }
        let configured = host.configured_sources();
        let scanned = if configured.is_empty() {
            "all".to_string()
        } else {
            configured.join(",")
        };
        emit(&format!(
            "{:<12} {:<24} {:<10} {:<36} {status}",
            host.name(),
            host.destination(),
            scanned,
            origin.as_deref().unwrap_or("unpinned"),
        ));
    }
    if json {
        emit(&serde_json::to_string_pretty(&rows)?);
    }
    if let Some(failure) = failure {
        eprintln!("llm-sessions: {failure}");
    }
    Ok(())
}

/// Add or update a host from the `hosts add` fields, applying the same
/// rules `Config::load` does so a saved entry always loads.
pub(super) fn add(home: &Path, args: super::args::AddHostArgs) -> Result<()> {
    let super::args::AddHostArgs {
        name,
        ssh,
        url,
        home: remote_home,
        token_env,
        sources: wanted,
        mode,
    } = args;
    let wanted: Vec<String> = wanted.iter().map(|id| id.name().to_owned()).collect();
    if name.trim().is_empty() {
        return Err(Error::BadArgument {
            what: "name",
            got: name.clone(),
        });
    }
    let entry = match remote::Transport::from_parts(ssh, url, remote_home, token_env) {
        Ok(transport) => remote::HostConfig {
            name,
            transport,
            origin_id: None,
            sources: wanted,
            mode: mode.unwrap_or_default(),
        },
        Err(problem) => {
            return Err(Error::BadArgument {
                what: "hosts add",
                got: problem,
            });
        }
    };
    if let Some(problem) = entry.problem() {
        return Err(Error::BadArgument {
            what: "hosts add",
            got: problem,
        });
    }
    let (mut config, failure) = remote::Config::load(home);
    if let Some(failure) = failure {
        eprintln!("llm-sessions: {failure} (rewriting the file)");
    }
    let updated = config.hosts.iter().any(|kept| kept.name == entry.name);
    config.hosts.retain(|kept| kept.name != entry.name);
    let name = entry.name.clone();
    config.hosts.push(entry);
    config.save(home)?;
    emit(&format!(
        "{} {} ({} hosts total)",
        if updated { "updated" } else { "added" },
        name,
        config.hosts.len()
    ));
    Ok(())
}

pub(super) fn remove(home: &Path, name: &str) -> Result<()> {
    let (mut config, _) = remote::Config::load(home);
    let before = config.hosts.len();
    config.hosts.retain(|kept| kept.name != name);
    if config.hosts.len() == before {
        emit(&format!("{name} was not configured"));
        return Ok(());
    }
    config.save(home)?;
    emit(&format!("removed {name}"));
    Ok(())
}

/// Pin the peer's origin as observed through its connection. With `init`,
/// first create the peer's marker on it — recovering a peer whose own
/// `llm-sessions` predates origin identity — and pin what resulted.
pub(super) fn refresh(home: &Path, name: &str, init: bool, force: bool) -> Result<()> {
    let (mut config, failure) = remote::Config::load(home);
    if let Some(failure) = failure {
        return Err(Error::BadArgument {
            what: "hosts.json",
            got: failure,
        });
    }
    let entry = config
        .hosts
        .iter_mut()
        .find(|host| host.name == name)
        .ok_or_else(|| Error::BadArgument {
            what: "host name",
            got: name.to_owned(),
        })?;
    let peer = remote::Host::new(entry.clone());
    let observed = if init {
        peer.init_origin(force)?
    } else {
        peer.observed_origin()?.to_owned()
    };
    let previous = entry.origin_id.replace(observed.clone());
    config.save(home)?;
    emit(&format!(
        "{name}: {} -> {observed}",
        previous.as_deref().unwrap_or("unregistered")
    ));
    Ok(())
}
