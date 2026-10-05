//! Rows a person reads: the `list` line and the `{field}` template.
//!
//! One home for presenting a [`SessionSummary`] as terminal text, so the
//! column set and the template spellings are testable without running the
//! binary and shared by every printing subcommand.

use std::path::Path;

use crate::catalog::workspace;
use crate::model::{Liveness, PathTrust, SessionSummary};
use crate::output::reader::truncate;
use crate::runtime::live::{Activity, Fleet};

/// Substitute `{field}` placeholders. An unknown field expands to empty
/// rather than erroring, so a template stays usable as agents gain columns.
pub fn expand(template: &str, row: &SessionSummary, home: &Path, fleet: &Fleet) -> String {
    let process = row.liveness.pid().and_then(|pid| fleet.get(pid));
    let mut out = String::with_capacity(template.len() + 64);
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}').map(|i| start + i) else {
            break;
        };
        let field = &rest[start + 1..end];
        out.push_str(&match field {
            "liveness" => row.liveness.badge().to_string(),
            "host" => row.host.clone().unwrap_or_else(|| "local".into()),
            "pid" => process.map(|p| p.pid.to_string()).unwrap_or_default(),
            "pane" => process
                .and_then(|p| p.pane.as_ref())
                .map(|pane| pane.address.clone())
                .unwrap_or_default(),
            "activity" => process
                .map(|p| match p.activity {
                    Activity::Working => "working",
                    Activity::Waiting => "waiting",
                    Activity::Unknown => "",
                })
                .unwrap_or_default()
                .to_string(),
            "source" | "agent" => row.source.name().to_string(),
            "id" => row.id.clone(),
            "title" | "label" => row.display_title().to_string(),
            "workspace" => row
                .workspace
                .as_deref()
                .map(|p| workspace::display(p, home))
                .unwrap_or_default(),
            "branch" => row.branch.clone().unwrap_or_default(),
            "model" => row.model.clone().unwrap_or_default(),
            "cost" => row
                .usage
                .map(|u| format!("{:.4}", u.cost))
                .unwrap_or_default(),
            "tokens" => row
                .usage
                .map(|u| (u.input + u.output).to_string())
                .unwrap_or_default(),
            "files" => row.change.map(|c| c.files.to_string()).unwrap_or_default(),
            "updated" => row.updated.unix_timestamp().to_string(),
            "path" => row.store_path().display().to_string(),
            "n" => "\n".to_string(),
            "t" => "\t".to_string(),
            _ => String::new(),
        });
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out.replace("\\t", "\t").replace("\\n", "\n")
}

pub fn format_row(row: &SessionSummary, home: &Path, fleet: &Fleet) -> String {
    let place = row
        .workspace
        .as_ref()
        .map_or_else(|| "?".to_string(), |p| workspace::display(p, home));
    let uncertain = if row.trust == PathTrust::Decoded {
        "~"
    } else {
        " "
    };
    format!(
        "{} {}  {}  {}{:<34}  {}{}",
        row.liveness.badge(),
        row.source.badge(),
        date(row.updated),
        uncertain,
        truncate(&place, 34),
        truncate(row.display_title(), 60),
        attachment_suffix(row, fleet),
    )
}

/// Where a running session is, and what it is doing. Empty when nothing is
/// attached, so an archive listing stays as terse as it was.
pub fn attachment_suffix(row: &SessionSummary, fleet: &Fleet) -> String {
    let Some(process) = row.liveness.pid().and_then(|pid| fleet.get(pid)) else {
        return String::new();
    };
    // A tier-2 guess is marked as one; a bare pid would read as certainty.
    let certainty = match row.liveness {
        Liveness::Live { .. } => "",
        _ => "?",
    };
    let pane = process
        .pane
        .as_ref()
        .map(|pane| format!(" [{}]", pane.address))
        .unwrap_or_default();
    let activity = match process.activity {
        Activity::Working => " working",
        Activity::Waiting => " waiting",
        Activity::Unknown => "",
    };
    format!("  pid {}{certainty}{pane}{activity}", process.pid)
}

/// `2026-09-21 13:45`, on the reader's own clock. Machine-readable output
/// keeps UTC; this is the column a person reads.
pub fn date(t: time::OffsetDateTime) -> String {
    let local = crate::clock::at(t, crate::clock::offset());
    let (h, m, _) = local.time().as_hms();
    format!("{} {h:02}:{m:02}", local.date())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Locator, SourceId};

    fn row() -> SessionSummary {
        let mut row = SessionSummary::new(
            "abc".into(),
            SourceId::Omp,
            time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
            Locator::File {
                path: "/x.jsonl".into(),
            },
        );
        row.workspace = Some("/w/proj".into());
        row.title = Some("Fix the login".into());
        row
    }

    #[test]
    fn a_row_renders_its_columns() {
        let line = format_row(&row(), Path::new("/w"), &Fleet::default());
        assert!(line.contains("Fix the login"), "{line}");
        assert!(line.contains("proj"), "{line}");
    }

    #[test]
    fn templates_expand_known_fields_and_ignore_unknown_ones() {
        let home = Path::new("/w");
        let fleet = Fleet::default();
        let row = row();
        assert_eq!(expand("{source}/{id}", &row, home, &fleet), "omp/abc");
        assert_eq!(expand("{nope}", &row, home, &fleet), "");
        assert_eq!(expand("a\\tb", &row, home, &fleet), "a\tb");
    }
}
