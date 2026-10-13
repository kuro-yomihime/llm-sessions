//! Sessions on other machines, reached over SSH or HTTP.
//!
//! A peer whose capabilities handshake names this observer's
//! [`crate::federation::PROTOCOL_VERSION`] receives guarded, origin-qualified
//! JSON requests that may carry mixed ssh/http routes. Older binaries —
//! including version 1 peers, whose validators accept ssh hops only — and
//! shell-only peers are downgraded to local-store leaves; ordinary remote
//! `list` and `show` are never compatibility fallbacks. Both transports read
//! the same provisioned origin marker, and every successful observation
//! retains an explicit access route separate from its origin display label.
//!
//! Neither transport copies a store over the wire. An SSH file listing costs
//! one `ssh` round trip per host, carrying at most `REMOTE_FILES_PER_SOURCE`
//! heads of `REMOTE_HEAD_BYTES` bytes; an HTTP peer answers the same questions
//! through its read-only federation routes, one bounded request each. HTTP is
//! fetch-only by construction: no route forwards a terminal, so HTTP rows
//! never offer resume.

mod parse;
mod transport;

#[cfg(feature = "http")]
mod http;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use clap::ValueEnum;
pub use parse::parse_shell_listing;
use serde::{Deserialize, Serialize};
use transport::{Probe, files_script, missing_sqlite3_script, open_sql_script, sql_script};

use crate::catalog::scan::Scan;
use crate::error::{Error, Result};
use crate::federation::{DiscoveryContext, FederationScan, valid_route};
use crate::model::{
    AccessRoute, Locator, RouteHop, SessionKey, SessionSummary, SourceId, Transcript,
    valid_peer_url,
};
use crate::runtime::live::Activity;
use crate::source::{self, Source};

/// Largest body the shell transport will pull over for reading. Past this a
/// remote `llm-sessions` is the right answer, not a multi-megabyte `cat`.
const REMOTE_BODY_MAX: u64 = 8 * 1024 * 1024;

/// How long TUI rows from a host stay fresh. A remote fleet does not change
/// faster than the eye, and this keeps one `ssh` per host per quarter minute.
const REFRESH: Duration = Duration::from_secs(15);

/// One configured machine. `name` labels rows; the [`Transport`] says how
/// it is reached. The token itself never enters this file, a route, or an
/// error.
#[derive(Clone, Debug)]
pub struct HostConfig {
    pub name: String,
    pub transport: Transport,
    /// Agent names ([`SourceId::name`]) to scan. Empty: all of them.
    pub sources: Vec<String>,
    pub mode: Mode,
}

impl HostConfig {
    /// One reported line explaining why this entry cannot be used, or `None`
    /// when it is well-formed. Checked at load and again in `hosts add`, so
    /// a hand-edited file and the CLI agree on what a peer looks like.
    /// Which transport an entry is — and that it states exactly one — is
    /// settled by construction ([`Transport::from_parts`]); what remains
    /// are the values an explicit transport can still carry wrongly.
    pub fn problem(&self) -> Option<String> {
        let Transport::Http { url, token_env } = &self.transport else {
            return None;
        };
        if !valid_peer_url(url) {
            return Some(format!(
                "url {url:?} must be http:// or https:// with a host"
            ));
        }
        if self.mode == Mode::Shell {
            return Some(
                "shell transport needs ssh; an HTTP peer always speaks the federation protocol"
                    .to_string(),
            );
        }
        let name = token_env.as_deref()?;
        if name.is_empty() || name.contains(['=', '\0']) || name.chars().any(char::is_whitespace) {
            return Some("token_env must name an environment variable".to_string());
        }
        None
    }
}

impl Serialize for HostConfig {
    /// The published flat hosts.json shape, unchanged: `ssh`, `url`,
    /// `token_env` and `home` sit side by side, an HTTP peer leaving `ssh`
    /// empty and an SSH peer leaving `url` unset.
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct as _;
        let mut state = serializer.serialize_struct("HostConfig", 7)?;
        state.serialize_field("name", &self.name)?;
        match &self.transport {
            Transport::Ssh { ssh, home } => {
                state.serialize_field("ssh", ssh)?;
                state.serialize_field("url", &None::<String>)?;
                state.serialize_field("token_env", &None::<String>)?;
                state.serialize_field("home", home)?;
            }
            Transport::Http { url, token_env } => {
                state.serialize_field("ssh", "")?;
                state.serialize_field("url", &Some(url.as_str()))?;
                state.serialize_field("token_env", token_env)?;
                state.serialize_field("home", &None::<PathBuf>)?;
            }
        }
        state.serialize_field("sources", &self.sources)?;
        state.serialize_field("mode", &self.mode)?;
        state.end()
    }
}

/// Exactly one way to reach a peer: an `ssh` destination, or the HTTP
/// federation protocol. `ssh` is whatever `ssh` accepts — an alias from
/// `~/.ssh/config` or `user@host`. An HTTP peer carries `url` instead
/// (never both), with an optional `token_env` naming the environment
/// variable that holds its bearer token.
#[derive(Clone, Debug)]
pub enum Transport {
    /// Reached with the `ssh` command.
    Ssh {
        /// Destination `ssh` is invoked with.
        ssh: String,
        /// Remote home. Absent: ask the host with `echo $HOME`.
        home: Option<PathBuf>,
    },
    /// Reached over HTTP, as a `llm-sessions serve` peer.
    Http {
        /// Base URL of the peer.
        url: String,
        /// Environment variable holding the peer's bearer token. A name,
        /// never a secret; resolved only on this machine, at request time.
        token_env: Option<String>,
    },
}

impl Transport {
    /// The exactly-one-of pair every construction path states: an optional
    /// `ssh` or `url`, with the per-transport extras on their own side —
    /// `home` belongs to ssh, `token_env` to http. A malformed combination
    /// is refused with the same wording [`HostConfig::problem`] reports,
    /// so the CLI and a hand-edited file fail alike.
    pub fn from_parts(
        ssh: Option<String>,
        url: Option<String>,
        home: Option<PathBuf>,
        token_env: Option<String>,
    ) -> std::result::Result<Self, String> {
        match (ssh.as_ref().is_none_or(|ssh| ssh.trim().is_empty()), url) {
            (true, None) => Err("needs one of ssh or url".to_string()),
            (false, Some(_)) => Err("has both ssh and url".to_string()),
            (false, None) => {
                if token_env.is_some() {
                    return Err("token_env is an http setting; an ssh peer needs none".to_string());
                }
                Ok(Self::Ssh {
                    ssh: ssh.expect("the match arm checked ssh non-empty"),
                    home,
                })
            }
            (true, Some(url)) => {
                if home.is_some() {
                    return Err("home is an ssh setting; an HTTP peer owns its own".to_string());
                }
                Ok(Self::Http { url, token_env })
            }
        }
    }

    /// What rows and `hosts` show for this peer: its URL, or the ssh alias.
    pub fn destination(&self) -> &str {
        match self {
            Self::Ssh { ssh, .. } => ssh,
            Self::Http { url, .. } => url,
        }
    }
}

/// Which transport to use. `Auto` probes for a remote `llm-sessions` once per process
/// and remembers the answer. One enum serves both the command line
/// (`hosts add --mode`) and hosts.json, whose kebab-case spellings
/// (`auto`, `llm-sessions`, `shell`) coincide with the CLI's.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    #[default]
    Auto,
    /// Require the remote `llm-sessions`; report a failure when it is missing.
    LlmSessions,
    /// Never use the remote `llm-sessions`, even when present.
    Shell,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Config {
    pub hosts: Vec<HostConfig>,
}

/// The wire shape of hosts.json: the historical flat fields, parsed only
/// inside [`Config::load_at`] and immediately turned into the explicit
/// [`Transport`]. The flat `ssh`/`url` pair — including the empty `ssh`
/// an HTTP peer historically wrote — never escapes this boundary.
mod wire {
    use std::path::PathBuf;

    use serde::Deserialize;

    use super::Mode;

    #[derive(Deserialize)]
    pub(super) struct Config {
        #[serde(default)]
        pub(super) hosts: Vec<HostConfig>,
    }

    #[derive(Deserialize)]
    pub(super) struct HostConfig {
        pub(super) name: String,
        #[serde(default)]
        pub(super) ssh: String,
        #[serde(default)]
        pub(super) url: Option<String>,
        #[serde(default)]
        pub(super) token_env: Option<String>,
        #[serde(default)]
        pub(super) home: Option<PathBuf>,
        #[serde(default)]
        pub(super) sources: Vec<String>,
        #[serde(default)]
        pub(super) mode: Mode,
    }
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
        let parsed: wire::Config = match serde_json::from_str(&text) {
            Ok(parsed) => parsed,
            Err(e) => return (Self::default(), Some(format!("hosts.json: {e}"))),
        };

        let mut hosts: Vec<HostConfig> = Vec::with_capacity(parsed.hosts.len());
        let mut problems: Vec<String> = Vec::new();
        for wire::HostConfig {
            name,
            ssh,
            url,
            token_env,
            home,
            sources,
            mode,
        } in parsed.hosts
        {
            if name.trim().is_empty() {
                problems.push(format!(
                    "hosts.json: host {name:?} needs a name, entry ignored"
                ));
                continue;
            }
            let entry = match Transport::from_parts(Some(ssh), url, home, token_env) {
                Ok(transport) => HostConfig {
                    name: name.clone(),
                    transport,
                    sources,
                    mode,
                },
                Err(problem) => {
                    problems.push(format!("hosts.json: host {name} {problem}, entry ignored"));
                    continue;
                }
            };
            if let Some(problem) = entry.problem() {
                problems.push(format!("hosts.json: host {name} {problem}, entry ignored"));
                continue;
            }
            if hosts.iter().any(|kept| kept.name == entry.name) {
                problems.push(format!(
                    "hosts.json: duplicate host {}, later entry ignored",
                    entry.name
                ));
                continue;
            }
            hosts.push(entry);
        }
        let failure = (!problems.is_empty()).then(|| problems.join("; "));
        (Self { hosts }, failure)
    }
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
    /// Learned once over HTTP: the peer's origin, from its capabilities.
    #[cfg(feature = "http")]
    http_origin: OnceLock<std::result::Result<String, String>>,
    #[cfg(test)]
    scripted: Option<transport::Scripted>,
}

/// Resume at the actual origin, preserving every SSH shell parsing layer.
///
/// The route is chosen independently of the read-preferred route: reads
/// may travel over HTTP, but a terminal cannot, so this picks
/// the deterministic shortest ssh-only route among the session's canonical
/// routes. An HTTP-only session yields no command at all — the HTTP
/// protocol is fetch-only, and pretending a URL could carry a terminal
/// would be a fabricated resume.
pub fn resume_command(
    session: &SessionSummary,
    sources: &[Box<dyn Source>],
) -> Option<Vec<String>> {
    let mut command = source::find(sources, session.source)?.resume_command(&session.id)?;
    if session.is_local() {
        return Some(command);
    }
    let route = resume_route(session)?;
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

/// The route a resume travels: among the session's canonical routes, the
/// [`AccessRoute::cmp_preferred`] one whose hops are all valid ssh hops.
/// Selection ignores [`SessionSummary::preferred_route`], which
/// legitimately prefers an HTTP hop for reading. An invalid or http-only
/// route is never a fallback — it is not a candidate at all.
fn resume_route(session: &SessionSummary) -> Option<&AccessRoute> {
    let origin = session.origin_id.as_deref()?;
    session
        .routes
        .iter()
        .filter(|route| {
            valid_route(&route.hops, origin) && route.hops.iter().all(|hop| hop.url.is_none())
        })
        .min_by(|a, b| a.cmp_preferred(b))
}

impl Host {
    pub fn new(config: HostConfig) -> Self {
        Self {
            config,
            probe: OnceLock::new(),
            #[cfg(feature = "http")]
            http_origin: OnceLock::new(),
            #[cfg(test)]
            scripted: None,
        }
    }

    /// Rebuild the peer an observation's first hop names, for opening that
    /// session. Credential bindings never cross this boundary: a hop's
    /// `token_env` is ignored here even when set, and an HTTP peer's token
    /// is re-resolved by matching its URL against this machine's own
    /// hosts.json. A URL this observer never configured is asked nothing
    /// this observer holds.
    pub fn from_route_hop(hop: &RouteHop, home: &Path) -> Self {
        let transport = match hop.url.as_deref() {
            Some(url) => {
                let token_env = Config::load(home)
                    .0
                    .hosts
                    .into_iter()
                    .find(|configured| {
                        matches!(&configured.transport, Transport::Http { url: bound, .. }
                            if bound.trim_end_matches('/') == url.trim_end_matches('/'))
                    })
                    .and_then(|configured| match configured.transport {
                        Transport::Http { token_env, .. } => token_env,
                        Transport::Ssh { .. } => None,
                    });
                Transport::Http {
                    url: url.to_owned(),
                    token_env,
                }
            }
            None => Transport::Ssh {
                ssh: hop.ssh.clone(),
                home: hop.home.clone(),
            },
        };
        Self::new(HostConfig {
            name: hop.name.clone(),
            transport,
            sources: Vec::new(),
            mode: Mode::Auto,
        })
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// What rows and `hosts` show for this peer: its URL, or the ssh alias.
    pub fn destination(&self) -> &str {
        self.config.transport.destination()
    }

    /// Whether this peer is reached over HTTP rather than ssh.
    fn is_http(&self) -> bool {
        matches!(self.config.transport, Transport::Http { .. })
    }

    /// The remote home the configuration pins, if it does. An HTTP peer
    /// owns its own; only the ssh transport has one to pin.
    fn configured_home(&self) -> Option<&Path> {
        match &self.config.transport {
            Transport::Ssh { home, .. } => home.as_deref(),
            Transport::Http { .. } => None,
        }
    }

    #[cfg(not(feature = "http"))]
    /// The HTTP transport is reachable only in builds with the `http`
    /// feature; without it the peer is reported, never silently skipped.
    fn http_unavailable(&self) -> Error {
        self.fail("HTTP transport not built in; rebuild with the http feature to reach url peers")
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
        if self.is_http() {
            return self.http_status();
        }
        match self.probe_once() {
            Err(message) => format!("unreachable: {message}"),
            Ok(probe) if self.use_llm_sessions(probe) => "llm-sessions".to_string(),
            Ok(_) => "shell".to_string(),
        }
    }

    /// `context.visited` already includes the calling instance. Each branch
    /// borrows that immutable value; only the receiving endpoint extends it.
    pub fn list_with(
        &self,
        sources: &[Box<dyn Source>],
        context: &DiscoveryContext,
    ) -> Result<Scan> {
        if context.remaining_hops == 0 {
            return Ok(Scan {
                sessions: Vec::new(),
                failures: vec![format!(
                    "{}: federation hop limit reached; discovery truncated",
                    self.name()
                )],
            });
        }
        if self.is_http() {
            return self.http_list_with(context);
        }
        let probe = self.probed()?;
        if context.visited.contains(&probe.origin_id) {
            return Ok(Scan::default());
        }
        if !self.use_llm_sessions(probe) {
            let mut scan = self.list_via_shell(sources, probe)?;
            // A peer whose binary speaks another federation protocol
            // version — in practice the version 1 SSH-only contract —
            // must never receive this observer's mixed routes or
            // chained discovery. It stays a shell leaf, and the
            // diagnostic names the downgrade.
            scan.failures.push(format!(
                "{}: {}; shell local-only listing (no chained discovery)",
                self.name(),
                if probe.federation == 0 {
                    "federation protocol unavailable".to_string()
                } else {
                    format!(
                        "federation protocol version {} peer accepts only ssh routes; downgraded to a shell leaf",
                        probe.federation
                    )
                }
            ));
            return Ok(scan);
        }
        self.list_via_llm_sessions(probe, context)
    }

    /// Open through the selected route, qualified by origin and source.
    ///
    /// An HTTP first hop is always the gateway model: the peer resolves the
    /// qualified identity from its own permitted catalog and follows its own
    /// routes to the origin. The rest of this route is the peer's business —
    /// HTTP never asks a server to execute a received route.
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
        if first.url.is_some() {
            return self.http_open(session, first);
        }
        let probe = self.probed()?;
        let Transport::Ssh {
            ssh,
            home: peer_home,
        } = &self.config.transport
        else {
            return Err(self.fail("access route does not match the probed peer origin"));
        };
        if first.ssh != *ssh
            || first.home.as_deref() != peer_home.as_deref()
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

    /// Remote processes, for `llm-sessions top` and liveness. The HTTP
    /// protocol exposes no live-process view, and inventing one from rows
    /// would be a fabrication, so HTTP peers refuse.
    pub fn top(&self) -> Result<Vec<RemoteProcess>> {
        if self.is_http() {
            return self.http_top();
        }
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
        if self.is_http() {
            return self.http_origin_id();
        }
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

    fn list_via_llm_sessions(&self, probe: &Probe, context: &DiscoveryContext) -> Result<Scan> {
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
        strip_received_credentials(&mut scan);
        scan.sessions.retain(|session| self.wants(session.source));
        for session in &mut scan.sessions {
            self.attach_route(session, &probe.origin_id)?;
        }
        Ok(Scan {
            sessions: scan.sessions,
            failures: scan.failures,
        })
    }

    /// Qualify one received row with this peer as its first hop. `peer` is
    /// the origin the probe or capabilities call attributed to this peer.
    fn attach_route(&self, session: &mut SessionSummary, peer: &str) -> Result<()> {
        let origin = session
            .origin_id
            .as_deref()
            .filter(|origin| crate::federation::valid_origin(origin))
            .ok_or_else(|| self.fail("received session has no valid canonical origin identity"))?;
        let hop = match &self.config.transport {
            Transport::Ssh { ssh, home } => RouteHop {
                origin_id: peer.to_owned(),
                name: self.config.name.clone(),
                ssh: ssh.clone(),
                url: None,
                token_env: None,
                home: home.clone(),
            },
            Transport::Http { url, token_env } => RouteHop {
                origin_id: peer.to_owned(),
                name: self.config.name.clone(),
                ssh: String::new(),
                url: Some(url.clone()),
                token_env: token_env.clone(),
                home: None,
            },
        };
        if session.routes.is_empty() {
            if origin != peer {
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
        if origin == peer {
            session.origin_label = Some(self.config.name.clone());
        }
        Ok(())
    }

    fn list_via_shell(&self, sources: &[Box<dyn Source>], probe: &Probe) -> Result<Scan> {
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
            return Ok(Scan {
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
            self.attach_route(session, &probe.origin_id)?;
        }
        failures.extend(unscanned(&raw, self.name()));
        Ok(Scan { sessions, failures })
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
        self.config.mode != Mode::Shell
            && probe.federation == crate::federation::PROTOCOL_VERSION
            && probe.llm_sessions.is_some()
    }

    fn fail(&self, message: impl Into<String>) -> Error {
        Error::Remote {
            host: self.config.name.clone(),
            message: message.into(),
        }
    }
}

/// The HTTP half of [`Host`], for builds without the `http` feature: every
/// method the `http` module provides, answering with the missing-transport
/// report so an HTTP peer is always reported, never silently skipped.
#[cfg(not(feature = "http"))]
impl Host {
    fn http_status(&self) -> String {
        format!("unreachable: {}", self.http_unavailable())
    }

    fn http_list_with(&self, _context: &DiscoveryContext) -> Result<Scan> {
        Err(self.http_unavailable())
    }

    fn http_open(&self, _session: &SessionSummary, _first: &RouteHop) -> Result<Transcript> {
        Err(self.http_unavailable())
    }

    fn http_top(&self) -> Result<Vec<RemoteProcess>> {
        Err(self.http_unavailable())
    }

    fn http_origin_id(&self) -> Result<&str> {
        Err(self.http_unavailable())
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

/// Discard credential references from every route a peer sent. Routes are
/// the one place a remote's JSON becomes local state; only local hosts.json
/// may say which environment variable holds a token for an endpoint, so a
/// received `token_env` is dropped before any route is stored or forwarded.
fn strip_received_credentials(scan: &mut FederationScan) {
    for session in &mut scan.sessions {
        for route in &mut session.routes {
            route.strip_credentials();
        }
    }
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
            let host = Arc::new(Host::from_route_hop(&hop, &self.home));
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
        let mut scan = Scan::default();
        for handle in handles {
            match handle.join() {
                Ok(Ok(branch)) => scan.extend(branch),
                Ok(Err(e)) => scan.failures.push(e.to_string()),
                Err(_) => scan.failures.push("a host listing panicked".to_string()),
            }
        }
        scan.assemble();
        (scan.sessions, scan.failures)
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
            url: None,
            token_env: None,
            home: None,
        }
    }

    fn http_hop(origin: &str, name: &str, url: &str) -> RouteHop {
        RouteHop {
            origin_id: origin.into(),
            name: name.into(),
            ssh: String::new(),
            url: Some(url.into()),
            token_env: None,
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
        let mut host = Host::from_route_hop(&hop(origin, name), Path::new("/local"));
        host.probe
            .set(Ok(Probe {
                home: PathBuf::from("/remote"),
                llm_sessions: Some("/remote/bin/llm-sessions".into()),
                sqlite3: true,
                origin_id: origin.into(),
                federation: if guarded {
                    crate::federation::PROTOCOL_VERSION
                } else {
                    0
                },
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
        let mut host = Host::from_route_hop(&route_hop, Path::new("/local"));
        let transcript = Transcript::new("same-id", SourceId::Omp);
        host.scripted = Some(transport::Scripted {
            replies: std::sync::Mutex::new(
                [
                    Ok(format!(
                        "home:{}\norigin:{A}\nllm-sessions:/bin/peer\n",
                        home.display()
                    )),
                    Ok(format!(
                        r#"{{"version":{},"origin_id":"{A}"}}"#,
                        crate::federation::PROTOCOL_VERSION
                    )),
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
        let reopened =
            Host::from_route_hop(&scan.sessions[0].routes[0].hops[0], Path::new("/local"));
        assert!(matches!(
            &reopened.config.transport,
            Transport::Ssh {
                home: configured,
                ..
            } if configured.as_deref() == Some(home.as_path())
        ));
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
            transport: Transport::Ssh {
                ssh: "must-not-contact".into(),
                home: None,
            },
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
        let mut host = Host::from_route_hop(&hop(A, "old"), Path::new("/local"));
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

        let mut missing = Host::from_route_hop(&hop(B, "missing"), Path::new("/local"));
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
    fn a_version1_peer_is_a_downgraded_shell_leaf_and_never_receives_federation_requests() {
        // The previous protocol: a real capabilities handshake that claims
        // version 1, whose validators accept only ssh hops. It must be
        // listed as a shell leaf with a diagnostic, and never be sent a
        // guarded `federation` request that could carry mixed routes.
        let mut host = Host::from_route_hop(&hop(A, "legacy"), Path::new("/local"));
        host.scripted = Some(transport::Scripted {
            replies: std::sync::Mutex::new(
                [
                    Ok(format!("home:/remote\norigin:{A}\nllm-sessions:/bin/old\n")),
                    Ok(format!(r#"{{"version":1,"origin_id":"{A}"}}"#)),
                    Ok("F\tomp\t/remote/s.jsonl\t1700000000\t30\nH\t/remote/s.jsonl\teyJ0eXBlIjoic2Vzc2lvbiIsImlkIjoicyJ9\n".into()),
                ]
                .into_iter()
                .collect(),
            ),
            ..transport::Scripted::default()
        });
        let sources = source::all(Path::new("/local"));
        let scan = host
            .list_with(&sources, &DiscoveryContext::default())
            .unwrap();
        assert!(
            scan.failures.iter().any(|failure| {
                failure.contains("version 1") && failure.contains("downgraded to a shell leaf")
            }),
            "{:?}",
            scan.failures
        );
        assert_eq!(scan.sessions.len(), 1);
        assert_eq!(scan.sessions[0].origin_id.as_deref(), Some(A));
        assert_eq!(scan.sessions[0].routes[0].hops, vec![hop(A, "legacy")]);
        let commands = calls(&host);
        assert_eq!(
            commands[1],
            vec![
                String::from("/bin/old"),
                String::from("federation"),
                String::from("capabilities")
            ]
        );
        assert!(
            !commands
                .iter()
                .any(|command| command.contains(&"list".into())),
            "{commands:?}: a legacy peer receives no guarded discovery"
        );
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
        host.attach_route(&mut session, host.probed().unwrap().origin_id.as_str())
            .unwrap();
        assert_eq!(session.origin_id.as_deref(), Some(A));
        assert_eq!(session.origin_label.as_deref(), Some("a"));
        assert_eq!(session.preferred_route().unwrap().hops, vec![hop(A, "a")]);
        assert!(!session.is_local());
    }

    #[test]
    fn received_routes_reject_cycles_invalid_hops_and_incorrect_targets() {
        let host = scripted_host(A, "a", true, Vec::new());
        let probe = host.probed().unwrap().origin_id.clone();
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
            assert!(host.attach_route(&mut session, &probe).is_err());
        }
        assert!(calls(&host).is_empty());
    }

    #[test]
    fn capability_identity_mismatch_stops_before_listing() {
        let mut host = Host::from_route_hop(&hop(A, "a"), Path::new("/local"));
        host.scripted = Some(transport::Scripted {
            replies: std::sync::Mutex::new(
                [
                    Ok(format!(
                        "home:/remote\norigin:{A}\nllm-sessions:/bin/peer\n"
                    )),
                    Ok(format!(
                        r#"{{"version":{},"origin_id":"{B}"}}"#,
                        crate::federation::PROTOCOL_VERSION
                    )),
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
        assert!(
            matches!(
                &config.hosts[0].transport,
                Transport::Ssh { ssh, .. } if ssh == "a"
            ),
            "the first entry wins"
        );
        let failure = failure.expect("a duplicate must be reported");
        assert!(failure.contains("duplicate host a"), "{failure}");
    }

    #[test]
    fn http_peers_load_and_conflicting_entries_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.json");
        std::fs::write(
            &path,
            r#"{"hosts":[
                {"name":"leaf","url":"http://127.0.0.1:8787/","token_env":"LLMS_PEER_TOKEN","sources":["omp"]},
                {"name":"both","ssh":"both","url":"https://example.net"},
                {"name":"no-transport"},
                {"name":"scheme","url":"ftp://example.net"},
                {"name":"ssh-token","ssh":"ssh-only","token_env":"LLMS_PEER_TOKEN"},
                {"name":"home-url","url":"https://example.net","home":"/remote"},
                {"name":"shell-url","url":"https://example.net","mode":"shell"}
            ]}"#,
        )
        .unwrap();

        let (config, failure) = Config::load_at(&path);
        assert_eq!(config.hosts.len(), 1, "{failure:?}");
        let leaf = &config.hosts[0];
        assert!(matches!(
            &leaf.transport,
            Transport::Http { url, token_env }
                if url == "http://127.0.0.1:8787/"
                    && token_env.as_deref() == Some("LLMS_PEER_TOKEN")
        ));

        let failure = failure.expect("every malformed entry is reported");
        for (name, why) in [
            ("both", "both ssh and url"),
            ("no-transport", "one of ssh or url"),
            ("scheme", "http:// or https://"),
            ("ssh-token", "ssh peer needs none"),
            ("home-url", "owns its own"),
            ("shell-url", "federation protocol"),
        ] {
            assert!(failure.contains(name), "{name} missing: {failure}");
            assert!(failure.contains(why), "{why} missing: {failure}");
        }
    }

    #[test]
    fn mixed_transport_routes_validate_and_reject_origin_cycles() {
        let mixed = vec![
            http_hop(A, "gateway", "http://127.0.0.1:8001"),
            hop(B, "worker"),
        ];
        assert!(valid_route(&mixed, B));
        // The same origin twice — even through two transports or under two
        // display names — is a cycle, whatever the last hop says.
        let cycle = vec![
            http_hop(A, "gateway", "http://127.0.0.1:8001"),
            hop(A, "gateway-ssh-alias"),
            hop(B, "worker"),
        ];
        assert!(!valid_route(&cycle, B));
        // A hop carrying both transports is refused, as is an unreachable one.
        let mut both = http_hop(A, "gateway", "http://127.0.0.1:8001");
        both.ssh = "user@gateway".into();
        assert!(!valid_route(&[both, hop(B, "worker")], B));
        let mut offline = http_hop(A, "gateway", "http://127.0.0.1:8001");
        offline.url = None;
        assert!(!valid_route(&[offline, hop(B, "worker")], B));
    }

    /// Credential rebinding for delegated HTTP hops (a wire `token_env` cannot
    /// select a local secret; only an exact URL match against this machine's
    /// hosts.json binds one) is exercised by the subprocess federation harness:
    /// it owns real `XDG_CONFIG_HOME` sandboxes, which a unit test cannot steer
    /// without touching the user's namespace or mutating the environment.

    #[test]
    fn http_rows_never_offer_a_resume_command() {
        let home = tempfile::tempdir().unwrap();
        let sources = source::all(home.path());
        let mut session = row(B);
        session.routes = vec![AccessRoute {
            hops: vec![
                hop(A, "gateway"),
                http_hop(B, "origin", "http://127.0.0.1:8001"),
            ],
        }];
        assert!(
            resume_command(&session, &sources).is_none(),
            "a fetch-only transport cannot carry a terminal"
        );
        // SSH-only routes keep their terminal path.
        session.routes = vec![AccessRoute {
            hops: vec![hop(B, "worker")],
        }];
        let argv = resume_command(&session, &sources).expect("ssh route resumes");
        assert_eq!(&argv[..4], &["ssh", "-t", "--", "user@worker"]);
    }

    #[test]
    fn resume_picks_the_ssh_alternate_when_http_is_the_preferred_read_route() {
        let home = tempfile::tempdir().unwrap();
        let sources = source::all(home.path());
        let mut session = row(B);
        session.id = "id 'with spaces' $(false)".into();
        // The same origin through both transports: the http hop is the
        // read-preferred route (one hop, lexically first), and merge keeps
        // the ssh alternative. A terminal can only travel the ssh one.
        session.routes = vec![
            AccessRoute {
                hops: vec![http_hop(B, "api", "http://127.0.0.1:8001")],
            },
            AccessRoute {
                hops: vec![hop(B, "worker")],
            },
        ];
        assert!(
            session
                .preferred_route()
                .unwrap()
                .hops
                .iter()
                .any(|hop| hop.url.is_some()),
            "the fixture must prefer the http route for reads"
        );
        let command = resume_command(&session, &sources).expect("ssh alternative resumes");
        assert_eq!(&command[..4], &["ssh", "-t", "--", "user@worker"]);
        let inner = shell_words(&command[4]);
        assert_eq!(
            inner,
            source::find(&sources, session.source)
                .unwrap()
                .resume_command(&session.id)
                .unwrap(),
            "quoting survives the ssh layer"
        );

        // A longer ssh route loses to the shorter one, an invalid route is
        // never selected, and a genuinely http-only session has no resume.
        session.routes = vec![
            AccessRoute {
                hops: vec![hop(A, "gateway"), hop(B, "worker")],
            },
            AccessRoute {
                hops: vec![hop(B, "direct")],
            },
        ];
        assert_eq!(
            &resume_command(&session, &sources).unwrap()[..4],
            &["ssh", "-t", "--", "user@direct"]
        );
        session.routes = vec![
            AccessRoute {
                hops: vec![http_hop(B, "api", "http://127.0.0.1:8001")],
            },
            AccessRoute {
                hops: vec![hop(C, "wrong-target")],
            },
        ];
        assert!(
            resume_command(&session, &sources).is_none(),
            "an invalid route is not a silent fallback"
        );
    }

    #[test]
    fn attached_http_routes_carry_the_local_binding_and_forwarded_routes_do_not() {
        #[cfg(feature = "http")]
        {
            let host = Host::new(HostConfig {
                name: "gateway".into(),
                transport: Transport::Http {
                    url: "http://127.0.0.1:8001".into(),
                    token_env: Some("LLMS_LOCAL_PEER_TOKEN".into()),
                },
                sources: Vec::new(),
                mode: Mode::Auto,
            });
            let origin = "00000000-0000-4000-8000-0000000000aa";
            let mut direct = row(origin);
            direct.origin_id = Some(origin.into());
            host.attach_route(&mut direct, origin).unwrap();
            let direct = &direct.routes[0].hops[0];
            assert_eq!(direct.ssh, "");
            assert_eq!(direct.url.as_deref(), Some("http://127.0.0.1:8001"));
            assert_eq!(direct.token_env.as_deref(), Some("LLMS_LOCAL_PEER_TOKEN"));
            assert_eq!(direct.home, None, "an HTTP peer owns its own home");
        }

        // Routes received from any peer are stripped before they are stored.
        let mut scan = FederationScan {
            sessions: vec![row(A)],
            failures: Vec::new(),
        };
        scan.sessions[0].routes = vec![AccessRoute {
            hops: vec![RouteHop {
                origin_id: A.into(),
                name: "forged".into(),
                ssh: String::new(),
                url: Some("http://127.0.0.1:8001".into()),
                token_env: Some("HOME".into()),
                home: None,
            }],
        }];
        strip_received_credentials(&mut scan);
        assert_eq!(scan.sessions[0].routes[0].hops[0].token_env, None);
    }

    #[test]
    fn a_saved_config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.json");
        let config = Config {
            hosts: vec![HostConfig {
                name: "nx-1".into(),
                transport: Transport::Ssh {
                    ssh: "p1g3-nx-1".into(),
                    home: Some(PathBuf::from("/var/lib/hermes")),
                },
                sources: vec!["hermes".into()],
                mode: Mode::Shell,
            }],
        };
        Config::save_at(&config, &path).unwrap();
        let (loaded, failure) = Config::load_at(&path);
        assert_eq!(failure, None);
        assert_eq!(loaded.hosts.len(), 1);
        assert!(matches!(
            &loaded.hosts[0].transport,
            Transport::Ssh { ssh, home }
                if ssh == "p1g3-nx-1" && home.as_deref() == Some(Path::new("/var/lib/hermes"))
        ));
        assert_eq!(loaded.hosts[0].mode, Mode::Shell);
        assert_eq!(loaded.hosts[0].sources, vec!["hermes".to_string()]);
    }
}
