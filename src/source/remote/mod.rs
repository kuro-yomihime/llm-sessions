//! Sessions on other machines, reached over SSH.
//!
//! Two transports, chosen per host. When `llm-sessions` is installed on the host its
//! own JSON output is parsed back into [`SessionSummary`]/[`Transcript`]:
//! full fidelity, including the process liveness only that machine's `/proc`
//! can establish. Otherwise a generated `sh` script does a bounded listing
//! with `find`, `head` and `sqlite3` — less precise, and honest about it.
//!
//! Neither transport copies a store over the wire. A file listing costs one
//! `ssh` round trip per host, carrying at most `REMOTE_FILES_PER_SOURCE`
//! heads of `REMOTE_HEAD_BYTES` bytes.

mod parse;
mod transport;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

pub use parse::parse_shell_listing;
use serde::{Deserialize, Serialize};
use transport::{Probe, files_script, missing_sqlite3_script, open_sql_script, sql_script};

use crate::error::{Error, Result};
use crate::model::{Locator, SessionSummary, SourceId, Transcript};
use crate::runtime::live::Activity;
use crate::source::{self, Source};

/// Largest body the shell transport will pull over for reading. Past this a
/// remote `llm-sessions` is the right answer, not a multi-megabyte `cat`.
const REMOTE_BODY_MAX: u64 = 8 * 1024 * 1024;

/// How long TUI rows from a host stay fresh. A remote fleet does not change
/// faster than the eye, and this keeps one `ssh` per host per quarter minute.
const REFRESH: Duration = Duration::from_secs(15);

/// One configured machine. `name` labels rows; `ssh` is whatever `ssh`
/// accepts — an alias from `~/.ssh/config` or `user@host`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HostConfig {
    pub name: String,
    pub ssh: String,
    /// Remote home. Absent: ask the host with `echo $HOME`.
    #[serde(default)]
    pub home: Option<PathBuf>,
    /// Agent names ([`SourceId::name`]) to scan. Empty: all of them.
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub mode: Mode,
}

/// Which transport to use. `Auto` probes for a remote `llm-sessions` once per process
/// and remembers the answer.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    #[default]
    Auto,
    /// Require the remote `llm-sessions`; report a failure when it is missing.
    LlmSessions,
    /// Never use the remote `llm-sessions`, even when present.
    Shell,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub hosts: Vec<HostConfig>,
}

impl Config {
    /// Default location, honouring `$XDG_CONFIG_HOME`.
    pub fn default_path(home: &Path) -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map_or_else(|| home.join(".config"), PathBuf::from)
            .join("llm-sessions/hosts.json")
    }

    /// Absent file means no remote hosts, not an error. A malformed file is
    /// one reported failure, never a panic and never an empty list of local
    /// sessions.
    pub fn load(home: &Path) -> (Self, Option<String>) {
        Self::load_at(&Self::default_path(home))
    }

    /// Write the configuration back, creating the directory. The file is
    /// rewritten from the parsed model, so comments and unknown fields a
    /// future version might add are not preserved — `hosts.json` is small
    /// and `llm-sessions hosts` is the intended editor.
    pub fn save(&self, home: &Path) -> Result<()> {
        Self::save_at(self, &Self::default_path(home))
    }

    /// Path-based core, the counterpart of [`Config::load_at`].
    pub(crate) fn save_at(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(crate::error::io(parent))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(Error::Json)?;
        std::fs::write(path, text + "\n").map_err(crate::error::io(&path))
    }

    /// Path-based core, so tests can point at a tempdir without their
    /// ambient `$XDG_CONFIG_HOME` leaking the fixture into a real config.
    pub(crate) fn load_at(path: &Path) -> (Self, Option<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Self::default(), None),
            Err(e) => return (Self::default(), Some(format!("hosts.json: {e}"))),
        };
        let parsed: Self = match serde_json::from_str(&text) {
            Ok(parsed) => parsed,
            Err(e) => return (Self::default(), Some(format!("hosts.json: {e}"))),
        };

        let mut hosts: Vec<HostConfig> = Vec::with_capacity(parsed.hosts.len());
        let mut problems: Vec<String> = Vec::new();
        for host in parsed.hosts {
            if host.name.trim().is_empty() || host.ssh.trim().is_empty() {
                problems.push(format!(
                    "hosts.json: host {:?} needs both name and ssh, entry ignored",
                    host.name
                ));
                continue;
            }
            if hosts.iter().any(|kept| kept.name == host.name) {
                problems.push(format!(
                    "hosts.json: duplicate host {}, later entry ignored",
                    host.name
                ));
                continue;
            }
            hosts.push(host);
        }
        let failure = (!problems.is_empty()).then(|| problems.join("; "));
        (Self { hosts }, failure)
    }
}

/// Rows and per-source problems from one host listing.
///
/// A source that cannot be listed without a remote `llm-sessions` is a failure
/// *entry*, not a failed host: the other sources on that machine still
/// produce rows.
#[derive(Debug, Default)]
pub struct HostScan {
    pub sessions: Vec<SessionSummary>,
    /// Self-describing messages: a host-wide problem names the machine.
    pub failures: Vec<String>,
}

/// One remote agent process, exactly as `llm-sessions top --json` emits it.
#[derive(Clone, Debug, Deserialize)]
pub struct RemoteProcess {
    pub pid: u32,
    pub agent: SourceId,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub activity: Activity,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub exact: bool,
    #[serde(default)]
    pub pane: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

pub struct Host {
    config: HostConfig,
    /// Learned once: remote home, whether `llm-sessions` and `sqlite3` exist.
    probe: OnceLock<std::result::Result<Probe, String>>,
    /// Canned rows, so the scan's merge path is testable without an ssh.
    #[cfg(test)]
    canned: Option<Vec<SessionSummary>>,
}

impl Host {
    pub fn new(config: HostConfig) -> Self {
        Self {
            config,
            probe: OnceLock::new(),
            #[cfg(test)]
            canned: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn fake(name: &str, sessions: Vec<SessionSummary>) -> Self {
        Self {
            config: HostConfig {
                name: name.to_owned(),
                ssh: name.to_owned(),
                home: None,
                sources: Vec::new(),
                mode: Mode::Shell,
            },
            probe: OnceLock::new(),
            canned: Some(sessions),
        }
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    pub fn destination(&self) -> &str {
        &self.config.ssh
    }

    /// Agent names this host is restricted to, for `llm-sessions hosts`. Empty means
    /// every agent.
    pub fn configured_sources(&self) -> &[String] {
        &self.config.sources
    }

    /// Remote home, when the host has already been probed successfully.
    pub fn home(&self) -> Option<&Path> {
        match self.probe.get() {
            Some(Ok(probe)) => Some(probe.home.as_path()),
            _ => None,
        }
    }

    /// One line for `llm-sessions sources`: which transport this host will use, or
    /// why it cannot be reached.
    pub fn status(&self) -> String {
        match self.probe_once() {
            Err(message) => format!("unreachable: {message}"),
            Ok(probe) if self.use_llm_sessions(probe) => "llm-sessions".to_string(),
            Ok(_) => "shell".to_string(),
        }
    }

    /// Tier 1 for a whole machine. Every row comes back with
    /// `host: Some(name)`.
    pub fn list(&self, sources: &[Box<dyn Source>]) -> Result<HostScan> {
        #[cfg(test)]
        if let Some(rows) = &self.canned {
            return Ok(HostScan {
                sessions: rows.clone(),
                failures: Vec::new(),
            });
        }

        let probe = self.probed()?;
        if self.config.mode == Mode::LlmSessions && probe.llm_sessions.is_none() {
            return Err(self.fail("llm-sessions is not installed (mode = \"llm-sessions\")"));
        }
        if !self.use_llm_sessions(probe) {
            return self.list_via_shell(sources, probe);
        }
        match self.list_via_llm_sessions(probe) {
            Ok(sessions) => Ok(HostScan {
                sessions,
                failures: Vec::new(),
            }),
            // A remote of a different vintage may emit JSON this build cannot
            // read. Degrading to the shell transport keeps the machine
            // visible; failing the host would hide it entirely.
            Err(e) if self.config.mode == Mode::Auto => {
                let mut scan = self.list_via_shell(sources, probe)?;
                scan.failures.push(format!(
                    "{}: unreadable llm-sessions output, falling back to shell ({e})",
                    self.name()
                ));
                Ok(scan)
            }
            Err(e) => Err(e),
        }
    }

    /// Tier 2. `session` must be one this host produced.
    pub fn open(&self, session: &SessionSummary, home: &Path) -> Result<Transcript> {
        let probe = self.probed()?;
        if self.use_llm_sessions(probe) {
            let raw = self.ssh(&[
                probe.llm_sessions.as_deref().unwrap_or("llm-sessions"),
                "show",
                &session.id,
                "--json",
            ])?;
            if let Ok(transcript) = serde_json::from_str::<Transcript>(&raw) {
                return Ok(transcript);
            }
            if self.config.mode == Mode::LlmSessions {
                return Err(self.fail("unreadable llm-sessions show output"));
            }
        }
        match &session.locator {
            Locator::File { .. } => self.open_via_shell(session, home),
            Locator::Row { .. } => self.open_via_sql(session, home),
        }
    }

    /// Remote processes, for `llm-sessions top` and liveness.
    pub fn top(&self) -> Result<Vec<RemoteProcess>> {
        let probe = self.probed()?;
        let raw = self.ssh(&[
            probe.llm_sessions.as_deref().unwrap_or("llm-sessions"),
            "top",
            "--json",
        ])?;
        serde_json::from_str(&raw)
            .map_err(|e| self.fail(format!("unreadable llm-sessions top output: {e}")))
    }

    /// The local resume command for `session`, wrapped in an `ssh -t` that
    /// runs it on this host. Printed for inspection, never executed here.
    pub fn resume_command(
        &self,
        session: &SessionSummary,
        sources: &[Box<dyn Source>],
    ) -> Option<Vec<String>> {
        let inner = source::find(sources, session.source)?.resume_command(&session.id)?;
        let mut command = vec!["ssh".to_string(), "-t".to_string(), self.config.ssh.clone()];
        command.extend(inner);
        Some(command)
    }

    // ---- transports ----

    fn list_via_llm_sessions(&self, probe: &Probe) -> Result<Vec<SessionSummary>> {
        let raw = self.ssh(&[
            probe.llm_sessions.as_deref().unwrap_or("llm-sessions"),
            "list",
            "--json",
            "--limit",
            "0",
            "--archived",
        ])?;
        let mut sessions: Vec<SessionSummary> = serde_json::from_str(&raw)
            .map_err(|e| self.fail(format!("unreadable llm-sessions list output: {e}")))?;
        sessions.retain(|session| self.wants(session.source));
        for session in &mut sessions {
            // The remote already rolled up its own worktrees and read its own
            // `/proc`; workspace, liveness, usage and change stay as sent.
            session.host = Some(self.config.name.clone());
        }
        Ok(sessions)
    }

    fn list_via_shell(&self, sources: &[Box<dyn Source>], probe: &Probe) -> Result<HostScan> {
        let mut script = String::new();
        let mut failures: Vec<String> = Vec::new();
        let home = probe.home.to_string_lossy().into_owned();

        for src in sources {
            if !self.wants(src.id()) {
                continue;
            }
            let layout = src.remote_layout();
            let agent = src.id().name();
            if layout.files.is_empty() && layout.sql.is_none() {
                failures.push(format!(
                    "{}: {agent} sessions need llm-sessions on the host",
                    self.name()
                ));
                continue;
            }
            if !layout.files.is_empty() {
                script.push_str(&files_script(&home, agent, layout.files));
            }
            if let Some((db, queries)) = layout.sql {
                if probe.sqlite3 {
                    script.push_str(&sql_script(&home, agent, db, queries));
                } else {
                    // Reported from the host, and only when the store exists.
                    script.push_str(&missing_sqlite3_script(&home, agent, db));
                }
            }
        }

        if script.is_empty() {
            return Ok(HostScan {
                sessions: Vec::new(),
                failures,
            });
        }
        let raw = self.ssh_script(&script)?;
        let dbs: HashMap<SourceId, PathBuf> = sources
            .iter()
            .filter_map(|src| {
                src.remote_layout()
                    .sql
                    .map(|(db, _)| (src.id(), probe.home.join(db)))
            })
            .collect();
        let sessions = parse_shell_listing(&raw, self.name(), &dbs);
        failures.extend(unscanned(&raw, self.name()));
        Ok(HostScan { sessions, failures })
    }

    /// Open a database-row session by running its per-session queries on
    /// the host and transferring only those rows.
    fn open_via_sql(&self, session: &SessionSummary, home: &Path) -> Result<Transcript> {
        let Locator::Row { db, id } = &session.locator else {
            return Err(self.fail("row locator expected"));
        };
        let sources = source::all(home);
        let reader = source::find(&sources, session.source).ok_or(Error::Unsupported {
            agent: session.source,
            what: "reading transcripts",
        })?;
        let queries = reader.remote_open_sql(id);
        if queries.is_empty() {
            return Err(self.fail(format!(
                "install llm-sessions on {} to read this transcript",
                self.name()
            )));
        }
        if !self.probed()?.sqlite3 {
            return Err(self.fail("sqlite3 is not installed, cannot read this transcript"));
        }
        let raw = self.ssh_script(&open_sql_script(&db.to_string_lossy(), &queries))?;
        let rows = parse::parse_open_rows(&raw);
        reader.open_remote_rows(db, id, &rows)
    }

    /// Fetch one remote body into the local cache, then hand it to the local
    /// adapter for that agent so all nine parsers are reused verbatim.
    fn open_via_shell(&self, session: &SessionSummary, home: &Path) -> Result<Transcript> {
        let Locator::File { path } = &session.locator else {
            return Err(self.fail("file locator expected"));
        };
        if session.size_bytes.is_none_or(|size| size > REMOTE_BODY_MAX) {
            return Err(self.fail(format!(
                "install llm-sessions on {} to read this transcript",
                self.name()
            )));
        }

        let remote = path.to_string_lossy().into_owned();
        let body = self.ssh(&["cat", &remote])?;
        let cached = cache_path(home, self.name(), path);
        if let Some(parent) = cached.parent() {
            std::fs::create_dir_all(parent).map_err(crate::error::io(parent))?;
        }
        std::fs::write(&cached, body).map_err(crate::error::io(&cached))?;

        let sources = source::all(home);
        let reader = source::find(&sources, session.source).ok_or(Error::Unsupported {
            agent: session.source,
            what: "reading transcripts",
        })?;
        reader.open(&Locator::File { path: cached })
    }

    // ---- invocation ----

    fn wants(&self, id: SourceId) -> bool {
        self.config.sources.is_empty()
            || self
                .config
                .sources
                .iter()
                .any(|name| name.eq_ignore_ascii_case(id.name()))
    }

    fn use_llm_sessions(&self, probe: &Probe) -> bool {
        match self.config.mode {
            Mode::Shell => false,
            Mode::LlmSessions => true,
            Mode::Auto => probe.llm_sessions.is_some(),
        }
    }

    fn fail(&self, message: impl Into<String>) -> Error {
        Error::Remote {
            host: self.config.name.clone(),
            message: message.into(),
        }
    }
}

/// `M\t<agent>` records: a store exists on the host but `sqlite3` does not,
/// so that agent was not scanned. Neutral wording: nothing is broken.
fn unscanned(raw: &str, host: &str) -> Vec<String> {
    raw.lines()
        .filter_map(|line| line.strip_prefix("M\t"))
        .map(|agent| format!("{host}: {} not scanned (sqlite3 missing)", agent.trim()))
        .collect()
}

/// Where a fetched remote body is cached, honouring `$XDG_CACHE_HOME`.
fn cache_path(home: &Path, host: &str, remote: &Path) -> PathBuf {
    let sanitised: String = remote
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Remote paths are long; keep the tail, which carries the session id.
    let tail: String = sanitised
        .chars()
        .rev()
        .take(120)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    std::env::var_os("XDG_CACHE_HOME")
        .map_or_else(|| home.join(".cache"), PathBuf::from)
        .join("llm-sessions/remote")
        .join(host)
        .join(tail)
}

/// One remote transcript fetch in flight.
///
/// Keyed by the row it was started for: an `ssh` that lands after the
/// selection moved on is dropped, never drawn against another session.
struct Pending {
    key: (SourceId, String),
    host: String,
    worker: thread::JoinHandle<Result<Transcript>>,
}

/// Remote rows and bodies for the TUI: fetched off-thread, never blocking a
/// frame.
///
/// The TUI rescans every two seconds. One `ssh` per host per tick would be
/// unusable, so the panes read this cache and a worker refreshes it. Opening
/// a remote transcript is the same bargain: the round trip happens on a
/// thread and the reader shows it when it arrives.
pub struct Cache {
    hosts: Arc<Vec<HostConfig>>,
    home: PathBuf,
    sessions: Vec<SessionSummary>,
    failures: Vec<String>,
    /// Problems with `hosts.json` itself, which no refresh can clear.
    base: Vec<String>,
    fetched: Option<Instant>,
    worker: Option<thread::JoinHandle<(Vec<SessionSummary>, Vec<String>)>>,
    /// One [`Host`] per name, kept so its probe — an extra round trip — is
    /// paid once per process rather than once per transcript.
    openers: HashMap<String, Arc<Host>>,
    /// The transcript fetch the reader is waiting for, if any.
    pending: Option<Pending>,
}

impl Cache {
    /// A cache with no hosts, for TUI tests: they must not read (let alone
    /// ssh to) whatever the ambient `$XDG_CONFIG_HOME` points at.
    #[cfg(test)]
    pub(crate) fn empty() -> (Self, Vec<String>) {
        (
            Self {
                hosts: Arc::new(Vec::new()),
                home: PathBuf::new(),
                sessions: Vec::new(),
                failures: Vec::new(),
                base: Vec::new(),
                fetched: None,
                worker: None,
                openers: HashMap::new(),
                pending: None,
            },
            Vec::new(),
        )
    }

    pub fn new(home: &Path) -> (Self, Vec<String>) {
        let (config, failure) = Config::load(home);
        let base: Vec<String> = failure.into_iter().collect();
        let cache = Self {
            hosts: Arc::new(config.hosts),
            home: home.to_path_buf(),
            sessions: Vec::new(),
            failures: base.clone(),
            base: base.clone(),
            fetched: None,
            worker: None,
            openers: HashMap::new(),
            pending: None,
        };
        (cache, base)
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }

    /// The configured host a row belongs to, for resume and open routing.
    pub fn host(&self, name: &str) -> Option<&HostConfig> {
        self.hosts.iter().find(|host| host.name == name)
    }

    /// Collect a finished refresh and start another when stale. `force`
    /// ignores the TTL. Returns true when rows changed.
    pub fn poll(&mut self, force: bool) -> bool {
        let mut changed = false;
        if self
            .worker
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished)
        {
            let worker = self.worker.take().expect("finished worker");
            if let Ok((sessions, failures)) = worker.join() {
                self.sessions = sessions;
                self.failures = self.base.iter().cloned().chain(failures).collect();
                changed = true;
            }
            self.fetched = Some(Instant::now());
        }

        let stale = self.fetched.is_none_or(|at| at.elapsed() >= REFRESH);
        // A refresh still running when the next one comes due is left alone.
        if (force || stale) && self.worker.is_none() && !self.hosts.is_empty() {
            let hosts = Arc::clone(&self.hosts);
            let home = self.home.clone();
            self.worker = Some(thread::spawn(move || refresh(&hosts, &home)));
        }
        changed
    }

    pub fn sessions(&self) -> &[SessionSummary] {
        &self.sessions
    }

    pub fn failures(&self) -> &[String] {
        &self.failures
    }

    /// Start fetching one remote transcript off-thread, replacing a fetch
    /// still in flight: only the selected row's body is ever wanted.
    ///
    /// `Err` is a configuration problem no round trip can fix — the row names
    /// a host this process does not have.
    pub fn request_transcript(
        &mut self,
        session: &SessionSummary,
    ) -> std::result::Result<(), String> {
        let name = session.host.clone().unwrap_or_default();
        let host = if let Some(host) = self.openers.get(&name) {
            Arc::clone(host)
        } else {
            let config = self
                .host(&name)
                .cloned()
                .ok_or_else(|| format!("{name}: no host with this name in hosts.json"))?;
            let host = Arc::new(Host::new(config));
            self.openers.insert(name.clone(), Arc::clone(&host));
            host
        };
        let key = (session.source, session.id.clone());
        let session = session.clone();
        let home = self.home.clone();
        self.pending = Some(Pending {
            key,
            host: name,
            worker: thread::spawn(move || host.open(&session, &home)),
        });
        Ok(())
    }

    /// Whether a transcript fetch is in flight, for the reader's own state.
    pub fn fetching(&self) -> bool {
        self.pending.is_some()
    }

    /// Collect a finished transcript fetch, with the row it belongs to.
    /// `None` while one is still running, or when none was started.
    pub fn poll_transcript(&mut self) -> Option<((SourceId, String), Result<Transcript>)> {
        if !self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.worker.is_finished())
        {
            return None;
        }
        let pending = self.pending.take().expect("finished fetch");
        let result = pending.worker.join().unwrap_or_else(|_| {
            Err(Error::Remote {
                host: pending.host.clone(),
                message: "reading the transcript panicked".to_string(),
            })
        });
        Some((pending.key, result))
    }
}

/// List every configured host, one thread each, so one unreachable machine
/// cannot delay the others.
fn refresh(hosts: &[HostConfig], home: &Path) -> (Vec<SessionSummary>, Vec<String>) {
    let sources = source::all(home);
    thread::scope(|scope| {
        let handles: Vec<_> = hosts
            .iter()
            .map(|config| {
                let sources = &sources;
                scope.spawn(move || Host::new(config.clone()).list(sources))
            })
            .collect();
        let mut sessions = Vec::new();
        let mut failures = Vec::new();
        for handle in handles {
            match handle.join() {
                Ok(Ok(mut scan)) => {
                    sessions.append(&mut scan.sessions);
                    failures.append(&mut scan.failures);
                }
                Ok(Err(e)) => failures.push(e.to_string()),
                Err(_) => failures.push("a host listing panicked".to_string()),
            }
        }
        (sessions, failures)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_sqlite3_is_reported_neutrally_per_agent() {
        let raw = "S\topencode\t\nM\tcopilot\nnoise from a login banner\n";
        assert_eq!(
            unscanned(raw, "ar-2"),
            vec!["ar-2: copilot not scanned (sqlite3 missing)".to_string()]
        );
        assert!(unscanned("S\tcopilot\t\n", "ar-2").is_empty());
    }

    #[test]
    fn every_source_is_listable_without_a_remote_binary() {
        let home = tempfile::tempdir().unwrap();
        for src in source::all(home.path()) {
            let layout = src.remote_layout();
            assert!(
                !layout.files.is_empty() || layout.sql.is_some(),
                "{} has no remote layout",
                src.id().name()
            );
        }
    }

    #[test]
    fn a_duplicate_host_is_reported_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.json");
        std::fs::write(
            &path,
            r#"{"hosts":[{"name":"a","ssh":"a"},{"name":"a","ssh":"b"}]}"#,
        )
        .unwrap();

        let (config, failure) = Config::load_at(&path);
        assert_eq!(config.hosts.len(), 1);
        assert_eq!(config.hosts[0].ssh, "a", "the first entry wins");
        let failure = failure.expect("a duplicate must be reported");
        assert!(failure.contains("duplicate host a"), "{failure}");
    }

    #[test]
    fn a_saved_config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.json");
        let config = Config {
            hosts: vec![HostConfig {
                name: "nx-1".into(),
                ssh: "p1g3-nx-1".into(),
                home: Some(PathBuf::from("/var/lib/hermes")),
                sources: vec!["hermes".into()],
                mode: Mode::Shell,
            }],
        };
        Config::save_at(&config, &path).unwrap();
        let (loaded, failure) = Config::load_at(&path);
        assert_eq!(failure, None);
        assert_eq!(loaded.hosts.len(), 1);
        assert_eq!(loaded.hosts[0].ssh, "p1g3-nx-1");
        assert_eq!(loaded.hosts[0].mode, Mode::Shell);
        assert_eq!(loaded.hosts[0].sources, vec!["hermes".to_string()]);
    }
}
