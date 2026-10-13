//! Sessions on other machines, reached over SSH.
//!
//! A federation-capable peer receives guarded, origin-qualified JSON requests.
//! Older binaries and shell-only peers are local-store leaves; ordinary remote
//! `list` and `show` are never compatibility fallbacks. Both transports read the
//! same provisioned origin marker, and every successful observation retains an
//! explicit access route separate from its origin display label.
//!
//! Neither transport copies a store over the wire. A file listing costs one
//! `ssh` round trip per host, carrying at most `REMOTE_FILES_PER_SOURCE`
//! heads of `REMOTE_HEAD_BYTES` bytes.

mod parse;
mod transport;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

pub use parse::parse_shell_listing;
use serde::{Deserialize, Serialize};
use transport::{Probe, files_script, missing_sqlite3_script, open_sql_script, sql_script};

use crate::error::{Error, Result};
use crate::federation::{DiscoveryContext, FederationScan};
use crate::model::{
    AccessRoute, Locator, RouteHop, SessionKey, SessionSummary, SourceId, Transcript,
};
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
        crate::config_home(home).join("llm-sessions/hosts.json")
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
    #[cfg(test)]
    scripted: Option<transport::Scripted>,
}

fn valid_route(hops: &[RouteHop], origin: &str) -> bool {
    !hops.is_empty()
        && u32::try_from(hops.len()).is_ok_and(|length| length <= crate::federation::DEFAULT_HOPS)
        && hops.last().is_some_and(|hop| hop.origin_id == origin)
        && hops.iter().enumerate().all(|(index, hop)| {
            crate::federation::valid_origin(&hop.origin_id)
                && !hop.ssh.is_empty()
                && !hop.ssh.starts_with('-')
                && !hops[..index]
                    .iter()
                    .any(|previous| previous.origin_id == hop.origin_id)
        })
}

/// Resume at the actual origin, preserving every SSH shell parsing layer.
pub fn resume_command(
    session: &SessionSummary,
    sources: &[Box<dyn Source>],
) -> Option<Vec<String>> {
    let mut command = source::find(sources, session.source)?.resume_command(&session.id)?;
    if session.is_local() {
        return Some(command);
    }
    let route = session.preferred_route()?;
    if !valid_route(&route.hops, session.origin_id.as_deref()?) {
        return None;
    }
    for hop in route.hops.iter().rev() {
        let remote = if let Some(home) = &hop.home {
            let environment = transport::home_environment(home);
            transport::shell_command(
                std::iter::once("env")
                    .chain(environment.iter().map(String::as_str))
                    .chain(command.iter().map(String::as_str)),
            )
        } else {
            transport::shell_command(command.iter().map(String::as_str))
        };
        command = vec![
            "ssh".into(),
            "-t".into(),
            "--".into(),
            hop.ssh.clone(),
            remote,
        ];
    }
    Some(command)
}

impl Host {
    pub fn new(config: HostConfig) -> Self {
        Self {
            config,
            probe: OnceLock::new(),
            #[cfg(test)]
            canned: None,
            #[cfg(test)]
            scripted: None,
        }
    }

    pub fn from_route_hop(hop: &RouteHop) -> Self {
        Self::new(HostConfig {
            name: hop.name.clone(),
            ssh: hop.ssh.clone(),
            home: hop.home.clone(),
            sources: Vec::new(),
            mode: Mode::Auto,
        })
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
            scripted: None,
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

    /// Discover a peer with a request-local ancestor path. Standalone callers
    /// read their own marker first; a missing identity never permits recursion.
    pub fn list(&self, sources: &[Box<dyn Source>]) -> Result<HostScan> {
        #[cfg(test)]
        if let Some(rows) = &self.canned {
            return Ok(HostScan {
                sessions: rows.clone(),
                failures: Vec::new(),
            });
        }
        let origin = crate::federation::read_origin(&crate::home()?)?;
        self.list_with(
            sources,
            &DiscoveryContext {
                visited: vec![origin],
                ..DiscoveryContext::default()
            },
        )
    }

    /// `context.visited` already includes the calling instance. Each branch
    /// borrows that immutable value; only the receiving endpoint extends it.
    pub fn list_with(
        &self,
        sources: &[Box<dyn Source>],
        context: &DiscoveryContext,
    ) -> Result<HostScan> {
        if context.remaining_hops == 0 {
            return Ok(HostScan {
                sessions: Vec::new(),
                failures: vec![format!(
                    "{}: federation hop limit reached; discovery truncated",
                    self.name()
                )],
            });
        }
        #[cfg(test)]
        if let Some(rows) = &self.canned {
            return Ok(HostScan {
                sessions: rows.clone(),
                failures: Vec::new(),
            });
        }
        let probe = self.probed()?;
        if context.visited.contains(&probe.origin_id) {
            return Ok(HostScan::default());
        }
        if !self.use_llm_sessions(probe) {
            let mut scan = self.list_via_shell(sources, probe)?;
            if !probe.federation {
                scan.failures.push(format!(
                    "{}: federation protocol unavailable; shell local-only listing (no chained discovery)",
                    self.name()
                ));
            }
            return Ok(scan);
        }
        self.list_via_llm_sessions(probe, context)
    }

    /// Open through the selected route, qualified by origin and source.
    pub fn open(&self, session: &SessionSummary, home: &Path) -> Result<Transcript> {
        let route = session
            .preferred_route()
            .ok_or_else(|| self.fail("remote session has no access route"))?;
        let first = route
            .hops
            .first()
            .ok_or_else(|| self.fail("remote session has an empty access route"))?;
        let origin = session
            .origin_id
            .as_deref()
            .ok_or_else(|| self.fail("canonical origin identity unavailable"))?;
        if !valid_route(&route.hops, origin) {
            return Err(self
                .fail("invalid access route: malformed, cyclic, over-depth, or incorrect target"));
        }
        let probe = self.probed()?;
        if first.ssh != self.config.ssh
            || first.home != self.config.home
            || first.origin_id != probe.origin_id
        {
            return Err(self.fail("access route does not match the probed peer origin"));
        }
        if self.use_llm_sessions(probe) {
            let remaining = serde_json::to_string(&route.hops[1..])?;
            let raw = self.ssh_binary(
                &probe.home,
                &[
                    probe.llm_sessions.as_deref().unwrap_or("llm-sessions"),
                    "federation",
                    "show",
                    "--origin",
                    origin,
                    "--source",
                    session.source.name(),
                    "--id",
                    &session.id,
                    "--route",
                    &remaining,
                ],
            )?;
            return serde_json::from_str(&raw)
                .map_err(|error| self.fail(format!("unreadable federation show output: {error}")));
        }
        if route.hops.len() != 1 || origin != probe.origin_id {
            return Err(self.fail(
                "federation protocol unavailable: cannot delegate transcript to an indirect origin",
            ));
        }
        match &session.locator {
            Locator::File { .. } => self.open_via_shell(session, home),
            Locator::Row { .. } => self.open_via_sql(session, home),
        }
    }

    /// Remote processes, for `llm-sessions top` and liveness.
    pub fn top(&self) -> Result<Vec<RemoteProcess>> {
        let probe = self.probed()?;
        if !self.use_llm_sessions(probe) {
            return Err(
                self.fail("federation protocol unavailable: cannot query local-only processes")
            );
        }
        let raw = self.ssh_binary(
            &probe.home,
            &[
                probe.llm_sessions.as_deref().unwrap_or("llm-sessions"),
                "federation",
                "top",
            ],
        )?;
        serde_json::from_str(&raw)
            .map_err(|e| self.fail(format!("unreadable llm-sessions top output: {e}")))
    }

    /// Stable origin identity learned by the probe, for origin-qualified
    /// output. Probing happens once per process, so a caller that already
    /// used this host pays nothing extra.
    pub fn origin_id(&self) -> Result<&str> {
        Ok(&self.probed()?.origin_id)
    }

    /// The local resume command for `session`, wrapped in an `ssh -t` that
    /// runs it on this host. Printed for inspection, never executed here.
    pub fn resume_command(
        &self,
        session: &SessionSummary,
        sources: &[Box<dyn Source>],
    ) -> Option<Vec<String>> {
        resume_command(session, sources)
    }

    // ---- transports ----

    fn list_via_llm_sessions(&self, probe: &Probe, context: &DiscoveryContext) -> Result<HostScan> {
        let path = serde_json::to_string(&context.visited)?;
        let hops = (context.remaining_hops - 1).to_string();
        let raw = self.ssh_binary(
            &probe.home,
            &[
                probe.llm_sessions.as_deref().unwrap_or("llm-sessions"),
                "federation",
                "list",
                "--path",
                &path,
                "--hops",
                &hops,
            ],
        )?;
        let mut scan: FederationScan = serde_json::from_str(&raw)
            .map_err(|error| self.fail(format!("unreadable federation list output: {error}")))?;
        scan.sessions.retain(|session| self.wants(session.source));
        for session in &mut scan.sessions {
            self.attach_route(session, probe)?;
        }
        Ok(HostScan {
            sessions: scan.sessions,
            failures: scan.failures,
        })
    }

    fn attach_route(&self, session: &mut SessionSummary, probe: &Probe) -> Result<()> {
        let origin = session
            .origin_id
            .as_deref()
            .filter(|origin| crate::federation::valid_origin(origin))
            .ok_or_else(|| self.fail("received session has no valid canonical origin identity"))?;
        let hop = RouteHop {
            origin_id: probe.origin_id.clone(),
            name: self.config.name.clone(),
            ssh: self.config.ssh.clone(),
            home: self.config.home.clone(),
        };
        if session.routes.is_empty() {
            if origin != probe.origin_id {
                return Err(self.fail("received indirect session has no access route"));
            }
            session.routes.push(AccessRoute { hops: vec![hop] });
        } else {
            for route in &mut session.routes {
                route.hops.insert(0, hop.clone());
            }
        }
        if session
            .routes
            .iter()
            .any(|route| !valid_route(&route.hops, origin))
        {
            return Err(self.fail(
                "received invalid access route: malformed, cyclic, over-depth, or incorrect target",
            ));
        }
        if origin == probe.origin_id {
            session.origin_label = Some(self.config.name.clone());
        }
        Ok(())
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
        let mut sessions = parse_shell_listing(&raw, self.name(), &dbs);
        for session in &mut sessions {
            session.origin_id = Some(probe.origin_id.clone());
            self.attach_route(session, probe)?;
        }
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
        self.config.mode != Mode::Shell && probe.federation && probe.llm_sessions.is_some()
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
    crate::cache_home(home)
        .join("llm-sessions/remote")
        .join(host)
        .join(tail)
}

/// One remote transcript fetch in flight.
///
/// Keyed by the row it was started for: an `ssh` that lands after the
/// selection moved on is dropped, never drawn against another session.
struct Pending {
    key: SessionKey,
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
    /// One opener per explicit first hop, preserving probes between fetches.
    openers: BTreeMap<RouteHop, Arc<Host>>,
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
                openers: BTreeMap::new(),
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
            openers: BTreeMap::new(),
            pending: None,
        };
        (cache, base)
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
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
    /// `Err` means the row has no usable explicit access route.
    pub fn request_transcript(
        &mut self,
        session: &SessionSummary,
    ) -> std::result::Result<(), String> {
        let hop = session
            .preferred_route()
            .and_then(|route| route.hops.first())
            .ok_or_else(|| "remote session has no access route".to_string())?
            .clone();
        let host = if let Some(host) = self.openers.get(&hop) {
            Arc::clone(host)
        } else {
            let host = Arc::new(Host::from_route_hop(&hop));
            self.openers.insert(hop.clone(), Arc::clone(&host));
            host
        };
        let key = session.identity();
        let session = session.clone();
        let home = self.home.clone();
        self.pending = Some(Pending {
            key,
            host: hop.name,
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
    pub fn poll_transcript(&mut self) -> Option<(SessionKey, Result<Transcript>)> {
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
    let origin = match crate::federation::read_origin(home) {
        Ok(origin) => origin,
        Err(error) => return (Vec::new(), vec![error.to_string()]),
    };
    let context = DiscoveryContext {
        visited: vec![origin],
        ..DiscoveryContext::default()
    };
    thread::scope(|scope| {
        let handles: Vec<_> = hosts
            .iter()
            .map(|config| {
                let sources = &sources;
                let context = &context;
                scope.spawn(move || Host::new(config.clone()).list_with(sources, context))
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
        (crate::catalog::merge::merge(sessions), failures)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: &str = "00000000-0000-4000-8000-000000000001";
    const A: &str = "00000000-0000-4000-8000-000000000002";
    const B: &str = "00000000-0000-4000-8000-000000000003";
    const C: &str = "00000000-0000-4000-8000-000000000004";

    fn hop(origin: &str, name: &str) -> RouteHop {
        RouteHop {
            origin_id: origin.into(),
            name: name.into(),
            ssh: format!("user@{name}"),
            home: None,
        }
    }

    fn row(origin: &str) -> SessionSummary {
        let mut row = SessionSummary::new(
            "same-id".into(),
            SourceId::Omp,
            time::OffsetDateTime::UNIX_EPOCH,
            Locator::File {
                path: PathBuf::from("/remote/session.jsonl"),
            },
        );
        row.origin_id = Some(origin.into());
        row
    }

    fn scripted_host(origin: &str, name: &str, guarded: bool, replies: Vec<String>) -> Host {
        let mut host = Host::from_route_hop(&hop(origin, name));
        host.probe
            .set(Ok(Probe {
                home: PathBuf::from("/remote"),
                llm_sessions: Some("/remote/bin/llm-sessions".into()),
                sqlite3: true,
                origin_id: origin.into(),
                federation: guarded,
            }))
            .unwrap();
        host.scripted = Some(transport::Scripted {
            replies: std::sync::Mutex::new(replies.into_iter().map(Ok).collect()),
            ..transport::Scripted::default()
        });
        host
    }

    fn calls(host: &Host) -> Vec<Vec<String>> {
        host.scripted
            .as_ref()
            .unwrap()
            .calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn wire(sessions: Vec<SessionSummary>) -> String {
        serde_json::to_string(&FederationScan {
            sessions,
            failures: Vec::new(),
        })
        .unwrap()
    }

    #[test]
    fn configured_home_is_carried_through_probe_routes_and_guarded_operations() {
        let home = PathBuf::from("/remote/home with ' spaces");
        let mut route_hop = hop(A, "a");
        route_hop.home = Some(home.clone());
        let mut host = Host::from_route_hop(&route_hop);
        let transcript = Transcript::new("same-id", SourceId::Omp);
        host.scripted = Some(transport::Scripted {
            replies: std::sync::Mutex::new(
                [
                    Ok(format!(
                        "home:{}\norigin:{A}\nllm-sessions:/bin/peer\n",
                        home.display()
                    )),
                    Ok(format!(r#"{{"version":1,"origin_id":"{A}"}}"#)),
                    Ok(wire(vec![row(A)])),
                    Ok(serde_json::to_string(&transcript).unwrap()),
                    Ok("[]".into()),
                ]
                .into_iter()
                .collect(),
            ),
            ..transport::Scripted::default()
        });
        let scan = host.list_with(&[], &DiscoveryContext::default()).unwrap();
        assert_eq!(scan.sessions[0].routes[0].hops[0], route_hop);
        assert_eq!(
            Host::from_route_hop(&scan.sessions[0].routes[0].hops[0])
                .config
                .home,
            Some(home.clone())
        );
        assert_eq!(
            host.open(&scan.sessions[0], Path::new("/local"))
                .unwrap()
                .id,
            "same-id"
        );
        assert!(host.top().unwrap().is_empty());
        let commands = calls(&host);
        let environment = transport::home_environment(&home);
        for (command, operation) in
            commands[1..]
                .iter()
                .zip(["capabilities", "list", "show", "top"])
        {
            assert_eq!(command[0], "env");
            assert_eq!(&command[1..6], &environment);
            assert_eq!(&command[6..9], &["/bin/peer", "federation", operation]);
        }
    }

    #[test]
    fn multihop_resume_preserves_each_configured_home_namespace() {
        let home_a = PathBuf::from("/namespace a with ' quotes");
        let home_b = PathBuf::from("/namespace b with spaces");
        let mut first = hop(A, "a");
        first.home = Some(home_a.clone());
        let mut final_hop = hop(B, "b");
        final_hop.home = Some(home_b.clone());
        let mut session = row(B);
        session.origin_label = Some("origin-b".into());
        session.routes = vec![AccessRoute {
            hops: vec![first, final_hop],
        }];
        let sources = source::all(Path::new("/local"));
        let command = resume_command(&session, &sources).unwrap();
        let first_remote = shell_words(&command[4]);
        assert_eq!(first_remote[0], "env");
        assert_eq!(&first_remote[1..6], &transport::home_environment(&home_a));
        assert_eq!(&first_remote[6..10], &["ssh", "-t", "--", "user@b"]);
        let final_remote = shell_words(&first_remote[10]);
        assert_eq!(final_remote[0], "env");
        assert_eq!(&final_remote[1..6], &transport::home_environment(&home_b));
        assert_eq!(
            &final_remote[6..],
            &source::find(&sources, session.source)
                .unwrap()
                .resume_command(&session.id)
                .unwrap()
        );
    }

    #[test]
    fn known_ancestors_and_aliases_never_receive_a_session_query() {
        let context = DiscoveryContext {
            visited: vec![LOCAL.into(), A.into(), B.into()],
            remaining_hops: 8,
        };
        for ancestor in [LOCAL, A, B] {
            for alias in ["first-alias", "other-alias"] {
                let host = scripted_host(ancestor, alias, true, Vec::new());
                let scan = host.list_with(&[], &context).unwrap();
                assert!(scan.sessions.is_empty());
                assert!(scan.failures.is_empty());
                assert!(calls(&host).is_empty());
            }
        }
        assert_eq!(context.visited, vec![LOCAL, A, B]);
    }

    #[test]
    fn depth_zero_does_not_even_probe_and_reports_truncation() {
        let host = scripted_host(A, "a", true, Vec::new());
        let context = DiscoveryContext {
            visited: vec![LOCAL.into()],
            remaining_hops: 0,
        };
        let scan = host.list_with(&[], &context).unwrap();
        assert!(scan.sessions.is_empty());
        assert!(scan.failures[0].contains("truncated"));
        assert!(calls(&host).is_empty());
    }

    #[test]
    fn background_refresh_without_caller_identity_never_contacts_peers() {
        let home = tempfile::tempdir().unwrap();
        let config = HostConfig {
            name: "must-not-contact".into(),
            ssh: "must-not-contact".into(),
            home: None,
            sources: Vec::new(),
            mode: Mode::Auto,
        };
        let (sessions, failures) = refresh(&[config], home.path());
        assert!(sessions.is_empty());
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("identity unavailable"));
        assert!(
            !home
                .path()
                .join(".local/state/llm-sessions/origin-id")
                .exists()
        );
    }

    #[test]
    fn independent_diamond_branches_keep_origin_routes_and_immutable_paths() {
        let context = DiscoveryContext {
            visited: vec![LOCAL.into()],
            remaining_hops: 5,
        };
        let mut forwarded = row(B);
        forwarded.origin_label = Some("origin-b".into());
        forwarded.routes = vec![AccessRoute {
            hops: vec![hop(B, "b")],
        }];
        let a = scripted_host(A, "a", true, vec![wire(vec![forwarded.clone()])]);
        let c = scripted_host(C, "c", true, vec![wire(vec![forwarded])]);
        let direct = scripted_host(B, "direct-b", true, vec![wire(vec![row(B)])]);
        let mut rows = Vec::new();
        for host in [&a, &c, &direct] {
            let scan = host.list_with(&[], &context).unwrap();
            assert_eq!(scan.sessions[0].origin_id.as_deref(), Some(B));
            if host.name() != "direct-b" {
                assert_eq!(scan.sessions[0].origin_label.as_deref(), Some("origin-b"));
            }
            let command = &calls(host)[0];
            assert_eq!(&command[1..4], &["federation", "list", "--path"]);
            assert_eq!(command[4], serde_json::to_string(&context.visited).unwrap());
            assert_eq!(&command[5..], &["--hops", "4"]);
            rows.extend(scan.sessions);
        }
        let merged = crate::catalog::merge::merge(rows);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].routes.len(), 3);
        assert_eq!(merged[0].origin_label.as_deref(), Some("direct-b"));
        assert_eq!(
            merged[0].preferred_route().unwrap().hops,
            vec![hop(B, "direct-b")]
        );
        assert_eq!(context.visited, vec![LOCAL]);
    }

    #[test]
    fn older_binary_is_a_shell_leaf_and_missing_markers_stop_before_capabilities() {
        let mut host = Host::from_route_hop(&hop(A, "old"));
        host.scripted = Some(transport::Scripted {
            replies: std::sync::Mutex::new([
                Ok(format!("home:/remote\norigin:{A}\nllm-sessions:/bin/old\n")),
                Err("unknown command federation".into()),
                Ok("F\tomp\t/remote/s.jsonl\t1700000000\t30\nH\t/remote/s.jsonl\teyJ0eXBlIjoic2Vzc2lvbiIsImlkIjoicyJ9\n".into()),
            ].into_iter().collect()),
            ..transport::Scripted::default()
        });
        let sources = source::all(Path::new("/local"));
        let scan = host
            .list_with(&sources, &DiscoveryContext::default())
            .unwrap();
        assert!(
            scan.failures
                .iter()
                .any(|failure| failure.contains("local-only"))
        );
        assert_eq!(scan.sessions.len(), 1);
        assert_eq!(scan.sessions[0].origin_id.as_deref(), Some(A));
        assert_eq!(scan.sessions[0].routes[0].hops, vec![hop(A, "old")]);
        assert_eq!(
            calls(&host),
            vec![
                vec![String::from("sh"), String::from("-s")],
                vec![
                    String::from("/bin/old"),
                    String::from("federation"),
                    String::from("capabilities")
                ],
                vec![String::from("sh"), String::from("-s")],
            ]
        );

        let mut missing = Host::from_route_hop(&hop(B, "missing"));
        missing.scripted = Some(transport::Scripted {
            replies: std::sync::Mutex::new(
                [Ok("home:/remote\nllm-sessions:/bin/old\n".into())]
                    .into_iter()
                    .collect(),
            ),
            ..transport::Scripted::default()
        });
        assert!(
            missing
                .list_with(&sources, &DiscoveryContext::default())
                .unwrap_err()
                .to_string()
                .contains("identity unavailable")
        );
        assert_eq!(calls(&missing).len(), 1);
    }

    #[test]
    fn malformed_guarded_output_never_falls_back_to_unguarded_discovery() {
        let host = scripted_host(A, "a", true, vec!["not JSON".into()]);
        assert!(host.list_with(&[], &DiscoveryContext::default()).is_err());
        assert_eq!(calls(&host).len(), 1);
        assert_eq!(&calls(&host)[0][1..3], &["federation", "list"]);
    }

    #[test]
    fn a_peer_local_zero_hop_sentinel_becomes_a_direct_access_route() {
        let host = scripted_host(A, "a", true, Vec::new());
        let mut session = row(A);
        session.routes.push(AccessRoute { hops: Vec::new() });
        host.attach_route(&mut session, host.probed().unwrap())
            .unwrap();
        assert_eq!(session.origin_id.as_deref(), Some(A));
        assert_eq!(session.origin_label.as_deref(), Some("a"));
        assert_eq!(session.preferred_route().unwrap().hops, vec![hop(A, "a")]);
        assert!(!session.is_local());
    }

    #[test]
    fn received_routes_reject_cycles_invalid_hops_and_incorrect_targets() {
        let host = scripted_host(A, "a", true, Vec::new());
        let probe = host.probed().unwrap();
        let mut option_hop = hop(B, "b");
        option_hop.ssh = "-oProxyCommand=wrong".into();
        let mut empty_destination = hop(B, "b");
        empty_destination.ssh.clear();
        let mut too_long: Vec<_> = (10..18)
            .map(|id| hop(&format!("00000000-0000-4000-8000-{id:012}"), "intermediate"))
            .collect();
        too_long.push(hop(B, "b"));
        for hops in [
            vec![hop(B, "b"), hop(A, "a"), hop(B, "b")],
            vec![hop("not-a-uuid", "invalid"), hop(B, "b")],
            vec![hop(C, "incorrect-target")],
            Vec::new(),
            vec![option_hop],
            vec![empty_destination],
            too_long,
        ] {
            let mut session = row(B);
            session.routes.push(AccessRoute { hops });
            assert!(host.attach_route(&mut session, probe).is_err());
        }
        assert!(calls(&host).is_empty());
    }

    #[test]
    fn capability_identity_mismatch_stops_before_listing() {
        let mut host = Host::from_route_hop(&hop(A, "a"));
        host.scripted = Some(transport::Scripted {
            replies: std::sync::Mutex::new(
                [
                    Ok(format!(
                        "home:/remote\norigin:{A}\nllm-sessions:/bin/peer\n"
                    )),
                    Ok(format!(r#"{{"version":1,"origin_id":"{B}"}}"#)),
                ]
                .into_iter()
                .collect(),
            ),
            ..transport::Scripted::default()
        });
        assert!(
            host.list_with(&[], &DiscoveryContext::default())
                .unwrap_err()
                .to_string()
                .contains("does not match")
        );
        assert_eq!(calls(&host).len(), 2);
    }

    #[test]
    fn a_failed_branch_does_not_hide_a_working_independent_route() {
        let context = DiscoveryContext {
            visited: vec![LOCAL.into()],
            remaining_hops: 3,
        };
        let failed = scripted_host(A, "a", true, vec!["invalid output".into()]);
        let working = scripted_host(B, "b", true, vec![wire(vec![row(B)])]);
        assert!(failed.list_with(&[], &context).is_err());
        let scan = working.list_with(&[], &context).unwrap();
        assert_eq!(scan.sessions.len(), 1);
        assert_eq!(scan.sessions[0].origin_id.as_deref(), Some(B));
        assert_eq!(context.visited, vec![LOCAL]);
    }

    #[test]
    fn delegated_show_qualifies_origin_source_id_and_consumes_one_hop() {
        let mut session = row(B);
        session.id = "same id'$(false)".into();
        session.origin_label = Some("not-a-local-ssh-label".into());
        session.routes = vec![AccessRoute {
            hops: vec![hop(A, "a"), hop(B, "b")],
        }];
        let transcript = Transcript::new(&session.id, session.source);
        let host = scripted_host(
            A,
            "a",
            true,
            vec![serde_json::to_string(&transcript).unwrap()],
        );
        assert_eq!(
            host.open(&session, Path::new("/local")).unwrap().id,
            session.id
        );
        let commands = calls(&host);
        let command = &commands[0];
        assert_eq!(&command[1..5], &["federation", "show", "--origin", B]);
        assert_eq!(&command[5..8], &["--source", "omp", "--id"]);
        assert_eq!(command[8], session.id);
        assert_eq!(
            command[10],
            serde_json::to_string(&vec![hop(B, "b")]).unwrap()
        );

        let old = scripted_host(A, "a", false, Vec::new());
        assert!(
            old.open(&session, Path::new("/local"))
                .unwrap_err()
                .to_string()
                .contains("cannot delegate")
        );
        assert!(calls(&old).is_empty());
    }

    #[test]
    fn a_changed_peer_identity_cannot_receive_a_delegated_show() {
        let mut session = row(B);
        session.routes = vec![AccessRoute {
            hops: vec![hop(A, "a"), hop(B, "b")],
        }];
        let host = scripted_host(C, "a", true, Vec::new());
        assert!(host.open(&session, Path::new("/local")).is_err());
        assert!(calls(&host).is_empty());
    }

    fn shell_words(line: &str) -> Vec<String> {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("set -- {line}; printf '%s\\0' \"$@\""))
            .output()
            .unwrap();
        assert!(output.status.success());
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|word| !word.is_empty())
            .map(|word| String::from_utf8(word.to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn multihop_resume_preserves_each_remote_shell_argument() {
        let sources = source::all(Path::new("/local"));
        let mut session = row(B);
        session.id = "id 'with spaces' $(false)".into();
        session.routes = vec![AccessRoute {
            hops: vec![hop(A, "a"), hop(B, "b")],
        }];
        session.origin_label = Some("origin display only".into());
        let command = resume_command(&session, &sources).unwrap();
        assert_eq!(&command[..4], &["ssh", "-t", "--", "user@a"]);
        let second = shell_words(&command[4]);
        assert_eq!(&second[..4], &["ssh", "-t", "--", "user@b"]);
        let actual = shell_words(&second[4]);
        assert_eq!(
            actual,
            source::find(&sources, session.source)
                .unwrap()
                .resume_command(&session.id)
                .unwrap()
        );
        session.routes.clear();
        assert!(resume_command(&session, &sources).is_none());
    }

    #[test]
    fn transcript_cache_uses_first_hop_and_origin_qualified_identity() {
        let mut session = row(B);
        session.origin_label = Some("not-configured".into());
        let first = hop(A, "a");
        session.routes = vec![AccessRoute {
            hops: vec![first.clone(), hop(B, "b")],
        }];
        let transcript = Transcript::new(&session.id, session.source);
        let host = Arc::new(scripted_host(
            A,
            "a",
            true,
            vec![serde_json::to_string(&transcript).unwrap()],
        ));
        let (mut cache, _) = Cache::empty();
        cache.openers.insert(first, Arc::clone(&host));
        cache.request_transcript(&session).unwrap();
        let pending = cache.pending.as_ref().unwrap();
        assert_eq!(pending.key, session.identity());
        assert_eq!(pending.host, "a");
        let (key, result) = loop {
            if let Some(result) = cache.poll_transcript() {
                break result;
            }
            thread::yield_now();
        };
        assert_eq!(key, session.identity());
        assert_eq!(result.unwrap().id, session.id);
        assert_eq!(
            calls(&host)[0][10],
            serde_json::to_string(&vec![hop(B, "b")]).unwrap()
        );
        let mut other = session.clone();
        other.origin_id = Some(C.into());
        assert_ne!(other.identity(), session.identity());
    }

    #[test]
    fn top_is_local_only_on_guarded_peers_and_never_ordinary_top() {
        let host = scripted_host(A, "a", true, vec!["[]".into()]);
        assert!(host.top().unwrap().is_empty());
        assert_eq!(&calls(&host)[0][1..], &["federation", "top"]);
        let old = scripted_host(A, "old", false, Vec::new());
        assert!(old.top().is_err());
        assert!(calls(&old).is_empty());
    }

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
