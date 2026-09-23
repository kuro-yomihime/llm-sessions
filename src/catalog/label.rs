//! What to call a session in a list.
//!
//! Titles are a poor key. Measured on this machine: 51 sessions share
//! `"review changes [commit|branch|pr], defaults to uncommitted (@build
//! subagent)"`, 15 more share the next string, and 43 sessions have no title
//! at all — a slash command names every invocation identically, so a whole
//! project reads as a wall of indistinguishable rows.
//!
//! So a label is *derived*, with the rung that produced it recorded. A
//! derived label is often the better one, but it must never masquerade as a
//! name the agent chose, which is why [`Label::source`] travels with the text
//! and the renderer dims anything that is not [`LabelSource::Title`].

use std::collections::HashMap;

use crate::model::{Label, LabelSource, SessionSummary, SourceId};
use crate::output::reader::truncate;

/// Longest label worth carrying; the panes truncate further as they see fit.
const MAX_LEN: usize = 160;

pub type Key = (SourceId, String);

/// Label every session, then break ties inside each workspace.
///
/// `prompts` is the index's `first_prompt` cache, keyed like
/// [`SessionSummary::key`]. Without it the chain simply falls through to the
/// tool or fallback rungs — labelling never requires parsing a body.
pub fn assign(sessions: &[SessionSummary], prompts: &HashMap<Key, String>) -> HashMap<Key, Label> {
    let mut labels: HashMap<Key, Label> = sessions
        .iter()
        .map(|session| {
            let key = (session.source, session.id.clone());
            let prompt = prompts.get(&key).map(String::as_str);
            (key, derive(session, prompt))
        })
        .collect();
    disambiguate(sessions, &mut labels);
    labels
}

/// The precedence chain: agent title, first user prompt, first tool, then a
/// last resort that is at least unique.
pub fn derive(session: &SessionSummary, prompt: Option<&str>) -> Label {
    if let Some(text) = session.title.as_deref().and_then(clean) {
        return Label {
            text,
            source: LabelSource::Title,
        };
    }
    if let Some(text) = prompt.and_then(clean) {
        return Label {
            text,
            source: LabelSource::Prompt,
        };
    }
    Label {
        text: format!("{} session at {}", session.source, stamp(session)),
        source: LabelSource::Fallback,
    }
}

/// Give repeated labels inside one workspace something to tell them apart.
///
/// Preference order for the suffix: branch, then the id's short form. A
/// changed-file name would be better still but is not available at tier 1 for
/// every source, so it is not used here — an inconsistent suffix is worse
/// than a dull one.
fn disambiguate(sessions: &[SessionSummary], labels: &mut HashMap<Key, Label>) {
    // Bucket by (workspace, label text) with owned keys, so the pass that
    // rewrites labels cannot alias the pass that grouped them.
    let mut buckets: HashMap<(String, String), Vec<Key>> = HashMap::new();
    for session in sessions {
        let key = (session.source, session.id.clone());
        let Some(label) = labels.get(&key) else {
            continue;
        };
        let workspace = session
            .workspace
            .as_deref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        buckets
            .entry((workspace, label.text.clone()))
            .or_default()
            .push(key);
    }

    for keys in buckets.into_values() {
        if keys.len() < 2 {
            continue;
        }
        for key in keys {
            let Some(session) = sessions.iter().find(|s| s.source == key.0 && s.id == key.1) else {
                continue;
            };
            let suffix = session
                .branch
                .clone()
                .unwrap_or_else(|| short_id(&session.id));
            if let Some(label) = labels.get_mut(&key) {
                label.text = truncate(&format!("{} · {suffix}", label.text), MAX_LEN);
            }
        }
    }
}

fn clean(raw: &str) -> Option<String> {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then(|| truncate(&collapsed, MAX_LEN))
}

fn short_id(id: &str) -> String {
    id.chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn stamp(session: &SessionSummary) -> String {
    let t = session.updated;
    let (h, m, _) = t.time().as_hms();
    format!("{} {h:02}:{m:02}", t.date())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use time::OffsetDateTime;

    use super::*;
    use crate::model::{Liveness, Locator, PathTrust};

    fn session(id: &str, title: Option<&str>, workspace: &str) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            source: SourceId::Omp,
            workspace: (!workspace.is_empty()).then(|| PathBuf::from(workspace)),
            recorded_cwd: None,
            trust: PathTrust::Recorded,
            title: title.map(str::to_owned),
            created: None,
            updated: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            model: None,
            branch: None,
            parent: None,
            subagent: false,
            archived: false,
            usage: None,
            change: None,
            size_bytes: None,
            locator: Locator::File {
                path: PathBuf::from("/nonexistent"),
            },
            liveness: Liveness::Stale,
            host: None,
        }
    }

    fn key(s: &SessionSummary) -> Key {
        (s.source, s.id.clone())
    }

    #[test]
    fn the_agents_own_title_wins_when_it_says_something() {
        let s = session("a", Some("  Fix   the parser  "), "/w");
        let label = derive(&s, Some("some prompt"));
        assert_eq!(label.text, "Fix the parser");
        assert_eq!(label.source, LabelSource::Title);
    }

    #[test]
    fn an_untitled_session_falls_through_to_its_first_prompt() {
        let s = session("a", None, "/w");
        let label = derive(&s, Some("Rewrite crop_and_zoom.py in Rust"));
        assert_eq!(label.source, LabelSource::Prompt);
        assert_eq!(label.text, "Rewrite crop_and_zoom.py in Rust");
    }

    #[test]
    fn a_session_with_nothing_still_gets_a_usable_name() {
        let s = session("a", Some("   "), "/w");
        let label = derive(&s, None);
        assert_eq!(label.source, LabelSource::Fallback);
        assert!(label.text.starts_with("omp session at "));
    }

    #[test]
    fn repeated_titles_in_one_workspace_are_told_apart() {
        let mut first = session("aaaaaa111111", Some("review changes"), "/w/one");
        first.branch = Some("feat/one".into());
        let second = session("bbbbbb222222", Some("review changes"), "/w/one");
        let elsewhere = session("cccccc333333", Some("review changes"), "/w/two");

        let sessions = vec![first, second, elsewhere];
        let labels = assign(&sessions, &HashMap::new());

        // Branch when there is one, short id otherwise.
        assert_eq!(labels[&key(&sessions[0])].text, "review changes · feat/one");
        assert_eq!(labels[&key(&sessions[1])].text, "review changes · 222222");
        // A different workspace is not a collision.
        assert_eq!(labels[&key(&sessions[2])].text, "review changes");
    }

    #[test]
    fn a_derived_label_never_claims_to_be_a_title() {
        let sessions = vec![session("a", None, "/w")];
        let mut prompts = HashMap::new();
        prompts.insert(key(&sessions[0]), "do the thing".to_string());
        let labels = assign(&sessions, &prompts);
        assert_eq!(labels[&key(&sessions[0])].source, LabelSource::Prompt);
    }
}
