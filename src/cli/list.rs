//! Session listings: `list` rows and `workspaces` totals, both filtered by
//! the same query and visibility rules the TUI uses.

use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use llm_sessions::catalog::loader::body_hits;
use llm_sessions::catalog::query::{Filters, Query, eligible};
use llm_sessions::catalog::workspace;
use llm_sessions::output::reader::truncate;
use llm_sessions::output::report::{expand, format_row};
use llm_sessions::{Result, Scan, SessionSummary, SourceId};

use super::args::{FilterArgs, ListArgs, WorkspacesArgs};
use super::{Liveliness, Loaded, emit, report_failures, scan_all};

pub(super) fn run(home: &Path, args: &ListArgs) -> Result<()> {
    let Loaded { fleet, scan, .. } = scan_all(home, Liveliness::Needed);
    let rows = filter(home, &scan, &args.filters, args.limit);
    if args.json {
        let json = serde_json::to_string_pretty(&rows)?;
        emit(&json);
    } else if let Some(template) = &args.format {
        // A template is for machines, so it goes out raw: no truncation, no
        // padding, and `-0` for arguments that may contain whitespace.
        let mut out = std::io::stdout().lock();
        for row in &rows {
            let line = expand(template, row, home, &fleet);
            let end = if args.null { '\0' } else { '\n' };
            if write!(out, "{line}{end}").is_err() {
                return Ok(());
            }
        }
    } else {
        for row in &rows {
            emit(&format_row(row, home, &fleet));
        }
    }
    report_failures(&scan);
    Ok(())
}

pub(super) fn workspaces(home: &Path, args: &WorkspacesArgs) -> Result<()> {
    let Loaded { scan, .. } = scan_all(home, Liveliness::Needed);
    let rows = filter(home, &scan, &args.filters, 0);

    let mut groups: BTreeMap<Option<&PathBuf>, Vec<&SessionSummary>> = BTreeMap::new();
    for row in &rows {
        groups.entry(row.workspace.as_ref()).or_default().push(row);
    }
    let mut ordered: Vec<_> = groups.into_iter().collect();
    // Somewhere with an agent attached comes first; then plain recency.
    ordered.sort_by_key(|(_, rows)| {
        let running = rows.iter().any(|r| r.liveness.is_running());
        (
            std::cmp::Reverse(running),
            std::cmp::Reverse(rows[0].updated),
        )
    });

    if args.limit > 0 {
        ordered.truncate(args.limit);
    }
    let summaries: Vec<_> = ordered
        .into_iter()
        .map(|(path, rows)| {
            let mut agents: Vec<_> = rows.iter().map(|r| r.source.to_string()).collect();
            agents.sort_unstable();
            agents.dedup();
            WorkspaceSummary {
                workspace: path.map(PathBuf::as_path),
                count: rows.len(),
                live: rows.iter().filter(|r| r.liveness.is_running()).count(),
                agents,
                cost: rows.iter().filter_map(|r| r.usage).map(|u| u.cost).sum(),
            }
        })
        .collect();

    if args.json {
        emit(&serde_json::to_string_pretty(&summaries)?);
    } else {
        for summary in summaries {
            let WorkspaceSummary {
                workspace: path,
                count,
                live,
                agents,
                cost,
            } = summary;
            let label = path.map_or_else(
                || "(unattributed)".to_string(),
                |p| workspace::display(p, home),
            );
            emit(&format!(
                "{} {:>4}  {:<50}  {:<28}{}",
                if live > 0 { "●" } else { " " },
                count,
                truncate(&label, 50),
                agents.join(" "),
                if cost > 0.0 {
                    format!("  ${cost:.2}")
                } else {
                    String::new()
                },
            ));
        }
    }
    report_failures(&scan);
    Ok(())
}

#[derive(serde::Serialize)]
struct WorkspaceSummary<'a> {
    workspace: Option<&'a Path>,
    count: usize,
    live: usize,
    agents: Vec<String>,
    cost: f64,
}

/// Sessions matching the query's `body:` terms, or `None` when it has none.
///
/// Without the index a `body:` query narrows to nothing, said on stderr,
/// rather than silently matching everything.
fn body_restriction(home: &Path, query: &Query) -> Option<HashSet<(SourceId, String)>> {
    match body_hits(home, query)? {
        Ok(hits) => Some(hits),
        Err(e) => {
            eprintln!("llm-sessions: body: search failed: {e}; run `llm-sessions index`");
            Some(HashSet::new())
        }
    }
}

fn filter(home: &Path, scan: &Scan, args: &FilterArgs, limit: usize) -> Vec<SessionSummary> {
    let wanted: Vec<String> = args.sources.iter().map(|s| s.to_lowercase()).collect();
    let prefix = args
        .workspace
        .as_ref()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()));
    let query = Query::parse(args.query.as_deref().unwrap_or_default());
    let bodies = body_restriction(home, &query);
    // The archived / live / query gate is the library's, shared with the
    // TUI's toggles, so the two frontends cannot drift on what those mean.
    let filters = Filters {
        query: &query,
        hits: bodies.as_ref(),
        show_archived: args.archived,
        live_only: args.live,
    };

    scan.sessions
        .iter()
        // A subagent transcript is a child of a session, never a row of its
        // own; `llm-sessions show <id>` still reaches one directly.
        .filter(|s| !s.subagent)
        .filter(|s| eligible(s, &filters))
        .filter(|s| wanted.is_empty() || wanted.contains(&s.source.to_string()))
        .filter(|s| {
            args.hosts.is_empty()
                || args
                    .hosts
                    .contains(&s.host.clone().unwrap_or_else(|| "local".into()))
        })
        .filter(|s| match (&prefix, &s.workspace) {
            (None, _) => true,
            (Some(p), Some(w)) => w.starts_with(p),
            (Some(_), None) => false,
        })
        .take(if limit == 0 { usize::MAX } else { limit })
        .cloned()
        .collect()
}
