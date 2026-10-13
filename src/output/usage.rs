//! Aggregated recorded usage without reopening agent transcripts.
//!
//! Tier-1 usage wins when an adapter exposes it cheaply. The index fills the
//! same gap for body-only agents, preserving `None` so missing accounting is
//! never reported as a free session.

use std::collections::HashMap;

use serde::Serialize;
use time::OffsetDateTime;

use crate::error::Result;
use crate::model::{SessionSummary, Usage};
use crate::storage::index::Index;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Axis {
    #[value(alias = "ws")]
    Workspace,
    Model,
    #[value(alias = "source")]
    Agent,
    /// Calendar day in the session timestamp's recorded offset (usually UTC).
    Day,
}

impl std::fmt::Display for Axis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Workspace => "workspace",
            Self::Model => "model",
            Self::Agent => "agent",
            Self::Day => "day",
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Row {
    pub key: String,
    pub sessions: usize,
    pub usage: Usage,
    pub priced: usize,
}

/// Aggregate tier-1 usage, falling back to the body-derived index cache.
/// Sessions without accounting count toward `sessions`, but not `priced`.
pub fn report(
    index: &Index,
    sessions: &[SessionSummary],
    axis: Axis,
    since: Option<OffsetDateTime>,
) -> Result<Vec<Row>> {
    let cached_usage = index.usage()?;
    let cached_models = index.model_usage_per_session()?;
    let mut rows = HashMap::new();

    for session in sessions
        .iter()
        .filter(|session| since.is_none_or(|since| session.updated >= since))
    {
        let usage = cached_usage.resolve(session);

        match axis {
            Axis::Workspace => add(
                &mut rows,
                session.workspace.as_ref().map_or_else(
                    || "—".to_owned(),
                    |path| path.to_string_lossy().into_owned(),
                ),
                usage,
            ),
            Axis::Agent => add(&mut rows, session.source.name().to_owned(), usage),
            Axis::Day => {
                // The offset is part of the persisted timestamp; using it avoids
                // guessing a host-local zone in the multithreaded scanner.
                add(&mut rows, session.updated.date().to_string(), usage);
            }
            Axis::Model => {
                let models = session
                    .is_local()
                    .then(|| cached_models.get(&(session.source, session.id.clone())))
                    .flatten();
                if models.is_some_and(|models| !models.is_empty()) {
                    for (model, model_usage) in models.into_iter().flatten() {
                        add(&mut rows, model.clone(), Some(*model_usage));
                    }
                } else {
                    add(
                        &mut rows,
                        session.model.clone().unwrap_or_else(|| "—".to_owned()),
                        usage,
                    );
                }
            }
        }
    }

    let mut rows: Vec<_> = rows
        .into_iter()
        .map(|(key, aggregate)| Row {
            key,
            sessions: aggregate.sessions,
            usage: aggregate.usage,
            priced: aggregate.priced,
        })
        .collect();
    rows.sort_by(|left, right| {
        right
            .usage
            .cost
            .total_cmp(&left.usage.cost)
            .then_with(|| {
                right
                    .usage
                    .input
                    .saturating_add(right.usage.output)
                    .cmp(&left.usage.input.saturating_add(left.usage.output))
            })
            .then_with(|| left.key.cmp(&right.key))
    });
    Ok(rows)
}

#[derive(Default)]
struct Aggregate {
    sessions: usize,
    usage: Usage,
    priced: usize,
}

fn add(rows: &mut HashMap<String, Aggregate>, key: String, usage: Option<Usage>) {
    let aggregate = rows.entry(key).or_default();
    aggregate.sessions += 1;
    if let Some(usage) = usage {
        aggregate.priced += 1;
        aggregate.usage.input = aggregate.usage.input.saturating_add(usage.input);
        aggregate.usage.output = aggregate.usage.output.saturating_add(usage.output);
        aggregate.usage.cost += usage.cost;
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::model::{Locator, SourceId};

    fn session(id: &str, usage: Option<Usage>) -> SessionSummary {
        SessionSummary {
            workspace: Some(PathBuf::from("/workspace")),
            model: Some("model-a".to_owned()),
            usage,
            ..SessionSummary::new(
                id.to_owned(),
                SourceId::OpenCode,
                OffsetDateTime::UNIX_EPOCH,
                Locator::Row {
                    db: PathBuf::from("fixture.db"),
                    id: id.to_owned(),
                },
            )
        }
    }

    #[test]
    fn tier_one_usage_is_reported_when_the_session_is_not_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("index.db")).unwrap();
        let rows = report(
            &index,
            &[session(
                "outside-index",
                Some(Usage {
                    input: 3,
                    output: 5,
                    cost: 1.25,
                }),
            )],
            Axis::Workspace,
            None,
        )
        .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "/workspace");
        assert_eq!(rows[0].sessions, 1);
        assert_eq!(rows[0].priced, 1);
        assert!((rows[0].usage.cost - 1.25).abs() < 1e-9);
    }

    #[test]
    fn unpriced_session_is_counted_without_becoming_recorded_zero_cost() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("index.db")).unwrap();
        let rows = report(
            &index,
            &[
                session("priced", Some(Usage::default())),
                session("unknown", None),
            ],
            Axis::Workspace,
            None,
        )
        .unwrap();

        assert_eq!(rows[0].sessions, 2);
        assert_eq!(rows[0].priced, 1);
        assert!(rows[0].usage.is_zero());
    }
}
