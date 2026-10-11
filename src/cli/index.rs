//! The body index: building it (`index`) and querying it (`search`).

use std::path::Path;

use llm_sessions::catalog::workspace;
use llm_sessions::storage::index::{Index, RefreshOptions};
use llm_sessions::{Result, SessionSummary};

use super::args::{IndexArgs, SearchArgs};
use super::{Cached, Liveliness, Loaded, Remote, emit, parse_since, scan_all_with};

pub(super) fn build(home: &Path, args: &IndexArgs) -> Result<()> {
    let path = Index::default_path(home);
    if args.rebuild && path.exists() {
        std::fs::remove_file(&path).map_err(llm_sessions::error::io(&path))?;
    }

    if args.stats {
        let index = Index::open(&path)?;
        let stats = index.stats()?;
        emit(&format!(
            "{} document(s), {:.1} MiB at {}",
            stats.docs,
            stats.bytes as f64 / (1024.0 * 1024.0),
            workspace::display(&path, home)
        ));
        if let (Some(oldest), Some(newest)) = (stats.oldest, stats.newest) {
            emit(&format!("covering {} to {}", oldest.date(), newest.date()));
        }
        return Ok(());
    }

    if args.vacuum {
        let mut index = Index::open(&path)?;
        let reclaimed = index.vacuum()?;
        emit(&format!(
            "reclaimed {:.1} MiB",
            reclaimed as f64 / (1024.0 * 1024.0)
        ));
        return Ok(());
    }

    let options = RefreshOptions {
        since: args.since.as_deref().map(parse_since).transpose()?,
        max_body: args.max_body,
        max_bytes: args.max_bytes,
    };

    let Loaded { loader, scan, .. } =
        scan_all_with(home, Liveliness::Skip, Cached::Ignore, Remote::Inline);
    // Remote bodies cost one ssh per session and belong to another machine's
    // index; only rows on this machine are cacheable here.
    let local: Vec<SessionSummary> = scan
        .sessions
        .iter()
        .filter(|session| session.host.is_none())
        .cloned()
        .collect();
    let mut index = Index::open(&path)?;
    let stats = index.refresh_with(&local, &loader.sources, &options)?;
    emit(&format!(
        "indexed {} of {} session(s), {} removed, {} unreadable, in {:.1?}",
        stats.reindexed, stats.scanned, stats.removed, stats.failed, stats.elapsed
    ));
    emit(&format!(
        "{:.1} MiB at {}",
        index.size_bytes().unwrap_or(0) as f64 / (1024.0 * 1024.0),
        workspace::display(&path, home)
    ));
    Ok(())
}

pub(super) fn search(home: &Path, args: &SearchArgs) -> Result<()> {
    let index = Index::open(&Index::default_path(home))?;
    let hits = index.search(&args.query.join(" "), args.limit)?;
    if args.json {
        let rows: Vec<_> = hits
            .iter()
            .map(|h| {
                serde_json::json!({
                    "source": h.source.name(),
                    "session_id": h.session_id,
                    "title": h.title,
                    "workspace": h.workspace,
                    "snippet": h.snippet,
                })
            })
            .collect();
        emit(&serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if hits.is_empty() {
        eprintln!("llm-sessions: no matches (run `llm-sessions index` if you have not yet)");
    }
    for hit in hits {
        emit(&format!(
            "{}  {}  {}",
            hit.source.badge(),
            hit.workspace
                .as_ref()
                .map_or_else(|| "?".to_string(), |p| workspace::display(p, home)),
            hit.title.as_deref().unwrap_or("—")
        ));
        emit(&format!("      {}", hit.snippet.replace('\n', " ")));
    }
    Ok(())
}
