//! Fast workspace-grouped activity digests.
//!
//! The index already holds parsed first prompts, so digest construction never
//! opens an agent transcript. An unindexed session falls back to its tier-1
//! title instead of paying a body-parse cost.

use std::collections::HashMap;
use std::path::PathBuf;

use time::OffsetDateTime;

use crate::error::Result;
use crate::model::{SessionSummary, SourceId, Usage};
use crate::storage::index::Index;

#[derive(Clone, Debug)]
pub struct Entry {
    pub workspace: Option<PathBuf>,
    pub source: SourceId,
    pub session_id: String,
    pub updated: OffsetDateTime,
    pub title: Option<String>,
    pub prompt: Option<String>,
    pub usage: Option<Usage>,
}

/// Sessions touched since `since`, grouped by workspace and newest first.
///
/// Prompts are read from the local index only for local sessions. Other rows
/// use their tier-1 title as a fast fallback; this function never parses a
/// session body.
pub fn build(
    index: &Index,
    sessions: &[SessionSummary],
    since: OffsetDateTime,
) -> Result<Vec<(Option<PathBuf>, Vec<Entry>)>> {
    let cached = index.digest_data()?;
    let mut groups: HashMap<Option<PathBuf>, Vec<Entry>> = HashMap::new();

    for session in sessions.iter().filter(|session| session.updated >= since) {
        let prompt = session
            .is_local()
            .then(|| cached.prompts.get(&(session.source, session.id.clone())))
            .flatten()
            .cloned()
            .or_else(|| session.title.clone());
        let usage = cached.usage.resolve(session);
        groups
            .entry(session.workspace.clone())
            .or_default()
            .push(Entry {
                workspace: session.workspace.clone(),
                source: session.source,
                session_id: session.id.clone(),
                updated: session.updated,
                title: session.title.clone(),
                prompt,
                usage,
            });
    }

    let mut groups: Vec<_> = groups.into_iter().collect();
    for (_, entries) in &mut groups {
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.updated));
    }
    groups.sort_by(|(_, left), (_, right)| {
        let left_updated = left.first().map(|entry| entry.updated);
        let right_updated = right.first().map(|entry| entry.updated);
        right_updated.cmp(&left_updated)
    });
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Locator;

    fn session(id: &str, updated: OffsetDateTime) -> SessionSummary {
        SessionSummary {
            workspace: Some(PathBuf::from("/workspace")),
            recorded_cwd: Some(PathBuf::from("/workspace")),
            title: Some("tier-one title".to_owned()),
            ..SessionSummary::new(
                id.to_owned(),
                SourceId::Omp,
                updated,
                Locator::Row {
                    db: PathBuf::from("fixture.db"),
                    id: id.to_owned(),
                },
            )
        }
    }

    #[test]
    fn unindexed_session_uses_its_title_without_opening_a_body() {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("index.db")).unwrap();
        let session = session("not-indexed", OffsetDateTime::UNIX_EPOCH);

        let digest = build(&index, &[session], OffsetDateTime::UNIX_EPOCH).unwrap();

        assert_eq!(digest.len(), 1);
        assert_eq!(digest[0].1[0].prompt.as_deref(), Some("tier-one title"));
    }
}
