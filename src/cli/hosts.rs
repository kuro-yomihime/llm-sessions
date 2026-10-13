//! Where sessions come from: local adapters (`sources`) and remote machines
//! (`hosts`).

use std::io::Write;
use std::path::Path;
use std::time::Instant;

use llm_sessions::catalog::workspace;
use llm_sessions::error::Error;
use llm_sessions::source::{self, remote};
use llm_sessions::{Result, SourceId};

use super::emit;

pub(super) fn sources(home: &Path) {
    let mut stdout = std::io::stdout().lock();
    for src in source::all(home) {
        let started = Instant::now();
        let outcome = src.list();
        let elapsed = started.elapsed();
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
        let _ = writeln!(
            stdout,
            "{:<10} {:>16}  {:>9.1?}  {}",
            src.id().to_string(),
            status,
            elapsed,
            roots.join(", ")
        );
    }
    // One line per configured machine: which transport it will use, or why
    // it cannot be reached. Probing is one `ssh` per host, which is exactly
    // what `sources` exists to spend.
    let (config, parse_failure) = remote::Config::load(home);
    for host in config.hosts {
        let host = remote::Host::new(host);
        let _ = writeln!(
            stdout,
            "{:<10} {:>16}  {:>9}  {}",
            host.name(),
            host.destination(),
            "",
            host.status()
        );
    }
    if let Some(failure) = parse_failure {
        let _ = writeln!(
            stdout,
            "{:<10} {:>16}  {:>9}  {failure}",
            "hosts.json", "", ""
        );
    }
}

/// Show every configured host and probe it: name, destination, transport.
pub(super) fn list(home: &Path) {
    let (config, failure) = remote::Config::load(home);
    if config.hosts.is_empty() {
        let path = remote::Config::default_path(home);
        emit(&format!(
            "no remote hosts; add one with `llm-sessions hosts add <name> <ssh>` or `hosts add <name> --url <URL>` ({})",
            workspace::display(&path, home)
        ));
        if let Some(failure) = failure {
            eprintln!("llm-sessions: {failure}");
        }
        return;
    }
    for host in &config.hosts {
        let host = remote::Host::new(host.clone());
        let configured = host.configured_sources();
        let scanned = if configured.is_empty() {
            "all".to_string()
        } else {
            configured.join(",")
        };
        emit(&format!(
            "{:<12} {:<24} {:<10} {}",
            host.name(),
            host.destination(),
            scanned,
            host.status()
        ));
    }
    if let Some(failure) = failure {
        eprintln!("llm-sessions: {failure}");
    }
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
    // Unknown agent names cannot match a store, so they are rejected here
    // rather than silently listing nothing.
    for agent in &wanted {
        if SourceId::parse(agent).is_none() {
            return Err(Error::BadArgument {
                what: "--source",
                got: agent.clone(),
            });
        }
    }
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
