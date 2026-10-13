//! Which sessions are alive, where they are running, and whether they are
//! working or waiting on you.
//!
//! Three signals, all measured on Linux before being relied on:
//!
//! 1. **Open descriptors.** OMP keeps an fd on the exact session JSONL it is
//!    writing, so `/proc/<pid>/fd` gives an *exact* process↔session join.
//!    `OpenCode` holds only the shared `opencode.db`, so database-backed agents
//!    can never be joined this way.
//! 2. **Process ancestry.** A tmux pane's pid is an ancestor of the agent it
//!    launched, so walking `ppid` up to a known pane pid yields the pane
//!    address.
//! 3. **CPU time.** `utime + stime` advancing means the model is generating or
//!    a tool is running; flat plus state `S` means it is blocked on input.
//!
//! `/proc` exposes only the user's own processes. An unreadable entry means
//! "cannot tell", never "dead", so every check degrades to a weaker tier
//! rather than to a false negative.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::model::{Liveness, SessionSummary, SourceId};
use crate::runtime::procfs::{Proc, TcpSocket, socket_inode};
use crate::runtime::tmux::{self, Pane};

/// A tier-2 claim is only credible against a session the agent could plausibly
/// still be in. Without this, launching `opencode` in an old worktree would
/// light up a session from last spring.
const TIER2_MAX_AGE: Duration = Duration::from_hours(24);

/// How recently a session must have been written to count as [`Liveness::Recent`].
const RECENT_WINDOW: Duration = Duration::from_mins(5);

/// Gap between the two samples [`Fleet::probe`] takes so a first frame can
/// already say working or waiting.
const COLD_SAMPLE: Duration = Duration::from_millis(250);

/// An agent process is "working" above this many CPU ticks per second.
///
/// Idle agents still wake for timers; a hard zero threshold makes the badge
/// flicker. One tick per second is comfortably below the 5 ticks/s measured
/// for a generating agent.
const WORKING_TICKS_PER_SEC: f64 = 1.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Activity {
    /// CPU time is advancing: generating, or running a tool.
    Working,
    /// Blocked. In practice, waiting for you.
    Waiting,
    /// Not sampled twice yet, or `/proc` would not say. Also the default for
    /// a remote host that did not send the field.
    #[default]
    Unknown,
}

/// One running agent process.
#[derive(Clone, Debug)]
pub struct Process {
    pub pid: u32,
    pub agent: SourceId,
    pub cwd: Option<PathBuf>,
    /// Loopback ports of listening servers this process holds a connection
    /// to — how insert mode finds the server an `OpenCode` client talks to,
    /// from the kernel rather than from flags whose meaning drifts.
    pub servers: Vec<u16>,
    /// Session transcripts this process currently holds open.
    pub open_sessions: Vec<PathBuf>,
    pub pane: Option<Pane>,
    pub activity: Activity,
    /// CPU clock ticks consumed so far, for the next delta.
    ticks: u64,
}

/// Every agent process visible to this user, with its tmux placement.
#[derive(Clone, Debug, Default)]
pub struct Fleet {
    processes: Vec<Process>,
    /// `pid -> ticks` from the previous probe, so activity is a delta over the
    /// real interval rather than a blocking double-read every refresh.
    previous: HashMap<u32, u64>,
    sampled_at: Option<Instant>,
}

impl Fleet {
    /// Probe once, blocking a short cold sample so the first result already
    /// distinguishes working from waiting.
    pub fn probe() -> Self {
        let mut fleet = Self::default();
        fleet.refresh();
        std::thread::sleep(COLD_SAMPLE);
        fleet.refresh();
        fleet
    }

    /// Re-read the live kernel and tmux.
    pub fn refresh(&mut self) {
        self.refresh_from(&Proc::system());
    }

    /// Re-read process facts and tmux. Activity is the CPU delta since the
    /// previous call, so repeated refreshes never block.
    ///
    /// The proc tree is a parameter, so a fixture can drive fleet
    /// attribution end to end in tests.
    pub fn refresh_from(&mut self, proc: &Proc) {
        let panes = tmux::panes();
        let pane_by_pid: HashMap<u32, &Pane> = panes.iter().map(|p| (p.pid, p)).collect();

        let elapsed = self.sampled_at.map(|at| at.elapsed());
        let mut processes = Vec::new();
        // One namespace-wide read joins every process's sockets. Unreadable
        // tables mean no server is known, which insert mode refuses on.
        let tcp = proc.tcp().unwrap_or_default();
        let listening: HashSet<u16> = tcp
            .iter()
            .filter(|socket| socket.state == TcpSocket::LISTEN)
            .map(|socket| socket.local_port)
            .collect();
        // Unknown process inventory cannot support a live claim; fall back
        // to recency rather than treating an unreadable tree as process facts.
        for pid in proc.pids().unwrap_or_default() {
            let Some(agent) = agent_of(proc, pid) else {
                continue;
            };
            let descriptors: Vec<PathBuf> = proc
                .descriptors(pid)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(_, target)| target)
                .collect();
            let ticks = proc.stat(pid).map_or(0, |stat| stat.ticks);
            let activity = match (elapsed, self.previous.get(&pid)) {
                (Some(elapsed), Some(&before)) if elapsed.as_secs_f64() > 0.0 => {
                    let rate = ticks.saturating_sub(before) as f64 / elapsed.as_secs_f64();
                    if rate >= WORKING_TICKS_PER_SEC {
                        Activity::Working
                    } else {
                        Activity::Waiting
                    }
                }
                _ => Activity::Unknown,
            };
            processes.push(Process {
                pid,
                agent,
                cwd: proc.cwd(pid),
                servers: servers_of(&descriptors, &tcp, &listening),
                open_sessions: open_files(descriptors),
                pane: pane_for(proc, pid, &pane_by_pid).cloned(),
                activity,
                ticks,
            });
        }

        self.previous = processes.iter().map(|p| (p.pid, p.ticks)).collect();
        self.sampled_at = Some(Instant::now());
        self.processes = processes;
    }

    pub fn processes(&self) -> &[Process] {
        &self.processes
    }

    pub fn get(&self, pid: u32) -> Option<&Process> {
        self.processes.iter().find(|p| p.pid == pid)
    }

    pub fn is_empty(&self) -> bool {
        self.processes.is_empty()
    }

    /// Assign a [`Liveness`] to every session.
    ///
    /// Tier 1 is an exact fd match. Tier 2 is an inference from agent + cwd +
    /// recency, and is deliberately a different variant: a confident lie is
    /// worse than an admitted guess. Each process claims at most one session
    /// and each session at most one process, so two agents in one worktree
    /// cannot both point at the same row.
    pub fn attribute(&self, sessions: &mut [SessionSummary], now: time::OffsetDateTime) {
        let mut by_path: HashMap<PathBuf, usize> = HashMap::new();
        for (index, session) in sessions.iter().enumerate() {
            if let crate::model::Locator::File { path } = &session.locator {
                by_path.insert(path.clone(), index);
            }
        }

        let mut claimed_session: HashSet<usize> = HashSet::new();
        let mut claimed_pid: HashSet<u32> = HashSet::new();

        // Tier 1: exact, so it runs first and wins every contest.
        for process in &self.processes {
            for open in &process.open_sessions {
                if let Some(&index) = by_path.get(open)
                    && claimed_session.insert(index)
                {
                    sessions[index].liveness = Liveness::Live { pid: process.pid };
                    claimed_pid.insert(process.pid);
                    break;
                }
            }
        }

        // A process writing a subagent transcript is running its parent
        // conversation too — that is what spawned the child. Without this the
        // only attached row would be a nested one, so `--live` and the fleet
        // view would show nothing for an instance that is plainly working.
        let by_id: HashMap<(SourceId, String), usize> = sessions
            .iter()
            .enumerate()
            .map(|(index, session)| ((session.source, session.id.clone()), index))
            .collect();
        let inherited: Vec<(usize, u32)> = sessions
            .iter()
            .filter(|session| session.subagent)
            .filter_map(|child| {
                let pid = child.liveness.pid()?;
                let parent = child.parent.as_ref()?;
                let &index = by_id.get(&(child.source, parent.clone()))?;
                (!claimed_session.contains(&index)).then_some((index, pid))
            })
            .collect();
        for (index, pid) in inherited {
            if claimed_session.insert(index) {
                sessions[index].liveness = Liveness::Live { pid };
            }
        }

        // Tier 2: newest recent session in the process's workspace, same
        // agent. The cwd is rolled up exactly as a recorded cwd is, because an
        // agent launched in `repo/nixos` or in a linked worktree would
        // otherwise never match the repository its sessions were filed under.
        let mut resolver = crate::catalog::workspace::Resolver::new();
        for process in &self.processes {
            if claimed_pid.contains(&process.pid) {
                continue;
            }
            let Some(cwd) = process.cwd.as_deref() else {
                continue;
            };
            let exact = fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
            let rolled = resolver.resolve(cwd);
            let best = sessions
                .iter()
                .enumerate()
                .filter(|(index, session)| {
                    !claimed_session.contains(index)
                        && session.source == process.agent
                        && age(session.updated, now) <= TIER2_MAX_AGE
                        && (session.workspace.as_deref() == Some(rolled.as_path())
                            || session.recorded_cwd.as_deref() == Some(exact.as_path()))
                })
                .max_by_key(|(_, session)| session.updated)
                .map(|(index, _)| index);
            if let Some(index) = best {
                claimed_session.insert(index);
                claimed_pid.insert(process.pid);
                sessions[index].liveness = Liveness::Probably { pid: process.pid };
            }
        }

        for (index, session) in sessions.iter_mut().enumerate() {
            if claimed_session.contains(&index) {
                continue;
            }
            session.liveness = if age(session.updated, now) <= RECENT_WINDOW {
                Liveness::Recent
            } else {
                Liveness::Stale
            };
        }
    }
}

fn age(updated: time::OffsetDateTime, now: time::OffsetDateTime) -> Duration {
    (now - updated).try_into().unwrap_or(Duration::ZERO)
}

/// Which agent a pid is, from its argv ([`Proc::argv`] knows why `comm`
/// doesn't do). Worker and broker helpers share `argv[0]` with the real CLI
/// and are excluded by their marker argument — they own no session and would
/// otherwise claim one by cwd. `OpenCode` v2's shared background server
/// (`opencode serve --service`, running from `$HOME`) is one of them.
fn agent_of(proc: &Proc, pid: u32) -> Option<SourceId> {
    let argv = proc.argv(pid)?;
    if argv
        .iter()
        .any(|arg| arg.starts_with("__omp_worker") || arg == "daemon")
    {
        return None;
    }
    let file_name = |arg: &String| Path::new(arg).file_name()?.to_str().map(str::to_owned);
    // A script's process is its interpreter: Hermes' launcher execs
    // `venv/bin/python …/hermes-agent/hermes chat`, so the agent is the
    // script, not `python`.
    let argv = match argv.first().and_then(file_name) {
        Some(interpreter) if interpreter.starts_with("python") => &argv[1..],
        _ => &argv[..],
    };
    let name = file_name(argv.first()?)?;
    let name = name.as_str();
    match name {
        "omp" => Some(SourceId::Omp),
        "claude" => Some(SourceId::Claude),
        "codex" => Some(SourceId::Codex),
        "opencode" if argv.get(1).is_some_and(|arg| arg == "serve") => None,
        "opencode" => Some(SourceId::OpenCode),
        "copilot" => Some(SourceId::Copilot),
        "kilo" => Some(SourceId::Kilo),
        "goose" => Some(SourceId::Goose),
        "cursor-agent" => Some(SourceId::Cursor),
        "gemini" => Some(SourceId::Gemini),
        // Hermes is detected through its Python launcher (see above); no
        // `openclaw` binary exists on this machine, so its liveness is untested.
        "hermes" => Some(SourceId::Hermes),
        "openclaw" => Some(SourceId::OpenClaw),
        _ => None,
    }
}

/// Regular files the process holds open. Only transcripts matter, and the
/// caller matches them against known session paths, so anything else is noise
/// worth dropping here.
fn open_files(descriptors: Vec<PathBuf>) -> Vec<PathBuf> {
    descriptors
        .into_iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "jsonl" || ext == "json")
        })
        .collect()
}

/// Listening loopback ports the process's own sockets are connected to.
fn servers_of(descriptors: &[PathBuf], tcp: &[TcpSocket], listening: &HashSet<u16>) -> Vec<u16> {
    let inodes: HashSet<u64> = descriptors.iter().filter_map(|t| socket_inode(t)).collect();
    let mut ports: Vec<u16> = tcp
        .iter()
        .filter(|socket| {
            socket.state == TcpSocket::ESTABLISHED
                && socket.remote_loopback
                && inodes.contains(&socket.inode)
                && listening.contains(&socket.remote_port)
        })
        .map(|socket| socket.remote_port)
        .collect();
    ports.sort_unstable();
    ports.dedup();
    ports
}

/// Walk the ancestry until it reaches a pane's shell.
///
/// Bounded because a corrupted or racing `/proc` read could otherwise loop:
/// pids are reused, and a parent can be reaped mid-walk.
fn pane_for<'a>(proc: &Proc, pid: u32, panes: &HashMap<u32, &'a Pane>) -> Option<&'a Pane> {
    let mut current = pid;
    for _ in 0..32 {
        if let Some(pane) = panes.get(&current) {
            return Some(pane);
        }
        let parent = proc.stat(current)?.parent;
        if parent <= 1 || parent == current {
            return None;
        }
        current = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;
    use crate::model::{Locator, PathTrust};

    fn session(
        source: SourceId,
        id: &str,
        workspace: &str,
        path: &str,
        secs: i64,
    ) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            source,
            workspace: (!workspace.is_empty()).then(|| PathBuf::from(workspace)),
            recorded_cwd: None,
            trust: PathTrust::Recorded,
            title: None,
            created: None,
            updated: OffsetDateTime::from_unix_timestamp(secs).unwrap(),
            model: None,
            parent: None,
            archived: false,
            usage: None,
            size_bytes: None,
            locator: Locator::File {
                path: PathBuf::from(path),
            },
            branch: None,
            subagent: false,
            change: None,
            liveness: Liveness::Stale,
            origin_label: None,
            origin_id: None,
            routes: Vec::new(),
        }
    }

    fn fleet_of(processes: Vec<Process>) -> Fleet {
        Fleet {
            processes,
            ..Fleet::default()
        }
    }

    fn process(pid: u32, agent: SourceId, cwd: &str, open: &[&str]) -> Process {
        Process {
            pid,
            agent,
            cwd: Some(PathBuf::from(cwd)),
            servers: Vec::new(),
            open_sessions: open.iter().map(PathBuf::from).collect(),
            pane: None,
            activity: Activity::Unknown,
            ticks: 0,
        }
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_000_000).unwrap()
    }

    #[test]
    fn an_open_descriptor_wins_over_a_cwd_guess() {
        let mut sessions = vec![
            session(SourceId::Omp, "old", "/w", "/s/old.jsonl", 999_000),
            session(SourceId::Omp, "held", "/w", "/s/held.jsonl", 100),
        ];
        // The held session is far older, so a cwd-and-recency guess would
        // have picked the other one.
        let fleet = fleet_of(vec![process(7, SourceId::Omp, "/w", &["/s/held.jsonl"])]);
        fleet.attribute(&mut sessions, now());
        assert_eq!(sessions[1].liveness, Liveness::Live { pid: 7 });
        assert_ne!(sessions[0].liveness, Liveness::Live { pid: 7 });
    }

    #[test]
    fn a_database_agent_is_only_ever_a_guess() {
        let mut sessions = vec![session(
            SourceId::OpenCode,
            "s",
            "/w",
            "/s/s.jsonl",
            999_900,
        )];
        let fleet = fleet_of(vec![process(9, SourceId::OpenCode, "/w", &[])]);
        fleet.attribute(&mut sessions, now());
        assert_eq!(sessions[0].liveness, Liveness::Probably { pid: 9 });
    }

    /// Measured: `omp` held only `…/CstmBranchScout.jsonl` open. The attached
    /// row was therefore a nested child, and `--live` and the fleet view
    /// showed nothing for an instance that was plainly working.
    #[test]
    fn a_live_subagent_makes_its_parent_live_too() {
        let mut parent = session(SourceId::Omp, "parent", "/w", "/s/parent.jsonl", 999_900);
        parent.updated = OffsetDateTime::from_unix_timestamp(1).unwrap();
        let mut child = session(SourceId::Omp, "kid", "/w", "/s/parent/Scout.jsonl", 999_900);
        child.subagent = true;
        child.parent = Some("parent".into());
        let mut sessions = vec![parent, child];

        let fleet = fleet_of(vec![process(
            5,
            SourceId::Omp,
            "/w",
            &["/s/parent/Scout.jsonl"],
        )]);
        fleet.attribute(&mut sessions, now());
        assert_eq!(sessions[1].liveness, Liveness::Live { pid: 5 });
        assert_eq!(
            sessions[0].liveness,
            Liveness::Live { pid: 5 },
            "the parent is what that process is running"
        );
    }

    /// Measured: `opencode` running in `infra/nixos` was reported as attached
    /// to nothing, because sessions are filed under the repository root the
    /// rollup produced and the raw cwd never equals it.
    #[test]
    fn a_process_in_a_subdirectory_still_finds_its_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let repo = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir(repo.join(".git")).unwrap();
        let inside = repo.join("nixos");
        fs::create_dir(&inside).unwrap();

        let mut sessions = vec![session(
            SourceId::OpenCode,
            "s",
            repo.to_str().unwrap(),
            "/s/s.jsonl",
            999_900,
        )];
        let fleet = fleet_of(vec![process(
            11,
            SourceId::OpenCode,
            inside.to_str().unwrap(),
            &[],
        )]);
        fleet.attribute(&mut sessions, now());
        assert_eq!(sessions[0].liveness, Liveness::Probably { pid: 11 });
    }

    #[test]
    fn two_processes_in_one_worktree_claim_different_sessions() {
        let mut sessions = vec![
            session(SourceId::OpenCode, "a", "/w", "/s/a.jsonl", 999_900),
            session(SourceId::OpenCode, "b", "/w", "/s/b.jsonl", 999_800),
        ];
        let fleet = fleet_of(vec![
            process(1, SourceId::OpenCode, "/w", &[]),
            process(2, SourceId::OpenCode, "/w", &[]),
        ]);
        fleet.attribute(&mut sessions, now());
        assert_eq!(sessions[0].liveness, Liveness::Probably { pid: 1 });
        assert_eq!(sessions[1].liveness, Liveness::Probably { pid: 2 });
    }

    #[test]
    fn a_stale_session_is_not_claimed_by_a_new_process_in_the_same_directory() {
        let mut sessions = vec![session(SourceId::Omp, "ancient", "/w", "/s/a.jsonl", 1)];
        let fleet = fleet_of(vec![process(3, SourceId::Omp, "/w", &[])]);
        fleet.attribute(&mut sessions, now());
        assert_eq!(sessions[0].liveness, Liveness::Stale);
    }

    #[test]
    fn recency_falls_back_to_mtime_when_nothing_is_running() {
        let mut sessions = vec![
            session(SourceId::Omp, "warm", "/w", "/s/w.jsonl", 999_900),
            session(SourceId::Omp, "cold", "/w", "/s/c.jsonl", 1),
        ];
        fleet_of(Vec::new()).attribute(&mut sessions, now());
        assert_eq!(sessions[0].liveness, Liveness::Recent);
        assert_eq!(sessions[1].liveness, Liveness::Stale);
    }

    #[test]
    fn this_process_reads_its_own_cpu_time_and_parent() {
        let proc = Proc::system();
        let pid = std::process::id();
        let stat = proc.stat(pid);
        assert!(stat.is_some());
        assert_eq!(stat.map(|stat| stat.parent), Some(parent_from_status()));
    }

    /// A fixture `/proc` tree drives the whole attribution: one omp process
    /// holding the exact session file open claims it exactly.
    #[test]
    fn fleet_attributes_from_a_fixture_proc_tree() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let session_path = repo.join("2026-01-01T00-00-00-000Z_s1.jsonl");
        std::fs::write(&session_path, "{}\n").unwrap();

        let proc_dir = dir.path().join("proc");
        let pid_dir = proc_dir.join("42");
        std::fs::create_dir_all(pid_dir.join("fd")).unwrap();
        std::fs::write(pid_dir.join("cmdline"), b"omp\0").unwrap();
        std::fs::write(pid_dir.join("stat"), "(omp) S 41 0 0 0 0 0 0 0 7 5 0 0\n").unwrap();
        std::os::unix::fs::symlink(&session_path, pid_dir.join("fd").join("3")).unwrap();
        std::os::unix::fs::symlink(&repo, pid_dir.join("cwd")).unwrap();

        let mut fleet = Fleet::default();
        fleet.refresh_from(&Proc::at(&proc_dir));
        assert_eq!(fleet.processes().len(), 1);
        assert_eq!(fleet.processes()[0].agent, SourceId::Omp);
        assert_eq!(
            fleet.processes()[0].open_sessions,
            vec![session_path.clone()]
        );

        let mut sessions = vec![SessionSummary::new(
            "s1".into(),
            SourceId::Omp,
            time::OffsetDateTime::now_utc(),
            crate::model::Locator::File { path: session_path },
        )];
        fleet.attribute(&mut sessions, now());
        assert_eq!(sessions[0].liveness, Liveness::Live { pid: 42 });
    }

    /// `OpenCode` v2 keeps a shared `opencode serve --service` running from
    /// `$HOME`. Counted as an agent it would mark whatever recent session ran
    /// in `$HOME` as probably live; only the interactive client counts.
    #[test]
    fn opencode_v2_background_server_is_not_an_agent() {
        let dir = tempfile::tempdir().unwrap();
        let proc_dir = dir.path().join("proc");
        for (pid, argv) in [
            ("50", b"/usr/bin/opencode\0serve\0--service\0".as_slice()),
            ("51", b"/usr/bin/opencode\0".as_slice()),
        ] {
            let pid_dir = proc_dir.join(pid);
            std::fs::create_dir_all(pid_dir.join("fd")).unwrap();
            std::fs::write(pid_dir.join("cmdline"), argv).unwrap();
            std::fs::write(
                pid_dir.join("stat"),
                "(opencode) S 1 0 0 0 0 0 0 0 7 5 0 0\n",
            )
            .unwrap();
            std::os::unix::fs::symlink(dir.path(), pid_dir.join("cwd")).unwrap();
        }

        let mut fleet = Fleet::default();
        fleet.refresh_from(&Proc::at(&proc_dir));
        let pids: Vec<u32> = fleet.processes().iter().map(|p| p.pid).collect();
        assert_eq!(pids, [51]);
    }

    /// Hermes runs as `python …/hermes chat`. Keyed on `argv[0]` alone, a
    /// running Hermes looked stale — and insert mode resumed behind it.
    #[test]
    fn an_agent_run_by_its_interpreter_is_still_that_agent() {
        let dir = tempfile::tempdir().unwrap();
        let pid_dir = dir.path().join("60");
        std::fs::create_dir_all(pid_dir.join("fd")).unwrap();
        std::fs::write(
            pid_dir.join("cmdline"),
            b"/home/u/.hermes/hermes-agent/venv/bin/python\0/home/u/.hermes/hermes-agent/hermes\0chat\0",
        )
        .unwrap();
        let mut fleet = Fleet::default();
        fleet.refresh_from(&Proc::at(dir.path()));
        assert_eq!(fleet.get(60).map(|p| p.agent), Some(SourceId::Hermes));
    }

    /// `std` exposes no `getppid` and the crate takes no new dependencies, so
    /// the expectation is read from a different `/proc` file than the code
    /// under test uses.
    fn parent_from_status() -> u32 {
        let raw = fs::read_to_string("/proc/self/status").unwrap();
        raw.lines()
            .find_map(|line| line.strip_prefix("PPid:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }
}
