//! Canonical federation identity and whole-observation selection.
//!
//! Unknown origins are retained independently: a display label cannot prove
//! identity. Only routes and direct origin attribution are combined; metadata
//! always comes from one complete, shortest-route observation.

use std::cmp::Ordering;

use crate::model::{AccessRoute, Locator, PathTrust, SessionSummary, Usage};

pub(crate) fn merge(mut sessions: Vec<SessionSummary>) -> Vec<SessionSummary> {
    for session in &mut sessions {
        session.routes.sort_unstable();
        session.routes.dedup();
        if !session.is_local()
            && let Some(direct) = session.routes.iter().find(|route| {
                route.hops.len() == 1
                    && session.origin_id.as_deref() == Some(route.hops[0].origin_id.as_str())
            })
        {
            session.origin_label = Some(direct.hops[0].name.clone());
        }
    }
    sessions.sort_unstable_by(|a, b| a.key().cmp(&b.key()).then_with(|| observation(a, b)));
    sessions.dedup_by(|session, previous| {
        if session.origin_id.is_some() && previous.key() == session.key() {
            if previous.is_local() && previous.routes.is_empty() && !session.routes.is_empty() {
                previous.routes.push(AccessRoute { hops: Vec::new() });
            }
            previous.routes.append(&mut session.routes);
            true
        } else {
            false
        }
    });
    for session in &mut sessions {
        session.routes.sort_unstable_by(AccessRoute::cmp_preferred);
        session.routes.dedup();
    }
    sessions
}

fn distance(session: &SessionSummary) -> usize {
    if session.is_local() {
        0
    } else {
        session
            .preferred_route()
            .map_or(usize::MAX, |route| route.hops.len())
    }
}

// Lexical routes break equal-distance ties; all remaining fields break ties
// between inconsistent observations received over the same route, without
// serialization or a second metadata snapshot allocation.
fn observation(a: &SessionSummary, b: &SessionSummary) -> Ordering {
    distance(a)
        .cmp(&distance(b))
        .then_with(|| a.preferred_route().cmp(&b.preferred_route()))
        .then_with(|| b.updated.cmp(&a.updated))
        .then_with(|| {
            a.updated
                .offset()
                .whole_seconds()
                .cmp(&b.updated.offset().whole_seconds())
        })
        .then_with(|| a.origin_label.cmp(&b.origin_label))
        .then_with(|| a.title.cmp(&b.title))
        .then_with(|| a.workspace.cmp(&b.workspace))
        .then_with(|| a.recorded_cwd.cmp(&b.recorded_cwd))
        .then_with(|| {
            matches!(a.trust, PathTrust::Decoded).cmp(&matches!(b.trust, PathTrust::Decoded))
        })
        .then_with(|| {
            a.created
                .map(|t| (t, t.offset().whole_seconds()))
                .cmp(&b.created.map(|t| (t, t.offset().whole_seconds())))
        })
        .then_with(|| a.model.cmp(&b.model))
        .then_with(|| a.branch.cmp(&b.branch))
        .then_with(|| a.parent.cmp(&b.parent))
        .then_with(|| a.subagent.cmp(&b.subagent))
        .then_with(|| a.archived.cmp(&b.archived))
        .then_with(|| usage(a.usage, b.usage))
        .then_with(|| {
            a.change
                .map(|c| (c.files, c.added, c.deleted))
                .cmp(&b.change.map(|c| (c.files, c.added, c.deleted)))
        })
        .then_with(|| a.size_bytes.cmp(&b.size_bytes))
        .then_with(|| locator(&a.locator, &b.locator))
        .then_with(|| {
            (a.liveness.rank(), a.liveness.pid()).cmp(&(b.liveness.rank(), b.liveness.pid()))
        })
        .then_with(|| a.routes.cmp(&b.routes))
}

fn usage(a: Option<Usage>, b: Option<Usage>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a
            .input
            .cmp(&b.input)
            .then_with(|| a.output.cmp(&b.output))
            .then_with(|| a.cost.total_cmp(&b.cost)),
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
    }
}

fn locator(a: &Locator, b: &Locator) -> Ordering {
    match (a, b) {
        (Locator::File { path: a }, Locator::File { path: b }) => a.cmp(b),
        (Locator::Row { db: a, id: ai }, Locator::Row { db: b, id: bi }) => {
            a.cmp(b).then_with(|| ai.cmp(bi))
        }
        (Locator::File { .. }, Locator::Row { .. }) => Ordering::Less,
        (Locator::Row { .. }, Locator::File { .. }) => Ordering::Greater,
    }
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;
    use crate::model::{AccessRoute, RouteHop, SourceId};

    fn hop(origin: &str, name: &str) -> RouteHop {
        RouteHop {
            origin_id: origin.into(),
            name: name.into(),
            ssh: name.into(),
            url: None,
            token_env: None,
            home: None,
        }
    }

    fn row(origin: Option<&str>, route: Vec<RouteHop>) -> SessionSummary {
        let mut session = SessionSummary::new(
            "same-id".into(),
            SourceId::Omp,
            OffsetDateTime::UNIX_EPOCH,
            Locator::File {
                path: "/sessions/same-id".into(),
            },
        );
        session.origin_id = origin.map(str::to_owned);
        if !route.is_empty() {
            session.origin_label = Some("forwarded-label".into());
            session.routes.push(AccessRoute { hops: route });
        }
        session
    }

    fn json(sessions: Vec<SessionSummary>) -> String {
        serde_json::to_string(&merge(sessions)).unwrap()
    }

    #[test]
    fn diamond_merges_routes_and_prefers_a_whole_direct_observation() {
        let mut indirect = row(Some("B"), vec![hop("A", "alpha"), hop("B", "beta")]);
        indirect.title = Some("indirect snapshot".into());
        indirect.usage = Some(Usage {
            input: 10,
            output: 20,
            cost: 9.0,
        });
        indirect.archived = true;
        indirect.updated += time::Duration::days(1);
        let mut direct = row(Some("B"), vec![hop("B", "configured-beta")]);
        direct.title = Some("direct snapshot".into());
        let expected = json(vec![direct.clone(), indirect.clone()]);
        assert_eq!(expected, json(vec![indirect.clone(), direct.clone()]));
        let merged = merge(vec![indirect, direct.clone(), direct]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].origin_label.as_deref(), Some("configured-beta"));
        assert_eq!(merged[0].title.as_deref(), Some("direct snapshot"));
        assert_eq!(merged[0].usage, None);
        assert!(!merged[0].archived);
        assert_eq!(merged[0].updated, OffsetDateTime::UNIX_EPOCH);
        assert_eq!(merged[0].routes.len(), 2);
        assert_eq!(merged[0].preferred_route().unwrap().hops.len(), 1);
    }

    #[test]
    fn origins_sources_and_unknown_observations_remain_distinct() {
        let first = row(Some("A"), vec![hop("A", "alpha")]);
        let second = row(Some("B"), vec![hop("B", "beta")]);
        let mut other_source = first.clone();
        other_source.source = SourceId::Claude;
        let unknown = row(None, vec![hop("A", "alpha")]);
        assert_eq!(
            merge(vec![first, second, other_source, unknown.clone(), unknown]).len(),
            5
        );
    }

    #[test]
    fn equal_length_routes_and_conflicting_same_route_snapshots_are_deterministic() {
        let mut alpha = row(Some("B"), vec![hop("A", "alpha"), hop("B", "beta")]);
        let mut zeta = row(Some("B"), vec![hop("Z", "zeta"), hop("B", "beta")]);
        alpha.title = Some("alpha wins".into());
        zeta.title = Some("zeta loses".into());
        assert_eq!(
            json(vec![alpha.clone(), zeta.clone()]),
            json(vec![zeta, alpha.clone()])
        );
        let mut conflict = alpha.clone();
        conflict.archived = true;
        conflict.usage = Some(Usage {
            input: 1,
            output: 0,
            cost: 0.0,
        });
        assert_eq!(
            json(vec![alpha.clone(), conflict.clone()]),
            json(vec![conflict, alpha])
        );
    }

    #[test]
    fn local_observation_wins_without_losing_remote_routes() {
        let mut local = row(Some("A"), vec![]);
        local.title = Some("local observation".into());
        let mut remote = row(Some("A"), vec![hop("A", "alpha")]);
        remote.title = Some("remote observation".into());
        let merged = merge(vec![remote, local]);
        assert_eq!(merged.len(), 1);
        assert!(merged[0].routes.iter().any(|route| route.hops.is_empty()));
        assert!(
            merged[0]
                .routes
                .iter()
                .any(|route| route.hops == vec![hop("A", "alpha")])
        );
        assert_eq!(
            merged[0].origin_label, None,
            "a local observation retains local origin attribution"
        );
        assert!(
            merged[0].is_local(),
            "remote alternatives must not erase direct local access"
        );
        assert_eq!(merged[0].title.as_deref(), Some("local observation"));
        assert!(merged[0].preferred_route().unwrap().hops.is_empty());
        assert_eq!(serde_json::to_string(&merged).unwrap(), json(merged));
    }

    #[test]
    fn different_direct_aliases_share_identity_but_retain_both_routes() {
        let mut alpha = row(Some("B"), vec![hop("B", "alpha")]);
        let mut beta = row(Some("B"), vec![hop("B", "beta")]);
        alpha.title = Some("alpha snapshot".into());
        beta.title = Some("beta snapshot".into());
        let expected = json(vec![alpha.clone(), beta.clone()]);
        assert_eq!(expected, json(vec![beta.clone(), alpha.clone()]));
        let merged = merge(vec![beta, alpha]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].origin_label.as_deref(), Some("alpha"));
        assert_eq!(merged[0].title.as_deref(), Some("alpha snapshot"));
        assert_eq!(merged[0].routes.len(), 2);
        assert_eq!(merged[0].preferred_route().unwrap().hops[0].ssh, "alpha");
        assert_eq!(serde_json::to_string(&merged).unwrap(), json(merged));
    }

    #[test]
    fn timestamp_offset_is_a_deterministic_whole_observation_tiebreak() {
        let first = row(Some("B"), vec![hop("B", "beta")]);
        let mut second = first.clone();
        second.updated = second
            .updated
            .to_offset(time::UtcOffset::from_hms(1, 0, 0).unwrap());
        assert_eq!(
            json(vec![first.clone(), second.clone()]),
            json(vec![second, first])
        );
    }
}
