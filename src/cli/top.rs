use std::path::Path;

use llm_sessions::catalog::workspace;
use llm_sessions::model::Liveness;
use llm_sessions::output::reader::truncate;
use llm_sessions::runtime::live::{Activity, Process};
use llm_sessions::{Result, SessionSummary};

use super::args::TopArgs;
use super::{Liveliness, Loaded, emit, scan_all};

/// One row per running agent process.
///
/// Liveness exists as a column inside a list sorted by something else; when
/// eight agents are running the question is "who needs me", not "what
/// happened in this project".
pub(super) fn run(home: &Path, args: &TopArgs) -> Result<()> {
    let Loaded {
        loader,
        fleet,
        scan,
    } = scan_all(home, Liveliness::Needed);
    let hosts = &loader.hosts;
    let attached: Vec<(&Process, Option<&SessionSummary>)> = fleet
        .processes()
        .iter()
        .map(|process| {
            let session = scan
                .sessions
                .iter()
                .find(|s| s.is_local() && s.liveness.pid() == Some(process.pid));
            (process, session)
        })
        .collect();

    if args.json {
        let rows: Vec<_> = attached
            .iter()
            .map(|(process, session)| {
                serde_json::json!({
                    "pid": process.pid,
                    "agent": process.agent.name(),
                    "pane": process.pane.as_ref().map(|p| p.address.clone()),
                    "cwd": process.cwd,
                    "activity": match process.activity {
                        Activity::Working => "working",
                        Activity::Waiting => "waiting",
                        Activity::Unknown => "unknown",
                    },
                    "session": session.map(|s| s.id.clone()),
                    "exact": session.is_some_and(|s| matches!(s.liveness, Liveness::Live { .. })),
                    "title": session.and_then(|s| s.title.clone()),
                })
            })
            .collect();
        let mut remote_rows: Vec<_> = Vec::new();
        for host in hosts {
            let origin = host.origin_id().ok();
            match host.top() {
                Ok(processes) => remote_rows.extend(processes.iter().map(|process| {
                    serde_json::json!({
                        "host": host.name(),
                        "origin_id": origin,
                        "pid": process.pid,
                        "agent": process.agent.name(),
                        "pane": process.pane,
                        "cwd": process.cwd,
                        "activity": match process.activity {
                            Activity::Working => "working",
                            Activity::Waiting => "waiting",
                            Activity::Unknown => "unknown",
                        },
                        "session": process.session,
                        "exact": process.exact,
                        "title": process.title,
                    })
                })),
                // One machine's absence must not hide the others' rows.
                Err(e) => eprintln!("llm-sessions: {e}"),
            }
        }
        let mut rows = rows;
        rows.extend(remote_rows);
        emit(&serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if attached.is_empty() && hosts.is_empty() {
        eprintln!("llm-sessions: no agent processes");
        return Ok(());
    }
    for (process, session) in attached {
        let place = process
            .cwd
            .as_deref()
            .map_or_else(|| "?".into(), |path| workspace::display(path, home));
        emit(&format!(
            "{} {:>7} {:<8} {:<34} {:<8} {}",
            session.map_or(" ", |s| s.liveness.badge()),
            process.pid,
            process.agent.name(),
            truncate(&place, 34),
            match process.activity {
                Activity::Working => "working",
                Activity::Waiting => "waiting",
                Activity::Unknown => "",
            },
            process
                .pane
                .as_ref()
                .map(|pane| format!("[{}] ", pane.address))
                .unwrap_or_default()
                + session.map_or("—", SessionSummary::display_title),
        ));
    }
    for host in hosts {
        match host.top() {
            Ok(processes) if processes.is_empty() => {}
            Ok(processes) => {
                emit(&format!("── {} ", host.name()));
                let remote_home = host.home().map(Path::to_path_buf);
                for process in processes {
                    // A remote `~` is that machine's home, not ours; contract
                    // against it when the probe learned it.
                    let place = process.cwd.as_deref().map(|path| match &remote_home {
                        Some(root) if path.starts_with(root) => {
                            let rest = path.strip_prefix(root).unwrap_or(path);
                            format!("~{}", rest.display())
                        }
                        _ => path.display().to_string(),
                    });
                    emit(&format!(
                        "{} {:>7} {:<8} {:<34} {:<8} {}",
                        if process.exact { "\u{25cf}" } else { " " },
                        process.pid,
                        process.agent.name(),
                        truncate(
                            &format!("{}:{}", host.name(), place.unwrap_or_else(|| "?".into())),
                            34
                        ),
                        match process.activity {
                            Activity::Working => "working",
                            Activity::Waiting => "waiting",
                            Activity::Unknown => "",
                        },
                        process
                            .pane
                            .as_deref()
                            .map(|pane| format!("[{pane}] "))
                            .unwrap_or_default()
                            + process
                                .title
                                .as_deref()
                                .unwrap_or(process.session.as_deref().unwrap_or("—"),),
                    ));
                }
            }
            Err(e) => eprintln!("llm-sessions: {e}"),
        }
    }
    Ok(())
}
