//! Read-only reports over one scan: `digest`, `usage`, and `doctor`.

use std::path::Path;

use llm_sessions::catalog::workspace;
use llm_sessions::output::reader::truncate;
use llm_sessions::output::report::date;
use llm_sessions::output::{digest, usage};
use llm_sessions::runtime::doctor::{self, Severity};
use llm_sessions::storage::index::Index;
use llm_sessions::{Result, Usage};

use super::args::{DigestArgs, DoctorArgs, UsageArgs};
use super::{Liveliness, Loaded, emit, parse_since, scan_all};

/// One line per session touched since `--since`, grouped by workspace.
///
/// Prompts come from the index; sessions that were never indexed fall back to
/// their tier-1 title rather than parsing a body, because a digest that takes
/// half a minute is a digest nobody runs.
pub(super) fn digest(home: &Path, args: &DigestArgs) -> Result<()> {
    let Loaded { scan, .. } = scan_all(home, Liveliness::Skip);
    let since = parse_since(&args.since)?;
    let index = Index::open(&Index::default_path(home))?;
    let groups = digest::build(&index, &scan.sessions, since)?;

    if args.json {
        let payload: Vec<_> = groups
            .iter()
            .map(|(workspace, entries)| {
                serde_json::json!({
                    "workspace": workspace,
                    "sessions": entries.iter().map(|entry| serde_json::json!({
                        "source": entry.source.name(),
                        "session_id": entry.session_id,
                        "updated": entry.updated.to_string(),
                        "title": entry.title,
                        "prompt": entry.prompt,
                        "cost": entry.usage.map(|u| u.cost),
                        "tokens_input": entry.usage.map(|u| u.input),
                        "tokens_output": entry.usage.map(|u| u.output),
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        emit(&serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    if groups.is_empty() {
        eprintln!("llm-sessions: nothing since {since}");
        return Ok(());
    }
    for (workspace, entries) in groups {
        let label = workspace.as_deref().map_or_else(
            || "(unattributed)".to_string(),
            |path| workspace::display(path, home),
        );
        emit(&format!("{label}  ({} session(s))", entries.len()));
        for entry in entries {
            let text = entry
                .prompt
                .as_deref()
                .or(entry.title.as_deref())
                .unwrap_or("—");
            emit(&format!(
                "  {} {}  {}{}",
                entry.source.badge(),
                date(entry.updated),
                truncate(text, 96),
                entry
                    .usage
                    .filter(|usage| usage.cost > 0.0)
                    .map(|usage| format!("  ${:.2}", usage.cost))
                    .unwrap_or_default(),
            ));
        }
        emit("");
    }
    Ok(())
}

/// Recorded spend, grouped on one axis.
pub(super) fn usage(home: &Path, args: &UsageArgs) -> Result<()> {
    let since = args.since.as_deref().map(parse_since).transpose()?;

    let Loaded { scan, .. } = scan_all(home, Liveliness::Skip);
    let index = Index::open(&Index::default_path(home))?;
    let mut rows = usage::report(&index, &scan.sessions, args.by, since)?;
    if args.limit > 0 {
        rows.truncate(args.limit);
    }

    if args.json {
        let payload: Vec<_> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "key": row.key,
                    "sessions": row.sessions,
                    "priced": row.priced,
                    "cost": row.usage.cost,
                    "tokens_input": row.usage.input,
                    "tokens_output": row.usage.output,
                })
            })
            .collect();
        emit(&serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    let mut total = Usage::default();
    for row in &rows {
        // `priced` distinguishes "spent nothing" from "records nothing"; a
        // blank cost column is the honest rendering of the latter.
        let cost = if row.priced == 0 {
            "        —".to_string()
        } else {
            format!("{:>9.2}", row.usage.cost)
        };
        emit(&format!(
            "{cost}  {:>6} sess  {:>4} priced  {:>9} in  {:>9} out  {}",
            row.sessions,
            row.priced,
            row.usage.input,
            row.usage.output,
            truncate(&row.key, 60),
        ));
        total.input += row.usage.input;
        total.output += row.usage.output;
        total.cost += row.usage.cost;
    }
    emit(&format!(
        "{:>9.2}  {:>6} rows                  {:>9} in  {:>9} out  total",
        total.cost,
        rows.len(),
        total.input,
        total.output
    ));
    Ok(())
}

/// Health report. Never fixes anything: it prints the commands and leaves the
/// decision with the user.
pub(super) fn doctor(home: &Path, args: &DoctorArgs) -> Result<()> {
    let Loaded { scan, .. } = scan_all(home, Liveliness::Skip);
    let mut findings = doctor::run(home, &scan.sessions);
    findings.extend(doctor::remote_findings(home));
    if args.json {
        let payload: Vec<_> = findings
            .iter()
            .map(|finding| {
                serde_json::json!({
                    "severity": severity_name(finding.severity),
                    "check": finding.check,
                    "detail": finding.detail,
                    "fix": finding.fix,
                })
            })
            .collect();
        emit(&serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    if findings.is_empty() {
        emit("nothing to report");
        return Ok(());
    }
    for finding in &findings {
        let mark = severity_name(finding.severity);
        emit(&format!(
            "{mark}  {:<18}  {}",
            finding.check, finding.detail
        ));
        if let Some(fix) = &finding.fix {
            emit(&format!("                          $ {fix}"));
        }
    }
    let warnings = findings
        .iter()
        .filter(|f| f.severity == Severity::Warn)
        .count();
    emit("");
    emit(&format!(
        "{} finding(s), {warnings} warning(s)",
        findings.len()
    ));
    Ok(())
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Warn => "warn",
        Severity::Info => "info",
    }
}
