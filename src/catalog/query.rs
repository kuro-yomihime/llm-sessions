//! Shared metadata and body-query parser.
//!
//! Keeping this grammar outside the UI and CLI prevents a filter typed in one
//! surface from silently meaning something different in another.

use std::collections::HashSet;

use time::OffsetDateTime;

use crate::clock::parse_since;
use crate::model::{SessionSummary, SourceId};
/// The archived / live / query gate, without the subagent rule.
///
/// Shared by both frontends: the TUI's toggles and the CLI's flags mean the
/// same thing because they both end up here.
///
/// The one statement of what a given view considers visible; App supplies
/// the toggles, and [`Query`] owns the grammar.
pub struct Filters<'a> {
    pub query: &'a Query,
    /// Rows the last full-text search matched, by key.
    pub hits: Option<&'a HashSet<(SourceId, String)>>,
    pub show_archived: bool,
    pub live_only: bool,
}

pub fn eligible(session: &SessionSummary, filters: &Filters<'_>) -> bool {
    if !filters.query.has_archived_filter() && !filters.show_archived && session.archived {
        return false;
    }
    if filters.live_only && !session.liveness.is_running() {
        return false;
    }
    if let Some(hits) = filters.hits
        && !hits.contains(&(session.source, session.id.clone()))
    {
        return false;
    }
    filters.query.matches(session)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Comparison {
    Equal,
    Greater,
    Less,
}

/// Tolerance for `cost:` equality, in dollars.
///
/// Recorded costs are sums of per-message floats, so a session priced at
/// exactly `0.3` may be stored as `0.30000000000000004`. A nano-dollar is far
/// below any real price difference and far above summation noise, so `cost:0.3`
/// matches what the user sees while exact inputs keep matching exactly.
const COST_EPSILON: f64 = 1e-9;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Archived {
    Yes,
    No,
    Only,
}

#[derive(Clone, Debug)]
enum Predicate {
    Source(String),
    /// Machine a session lives on. `local` is this one, anything else is a
    /// host name from `hosts.json`.
    Host(String),
    Workspace(String),
    Model(String),
    Branch(String),
    Id(String),
    After(OffsetDateTime),
    Before(OffsetDateTime),
    Cost(Comparison, f64),
    Live(bool),
    Archived(Archived),
}

/// A parsed list/search filter.
///
/// `body:` terms deliberately remain separate: tier-1 summaries cannot answer
/// them, while the FTS index can. All other predicates are conjunctive.
pub struct Query {
    predicates: Vec<Predicate>,
    free_terms: Vec<String>,
    body_terms: Vec<String>,
    has_archived_filter: bool,
}

impl Query {
    /// Never fails: an unparseable term degrades to free text.
    pub fn parse(input: &str) -> Self {
        let mut query = Self {
            predicates: Vec::new(),
            free_terms: Vec::new(),
            body_terms: Vec::new(),
            has_archived_filter: false,
        };
        let now = OffsetDateTime::now_utc();

        for (term, balanced) in tokenize(input) {
            if !balanced {
                query.add_free(&term);
                continue;
            }
            let Some((key, value)) = term.split_once(':') else {
                query.add_free(&term);
                continue;
            };
            if key.is_empty() || value.is_empty() {
                query.add_free(&term);
                continue;
            }

            let key = key.to_ascii_lowercase();
            let parsed = match key.as_str() {
                "source" | "agent" => Some(Predicate::Source(fold(value))),
                "ws" | "workspace" => Some(Predicate::Workspace(fold(value))),
                "host" | "machine" => Some(Predicate::Host(fold(value))),
                "model" => Some(Predicate::Model(fold(value))),
                "branch" => Some(Predicate::Branch(fold(value))),
                "id" => Some(Predicate::Id(value.to_owned())),
                "after" => parse_since(value, now).map(Predicate::After),
                "before" => parse_since(value, now).map(Predicate::Before),
                "cost" => parse_cost(value)
                    .map(|(comparison, amount)| Predicate::Cost(comparison, amount)),
                "live" => parse_bool(value).map(Predicate::Live),
                "archived" => parse_archived(value).map(Predicate::Archived),
                "body" => {
                    query.body_terms.push(value.to_owned());
                    continue;
                }
                _ => None,
            };

            if let Some(predicate) = parsed {
                if matches!(predicate, Predicate::Archived(_)) {
                    query.has_archived_filter = true;
                }
                query.predicates.push(predicate);
            } else {
                query.add_free(&term);
            }
        }

        query
    }

    pub fn is_empty(&self) -> bool {
        self.predicates.is_empty() && self.free_terms.is_empty() && self.body_terms.is_empty()
    }

    /// Metadata predicate. `body:` terms are ignored here.
    pub fn matches(&self, session: &SessionSummary) -> bool {
        if !self.free_terms.is_empty() {
            let mut searchable = String::new();
            if let Some(title) = &session.title {
                searchable.push_str(title);
                searchable.push('\n');
            }
            if let Some(workspace) = &session.workspace {
                searchable.push_str(&workspace.to_string_lossy());
                searchable.push('\n');
            }
            searchable.push_str(session.source.name());
            if let Some(host) = &session.host {
                searchable.push('\n');
                searchable.push_str(host);
            }
            let searchable = searchable.to_lowercase();
            if self
                .free_terms
                .iter()
                .any(|term| !searchable.contains(term))
            {
                return false;
            }
        }

        self.predicates.iter().all(|predicate| match predicate {
            Predicate::Source(value) => session.source.name().starts_with(value),
            Predicate::Host(value) => session
                .host
                .as_deref()
                .unwrap_or("local")
                .starts_with(value),
            Predicate::Workspace(value) => session
                .workspace
                .as_ref()
                .is_some_and(|path| path.to_string_lossy().to_lowercase().contains(value)),
            Predicate::Model(value) => session
                .model
                .as_deref()
                .is_some_and(|model| fold(model).contains(value)),
            Predicate::Branch(value) => session
                .branch
                .as_deref()
                .is_some_and(|branch| fold(branch).contains(value)),
            Predicate::Id(value) => session.id.starts_with(value),
            Predicate::After(time) => session.updated >= *time,
            Predicate::Before(time) => session.updated <= *time,
            Predicate::Cost(comparison, amount) => {
                session.usage.is_some_and(|usage| match comparison {
                    Comparison::Equal => (usage.cost - *amount).abs() < COST_EPSILON,
                    Comparison::Greater => usage.cost > *amount,
                    Comparison::Less => usage.cost < *amount,
                })
            }
            Predicate::Live(wanted) => session.liveness.is_running() == *wanted,
            Predicate::Archived(wanted) => match wanted {
                Archived::Yes | Archived::Only => session.archived,
                Archived::No => !session.archived,
            },
        })
    }

    /// Input for [`crate::storage::index::Index::search`], containing `body:` terms and
    /// bare words. `Index` owns FTS5 escaping, so this remains ordinary user
    /// input instead of exposing FTS operators here.
    pub fn body_query(&self) -> Option<String> {
        self.body_terms
            .iter()
            .chain(&self.free_terms)
            .cloned()
            .reduce(|mut all, term| {
                all.push(' ');
                all.push_str(&term);
                all
            })
    }

    /// True when a body constraint requires the FTS index.
    pub fn needs_index(&self) -> bool {
        !self.body_terms.is_empty()
    }

    /// Whether `matches` already decided archived visibility.
    ///
    /// An explicit `archived:` predicate wins over the TUI/CLI archived toggle;
    /// callers should apply that toggle only when this returns `false`.
    pub fn has_archived_filter(&self) -> bool {
        self.has_archived_filter
    }

    fn add_free(&mut self, term: &str) {
        if !term.is_empty() {
            self.free_terms.push(fold(term));
        }
    }
}

fn tokenize(input: &str) -> Vec<(String, bool)> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut quoted = false;

    for character in input.chars() {
        match character {
            '"' => quoted = !quoted,
            character if character.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    terms.push((std::mem::take(&mut current), true));
                }
            }
            _ => current.push(character),
        }
    }
    if !current.is_empty() {
        terms.push((current, !quoted));
    } else if quoted {
        terms.push(("\"".into(), false));
    }
    terms
}

fn fold(value: &str) -> String {
    value.to_lowercase()
}

fn parse_cost(value: &str) -> Option<(Comparison, f64)> {
    let (comparison, number) = match value.as_bytes().first() {
        Some(b'>') => (Comparison::Greater, &value[1..]),
        Some(b'<') => (Comparison::Less, &value[1..]),
        _ => (Comparison::Equal, value),
    };
    let number = number.parse::<f64>().ok()?;
    number.is_finite().then_some((comparison, number))
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

fn parse_archived(value: &str) -> Option<Archived> {
    match value.to_ascii_lowercase().as_str() {
        "yes" => Some(Archived::Yes),
        "no" => Some(Archived::No),
        "only" => Some(Archived::Only),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use time::OffsetDateTime;

    use super::*;
    use crate::model::{Liveness, Locator, PathTrust, SourceId, Usage};

    fn session(source: SourceId, title: &str, workspace: &str) -> SessionSummary {
        SessionSummary {
            id: "session-1".into(),
            source,
            workspace: Some(PathBuf::from(workspace)),
            recorded_cwd: None,
            trust: PathTrust::Recorded,
            title: Some(title.into()),
            created: None,
            updated: OffsetDateTime::UNIX_EPOCH,
            model: None,
            branch: None,
            parent: None,
            subagent: false,
            archived: false,
            usage: None,
            change: None,
            size_bytes: None,
            locator: Locator::File {
                path: PathBuf::from("session.jsonl"),
            },
            liveness: Liveness::Stale,
            host: None,
        }
    }

    #[test]
    fn source_and_workspace_narrow_together() {
        let query = Query::parse("source:omp ws:dstm");
        assert!(query.matches(&session(SourceId::Omp, "work", "/work/dstm")));
        assert!(!query.matches(&session(SourceId::Claude, "work", "/work/dstm")));
        assert!(!query.matches(&session(SourceId::Omp, "work", "/work/other")));

        let quoted = Query::parse("ws:\"my dir\"");
        assert!(quoted.matches(&session(SourceId::Omp, "work", "/work/my dir")));
    }

    #[test]
    fn cost_filter_excludes_unrecorded_usage() {
        let query = Query::parse("cost:>1");
        let mut priced = session(SourceId::Omp, "work", "/work/dstm");
        priced.usage = Some(Usage {
            input: 0,
            output: 0,
            cost: 1.01,
        });
        assert!(query.matches(&priced));
        assert!(!query.matches(&session(SourceId::Omp, "work", "/work/dstm")));
    }

    #[test]
    fn malformed_terms_degrade_to_free_text() {
        let unbalanced = Query::parse("title:\"unfinished");
        assert!(unbalanced.matches(&session(SourceId::Omp, "title:unfinished", "/work")));

        let colon = Query::parse(":");
        assert!(colon.matches(&session(SourceId::Omp, ":", "/work")));
    }

    #[test]
    fn body_only_requires_the_index() {
        let query = Query::parse("body:naming");
        assert!(query.needs_index());
        assert_eq!(query.body_query().as_deref(), Some("naming"));
    }
}
