//! A session selects its harness by family and origin, never by reachability.
//! Transport negotiation belongs to that already-bound harness at write time.

#[cfg(feature = "http")]
pub mod hermes;
#[cfg(feature = "http")]
pub mod openclaw;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::error::{Error, FailureKind, Result, io};
use crate::model::{SessionSummary, SourceId};
use crate::runtime::insert::{self, Delivery};
use crate::runtime::live::Fleet;
use crate::source::Source;

/// An attached session's writing capability. Implementations own negotiation;
/// callers never select another harness when a transport is unavailable.
pub trait SessionHarness {
    fn write_message(&self, message: &str, timeout: Duration) -> Result<Delivery>;
}

pub fn failure(harness: &'static str, kind: FailureKind, message: impl Into<String>) -> Error {
    Error::Delivery {
        harness,
        kind,
        message: message.into(),
    }
}

pub fn launch(label: String, work: impl FnOnce() -> Result<()> + Send + 'static) -> Delivery {
    Delivery::background(label, work)
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    sessions: Vec<Binding>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    #[serde(default)]
    origin_id: Option<String>,
    source: SourceId,
    session_id: String,
    config: serde_json::Value,
}

impl Config {
    fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                failure(
                    "configuration",
                    FailureKind::Protocol,
                    format!("{}: {error}", path.display()),
                )
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(io(path)(error)),
        }
    }

    fn binding(&self, session: &SessionSummary) -> Result<Option<&Binding>> {
        let mut matches = self.sessions.iter().filter(|binding| {
            binding.source == session.source
                && binding.session_id == session.id
                && match &binding.origin_id {
                    None => session.is_local(),
                    Some(origin) => session
                        .origin_id
                        .as_deref()
                        .is_some_and(|id| id.eq_ignore_ascii_case(origin)),
                }
        });
        let found = matches.next();
        if matches.next().is_some() {
            return Err(failure(
                session.source.name(),
                FailureKind::Protocol,
                "duplicate session harness binding",
            ));
        }
        Ok(found)
    }
}

/// Bind a selected/discovered session. Only its own family's configuration is
/// eligible; federation URLs are read endpoints, not agent gateway endpoints.
pub fn attach<'a>(
    session: &'a SessionSummary,
    sources: &'a [Box<dyn Source>],
    fleet: &'a Fleet,
    home: &Path,
) -> Result<Box<dyn SessionHarness + 'a>> {
    let path = crate::config_home(home).join("llm-sessions/harnesses.json");
    attach_at(session, sources, fleet, &path)
}

fn attach_at<'a>(
    session: &'a SessionSummary,
    sources: &'a [Box<dyn Source>],
    fleet: &'a Fleet,
    path: &Path,
) -> Result<Box<dyn SessionHarness + 'a>> {
    if session.subagent {
        return Err(failure(
            session.source.name(),
            FailureKind::Rejected,
            "a subagent takes messages from its parent, not from here",
        ));
    }
    if matches!(session.source, SourceId::OpenClaw | SourceId::Hermes) {
        let config = Config::load(path)?;
        if let Some(binding) = config.binding(session)? {
            return configured(session, binding);
        }
    }
    let instance = CommandInstance {
        session,
        sources,
        fleet,
    };
    // No network probes or dispatch when opening the composer. The legacy
    // preflight retains all its exact-owner and pinned-origin refusals.
    instance.command("")?;
    if session.source == SourceId::Omp {
        Ok(Box::new(OmpHarness { instance }))
    } else {
        Ok(Box::new(CliHarness { instance }))
    }
}

#[cfg(feature = "http")]
fn configured(session: &SessionSummary, binding: &Binding) -> Result<Box<dyn SessionHarness>> {
    match session.source {
        SourceId::OpenClaw => {
            let config: openclaw::OpenClawConfig = serde_json::from_value(binding.config.clone())
                .map_err(|error| {
                failure("openclaw", FailureKind::Protocol, error.to_string())
            })?;
            if !session.is_local() && config.cli {
                return Err(failure(
                    "openclaw",
                    FailureKind::Rejected,
                    "co-located CLI delivery is local only; configure this remote instance's gateway",
                ));
            }
            Ok(Box::new(openclaw::OpenClawHarness {
                session_id: session.id.clone(),
                config,
            }))
        }
        SourceId::Hermes => {
            let config = serde_json::from_value(binding.config.clone())
                .map_err(|error| failure("hermes", FailureKind::Protocol, error.to_string()))?;
            Ok(Box::new(hermes::HermesHarness {
                session_id: session.id.clone(),
                config,
            }))
        }
        _ => Err(Error::Unsupported {
            agent: session.source,
            what: "gateway insertion",
        }),
    }
}

#[cfg(not(feature = "http"))]
fn configured(session: &SessionSummary, binding: &Binding) -> Result<Box<dyn SessionHarness>> {
    let _ = &binding.config;
    Err(failure(
        session.source.name(),
        FailureKind::Unavailable,
        "gateway insertion requires the http feature",
    ))
}

/// All command delivery still uses the original exact-owner, foreground,
/// pinned-origin and idle-writer guards. The family is fixed by this instance.
struct CommandInstance<'a> {
    session: &'a SessionSummary,
    sources: &'a [Box<dyn Source>],
    fleet: &'a Fleet,
}

impl CommandInstance<'_> {
    fn command(&self, message: &str) -> Result<Vec<String>> {
        let result = if self.session.is_local() {
            insert::local_command(self.session, self.sources, self.fleet, message)
        } else {
            crate::source::remote::insert_command(self.session, self.sources, message)
        };
        result
            .map_err(|message| failure(self.session.source.name(), FailureKind::Rejected, message))
    }

    fn write(&self, message: &str, timeout: Duration) -> Result<Delivery> {
        let command = self.command(message)?;
        let cwd: Option<PathBuf> = self
            .session
            .is_local()
            .then(|| {
                [&self.session.recorded_cwd, &self.session.workspace]
                    .into_iter()
                    .flatten()
                    .find(|dir| dir.is_dir())
                    .cloned()
            })
            .flatten();
        let label = if self.session.is_local() {
            format!("{} {}", self.session.source, self.session.id)
        } else {
            format!(
                "{} {} on {}",
                self.session.source,
                self.session.id,
                self.session.origin_label.as_deref().unwrap_or("remote")
            )
        };
        Ok(insert::send(label, &command, cwd, timeout))
    }
}

/// OMP's existing CLI/pane harness; no new fallback or ownership inference.
struct OmpHarness<'a> {
    instance: CommandInstance<'a>,
}

impl SessionHarness for OmpHarness<'_> {
    fn write_message(&self, message: &str, timeout: Duration) -> Result<Delivery> {
        self.instance.write(message, timeout)
    }
}

/// Existing source-native CLI/live-server harness (including SSH). Its source
/// is captured at attachment; it cannot become an `OpenClaw` or Hermes gateway.
struct CliHarness<'a> {
    instance: CommandInstance<'a>,
}

impl SessionHarness for CliHarness<'_> {
    fn write_message(&self, message: &str, timeout: Duration) -> Result<Delivery> {
        self.instance.write(message, timeout)
    }
}

#[cfg(test)]
mod tests;
