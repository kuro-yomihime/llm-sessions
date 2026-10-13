//! Read-only health checks for the local agent installation.
//!
//! A doctor report must be useful even when one runtime artifact is corrupt or
//! unreadable, so every failed probe becomes a finding instead of aborting the
//! whole report. It never removes, checkpoints, or otherwise changes agent data.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::model::SessionSummary;
use crate::storage::index::Index;
use crate::storage::overlay::Overlay;

const WAL_THRESHOLD: u64 = 8 * 1024 * 1024;
const TOP_DEAD_PATHS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub check: &'static str,
    pub detail: String,
    pub fix: Option<String>,
}

/// Run every read-only installation health check.
///
/// `sessions` is the tier-1 scan result so the dead-worktree check never opens
/// an agent store a second time. Individual checks deliberately degrade to an
/// informational finding: a broken runtime artifact should not make `doctor`
/// itself unusable.
pub fn run(home: &Path, sessions: &[SessionSummary]) -> Vec<Finding> {
    run_at(
        home,
        &Index::default_path(home),
        &Overlay::default_path(home),
        Path::new("/proc"),
        sessions,
    )
}

fn run_at(
    home: &Path,
    index_path: &Path,
    overlay_path: &Path,
    proc_root: &Path,
    sessions: &[SessionSummary],
) -> Vec<Finding> {
    let mut findings = Vec::new();
    check_brokers(home, proc_root, &mut findings);
    check_dead_paths(sessions, &mut findings);
    check_locks(home, proc_root, &mut findings);
    check_wals(home, proc_root, &mut findings);
    check_caches(index_path, overlay_path, sessions, &mut findings);
    findings
}

/// Remote-host health: a `hosts.json` that cannot be parsed, and machines
/// that cannot be reached. One `ssh` per configured host — exactly the
/// failure mode `doctor` exists to surface before a listing needs it.
pub fn remote_findings(home: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    let (config, failure) = crate::source::remote::Config::load(home);
    if let Some(failure) = failure {
        warn(&mut findings, "remote hosts", failure, None);
    }
    for host in config.hosts {
        let host = crate::source::remote::Host::new(host);
        let status = host.status();
        if let Some(message) = status.strip_prefix("unreachable: ") {
            warn(
                &mut findings,
                "remote hosts",
                format!("{}: {message}", host.name()),
                None,
            );
        }
    }
    findings
}

fn check_brokers(home: &Path, proc_root: &Path, findings: &mut Vec<Finding>) {
    let root = home.join(".omp/run/daemons");
    let Some(entries) = directory_entries(&root, "orphaned brokers", findings) else {
        return;
    };

    for entry in entries {
        let daemon = entry.path();
        let is_directory = match entry.file_type() {
            Ok(kind) => kind.is_dir(),
            Err(error) => {
                info(
                    findings,
                    "orphaned brokers",
                    format!("cannot inspect {}: {error}", daemon.display()),
                );
                false
            }
        };
        if !is_directory {
            continue;
        }
        let pid_path = daemon.join("broker.pid");
        match pid_path.try_exists() {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                info(
                    findings,
                    "orphaned brokers",
                    format!("cannot inspect {}: {error}", pid_path.display()),
                );
                continue;
            }
        }
        let pid = match read_pid_json(&pid_path) {
            Ok(pid) => pid,
            Err(error) => {
                info(
                    findings,
                    "orphaned brokers",
                    format!("cannot read broker pid {}: {error}", pid_path.display()),
                );
                continue;
            }
        };

        let live_broker = match probe_process(proc_root, pid) {
            ProcessProbe::Dead => {
                warn(
                    findings,
                    "orphaned brokers",
                    format!("broker {} in {} is not running", pid, daemon.display()),
                    Some(remove_dir_command(&daemon)),
                );
                false
            }
            ProcessProbe::Unreadable(error) => {
                info(
                    findings,
                    "orphaned brokers",
                    format!(
                        "cannot inspect broker pid {pid} in {}: {error}",
                        daemon.display()
                    ),
                );
                false
            }
            ProcessProbe::Live(process)
                if pid == std::process::id() || process.looks_like_omp() =>
            {
                true
            }
            ProcessProbe::Live(process) => {
                warn(
                    findings,
                    "orphaned brokers",
                    format!(
                        "broker pid {pid} in {} belongs to {}, not omp",
                        daemon.display(),
                        process.describe()
                    ),
                    Some(remove_dir_command(&daemon)),
                );
                false
            }
        };

        if !live_broker {
            continue;
        }

        let scope_path = daemon.join("scope.json");
        match project_dir(&scope_path) {
            Ok(Some(project)) => match project.try_exists() {
                Ok(false) => warn(
                    findings,
                    "orphaned brokers",
                    format!(
                        "live broker {pid} in {} still scopes removed project {}",
                        daemon.display(),
                        project.display()
                    ),
                    Some(remove_dir_command(&daemon)),
                ),
                Ok(true) => {}
                Err(error) => info(
                    findings,
                    "orphaned brokers",
                    format!(
                        "cannot inspect broker project {}: {error}",
                        project.display()
                    ),
                ),
            },
            Ok(None) => {}
            Err(error) => info(
                findings,
                "orphaned brokers",
                format!("cannot read broker scope {}: {error}", scope_path.display()),
            ),
        }
    }
}

fn check_dead_paths(sessions: &[SessionSummary], findings: &mut Vec<Finding>) {
    let mut missing: BTreeMap<&str, BTreeMap<String, usize>> = BTreeMap::new();
    let mut unreadable: BTreeMap<&str, usize> = BTreeMap::new();

    for session in sessions {
        let Some(path) = &session.recorded_cwd else {
            continue;
        };
        match path.try_exists() {
            Ok(false) => {
                let count = missing
                    .entry(session.source.name())
                    .or_default()
                    .entry(path.display().to_string())
                    .or_default();
                *count = count.saturating_add(1);
            }
            Ok(true) => {}
            Err(_) => {
                let count = unreadable.entry(session.source.name()).or_default();
                *count = count.saturating_add(1);
            }
        }
    }

    for (source, paths) in missing {
        let total = paths.values().copied().fold(0usize, usize::saturating_add);
        let mut offenders: Vec<_> = paths.into_iter().collect();
        offenders.sort_by(|(left_path, left_count), (right_path, right_count)| {
            right_count
                .cmp(left_count)
                .then_with(|| left_path.cmp(right_path))
        });
        let top = offenders
            .into_iter()
            .take(TOP_DEAD_PATHS)
            .map(|(path, count)| format!("{count}\u{00d7} {path}"))
            .collect::<Vec<_>>()
            .join(", ");
        warn(
            findings,
            "dead-path sessions",
            format!(
                "{source}: {total} session(s) reference removed working directories; top: {top}"
            ),
            None,
        );
    }

    for (source, count) in unreadable {
        info(
            findings,
            "dead-path sessions",
            format!(
                "cannot determine whether {count} {source} recorded working directory(s) exist"
            ),
        );
    }
}

fn check_locks(home: &Path, proc_root: &Path, findings: &mut Vec<Finding>) {
    let devin = crate::data_home(home).join("devin/cli/session_locks");
    let locks = directory_entries(&devin, "stale locks", findings).unwrap_or_default();
    for lock in locks {
        let path = lock.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("lock") {
            continue;
        }
        let Some(pid) = fs::read_to_string(&path)
            .ok()
            .and_then(|contents| contents.trim().parse::<u32>().ok())
        else {
            info(
                findings,
                "stale locks",
                format!("cannot read lock owner pid from {}", path.display()),
            );
            continue;
        };
        match probe_process(proc_root, pid) {
            ProcessProbe::Dead => warn(
                findings,
                "stale locks",
                format!(
                    "Devin lock {} has no live owner (age {})",
                    path.display(),
                    age(&path)
                ),
                Some(remove_file_command(&path)),
            ),
            ProcessProbe::Unreadable(error) => info(
                findings,
                "stale locks",
                format!("cannot inspect Devin lock owner pid {pid}: {error}"),
            ),
            ProcessProbe::Live(_) => {}
        }
    }

    let claude = home.join(".claude/session-env");
    let Some(session_envs) = directory_entries(&claude, "stale locks", findings) else {
        return;
    };
    if session_envs.is_empty() {
        return;
    }
    let processes = match list_processes(proc_root) {
        Ok(processes) => processes,
        Err(error) => {
            info(
                findings,
                "stale locks",
                format!("cannot inspect processes for Claude session environments: {error}"),
            );
            return;
        }
    };
    for session_env in session_envs {
        let path = session_env.path();
        let is_directory = match session_env.file_type() {
            Ok(kind) => kind.is_dir(),
            Err(error) => {
                info(
                    findings,
                    "stale locks",
                    format!("cannot inspect {}: {error}", path.display()),
                );
                false
            }
        };
        if !is_directory {
            continue;
        }
        let Some(id) = path.file_name().and_then(|name| name.to_str()) else {
            info(
                findings,
                "stale locks",
                format!(
                    "cannot decode Claude session environment name {}",
                    path.display()
                ),
            );
            continue;
        };
        if processes
            .iter()
            .any(|process| process.command.contains(id) || process.comm.contains(id))
        {
            continue;
        }
        warn(
            findings,
            "stale locks",
            format!(
                "Claude session environment {} has no matching live process (age {})",
                path.display(),
                age(&path)
            ),
            Some(remove_dir_command(&path)),
        );
    }
}

fn check_wals(home: &Path, proc_root: &Path, findings: &mut Vec<Finding>) {
    let roots = [
        (home.join(".omp/agent"), "omp"),
        (crate::data_home(home).join("opencode"), "opencode"),
    ];
    let processes = match list_processes(proc_root) {
        Ok(processes) => Some(processes),
        Err(error) => {
            info(
                findings,
                "orphaned WAL",
                format!("cannot inspect processes for WAL writers: {error}"),
            );
            None
        }
    };

    for (root, agent) in roots {
        let Some(entries) = directory_entries(&root, "orphaned WAL", findings) else {
            continue;
        };
        for entry in entries {
            let wal = entry.path();
            if !wal
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".db-wal"))
            {
                continue;
            }
            let size = match entry.metadata() {
                Ok(metadata) => metadata.len(),
                Err(error) => {
                    info(
                        findings,
                        "orphaned WAL",
                        format!("cannot inspect {}: {error}", wal.display()),
                    );
                    continue;
                }
            };
            if size <= WAL_THRESHOLD {
                continue;
            }
            let Some(processes) = &processes else {
                continue;
            };
            let Some(db) = wal
                .to_str()
                .and_then(|path| path.strip_suffix("-wal"))
                .map(PathBuf::from)
            else {
                info(
                    findings,
                    "orphaned WAL",
                    format!("cannot derive database path from {}", wal.display()),
                );
                continue;
            };
            match writer_holds_database(proc_root, processes, &db) {
                WriterProbe::Present => {}
                WriterProbe::Absent => warn(
                    findings,
                    "orphaned WAL",
                    format!(
                        "{} is {} with no live {agent} writer holding {}",
                        wal.display(),
                        bytes(size),
                        db.display()
                    ),
                    None,
                ),
                WriterProbe::Unreadable(error) => info(
                    findings,
                    "orphaned WAL",
                    format!(
                        "cannot determine whether {} has a live writer: {error}",
                        wal.display()
                    ),
                ),
            }
        }
    }
}

fn check_caches(
    index_path: &Path,
    overlay_path: &Path,
    sessions: &[SessionSummary],
    findings: &mut Vec<Finding>,
) {
    check_cache_size("index cache", index_path, findings);
    check_index_share(index_path, sessions, findings);
    match overlay_path.try_exists() {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            info(
                findings,
                "llm-sessions caches",
                format!(
                    "cannot inspect overlay cache {}: {error}",
                    overlay_path.display()
                ),
            );
            return;
        }
    }

    let size = match fs::metadata(overlay_path) {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            info(
                findings,
                "llm-sessions caches",
                format!(
                    "cannot inspect overlay cache {}: {error}",
                    overlay_path.display()
                ),
            );
            return;
        }
    };
    let present: HashSet<(&str, &str)> = sessions
        .iter()
        .map(|session| (session.source.name(), session.id.as_str()))
        .collect();
    let (rows, stale) = match overlay_counts(overlay_path, &present) {
        Ok(counts) => counts,
        Err(error) => {
            info(
                findings,
                "llm-sessions caches",
                format!(
                    "cannot read overlay cache {}: {error}",
                    overlay_path.display()
                ),
            );
            return;
        }
    };
    let detail = format!(
        "overlay cache {} is {}; {stale} of {rows} archived row(s) point at sessions no longer present",
        overlay_path.display(),
        bytes(size)
    );
    if stale == 0 {
        findings.push(Finding {
            severity: Severity::Info,
            check: "llm-sessions caches",
            detail,
            fix: Some(remove_database_command(overlay_path)),
        });
    } else {
        warn(
            findings,
            "llm-sessions caches",
            detail,
            Some(remove_database_command(overlay_path)),
        );
    }
}

/// An index larger than this share of the sessions it covers has stopped
/// being a cache.
const INDEX_SHARE_LIMIT: f64 = 0.5;

/// Warn when the body index outgrows the corpus it indexes.
///
/// It grew 545 MB -> 631 MB in three hours of ordinary work before retention
/// existed, so the question is not whether it drifts but when someone
/// notices. Only file-backed sessions have a comparable size; database rows
/// do not, so the comparison is stated as covering only what can be measured.
fn check_index_share(index_path: &Path, sessions: &[SessionSummary], findings: &mut Vec<Finding>) {
    let Ok(metadata) = fs::metadata(index_path) else {
        return;
    };
    let corpus: u64 = sessions
        .iter()
        .filter_map(|session| session.size_bytes)
        .sum();
    if corpus == 0 {
        return;
    }
    let share = metadata.len() as f64 / corpus as f64;
    if share <= INDEX_SHARE_LIMIT {
        return;
    }
    findings.push(Finding {
        severity: Severity::Warn,
        check: "llm-sessions caches",
        detail: format!(
            "index cache is {} against {} of file-backed sessions ({:.0}%)",
            bytes(metadata.len()),
            bytes(corpus),
            share * 100.0
        ),
        fix: Some("llm-sessions index --max-body 65536 && llm-sessions index --vacuum".to_string()),
    });
}

fn check_cache_size(label: &str, path: &Path, findings: &mut Vec<Finding>) {
    match path.try_exists() {
        Ok(false) => return,
        Err(error) => {
            info(
                findings,
                "llm-sessions caches",
                format!("cannot inspect {label} {}: {error}", path.display()),
            );
            return;
        }
        Ok(true) => {}
    }
    match fs::metadata(path) {
        Ok(metadata) => findings.push(Finding {
            severity: Severity::Info,
            check: "llm-sessions caches",
            detail: format!("{label} {} is {}", path.display(), bytes(metadata.len())),
            fix: Some(remove_database_command(path)),
        }),
        Err(error) => info(
            findings,
            "llm-sessions caches",
            format!("cannot inspect {label} {}: {error}", path.display()),
        ),
    }
}

fn overlay_counts(
    path: &Path,
    present: &HashSet<(&str, &str)>,
) -> std::result::Result<(usize, usize), String> {
    let (connection, _) = crate::storage::sqlite::open(path).map_err(|error| error.to_string())?;
    let mut statement = connection
        .prepare("SELECT source, id FROM archived")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;
    let mut total = 0usize;
    let mut stale = 0usize;
    for row in rows {
        let (source, id) = row.map_err(|error| error.to_string())?;
        total = total.saturating_add(1);
        if !present.contains(&(source.as_str(), id.as_str())) {
            stale = stale.saturating_add(1);
        }
    }
    Ok((total, stale))
}

fn writer_holds_database(proc_root: &Path, processes: &[Process], database: &Path) -> WriterProbe {
    let proc = crate::runtime::procfs::Proc::at(proc_root);
    let mut unreadable = None;
    for process in processes {
        let descriptors = match proc.descriptors(process.pid) {
            Ok(descriptors) => descriptors,
            Err(error) => {
                unreadable = Some(format!("cannot read /proc/{}/fd: {error}", process.pid));
                continue;
            }
        };
        for (descriptor, target) in descriptors {
            match target {
                Some(target) if target == database => {
                    match proc.descriptor_is_writable(process.pid, &descriptor) {
                        Ok(true) => return WriterProbe::Present,
                        Ok(false) => {}
                        Err(error) => unreadable = Some(error),
                    }
                }
                Some(_) => {}
                None => {
                    unreadable = Some("cannot inspect process file descriptor".to_owned());
                }
            }
        }
    }
    match unreadable {
        Some(error) => WriterProbe::Unreadable(error),
        None => WriterProbe::Absent,
    }
}

fn directory_entries(
    path: &Path,
    check: &'static str,
    findings: &mut Vec<Finding>,
) -> Option<Vec<fs::DirEntry>> {
    match fs::read_dir(path) {
        Ok(entries) => {
            let mut result = Vec::new();
            for entry in entries {
                match entry {
                    Ok(entry) => result.push(entry),
                    Err(error) => info(
                        findings,
                        check,
                        format!(
                            "cannot inspect directory entry in {}: {error}",
                            path.display()
                        ),
                    ),
                }
            }
            Some(result)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            info(
                findings,
                check,
                format!("cannot read {}: {error}", path.display()),
            );
            None
        }
    }
}

fn read_pid_json(path: &Path) -> std::result::Result<u32, String> {
    let contents = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let value: serde_json::Value =
        serde_json::from_str(&contents).map_err(|error| error.to_string())?;
    let pid = value
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "missing numeric pid".to_owned())?;
    u32::try_from(pid).map_err(|_| "pid exceeds u32".to_owned())
}

fn project_dir(path: &Path) -> std::result::Result<Option<PathBuf>, String> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let value: serde_json::Value =
                serde_json::from_str(&contents).map_err(|error| error.to_string())?;
            Ok(value
                .get("projectDir")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

#[derive(Debug)]
struct Process {
    pid: u32,
    comm: String,
    command: String,
    state: char,
    parent: u32,
}

impl Process {
    fn looks_like_omp(&self) -> bool {
        let comm = self.comm.to_ascii_lowercase();
        if comm == "omp" || comm.starts_with("omp ") {
            return true;
        }
        self.command
            .split_whitespace()
            .next()
            .and_then(|program| Path::new(program).file_name())
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("omp"))
    }

    fn describe(&self) -> String {
        if self.command.is_empty() {
            format!(
                "{} (state {}, parent {})",
                self.comm, self.state, self.parent
            )
        } else {
            self.command.clone()
        }
    }
}

enum ProcessProbe {
    Dead,
    Live(Process),
    Unreadable(String),
}

/// Facts through [`crate::runtime::procfs`]; probe judgement stays here.
fn probe_process(proc_root: &Path, pid: u32) -> ProcessProbe {
    let proc = crate::runtime::procfs::Proc::at(proc_root);
    match proc.exists(pid) {
        Ok(false) => {
            return match proc.pids() {
                Ok(_) => ProcessProbe::Dead,
                Err(error) => ProcessProbe::Unreadable(format!(
                    "cannot enumerate {}: {error}",
                    proc_root.display()
                )),
            };
        }
        Err(error) => return ProcessProbe::Unreadable(error),
        Ok(true) => {}
    }
    let unreadable =
        |what: &str| ProcessProbe::Unreadable(format!("cannot read /proc/{pid}/{what}"));
    let Some(comm) = proc.comm(pid) else {
        return unreadable("comm");
    };
    let Some(stat) = proc.stat(pid) else {
        return unreadable("stat");
    };
    let Some(argv) = proc.argv(pid) else {
        return unreadable("cmdline");
    };
    let command = argv.join(" ");
    ProcessProbe::Live(Process {
        pid,
        comm,
        command,
        state: stat.state,
        parent: stat.parent,
    })
}

fn list_processes(proc_root: &Path) -> std::result::Result<Vec<Process>, String> {
    let proc = crate::runtime::procfs::Proc::at(proc_root);
    let mut processes = Vec::new();
    for pid in proc
        .pids()
        .map_err(|error| format!("cannot enumerate {}: {error}", proc_root.display()))?
    {
        match probe_process(proc_root, pid) {
            ProcessProbe::Live(process) => processes.push(process),
            ProcessProbe::Dead => {}
            ProcessProbe::Unreadable(error) => {
                return Err(format!("cannot completely inspect /proc/{pid}: {error}"));
            }
        }
    }
    Ok(processes)
}

enum WriterProbe {
    Present,
    Absent,
    Unreadable(String),
}

fn age(path: &Path) -> String {
    match fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .and_then(|modified| {
            SystemTime::now()
                .duration_since(modified)
                .map_err(std::io::Error::other)
        }) {
        Ok(age) => format_duration(age),
        Err(_) => "unknown".to_owned(),
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 86_400 {
        format!("{}d", seconds / 86_400)
    } else if seconds >= 3_600 {
        format!("{}h", seconds / 3_600)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

fn bytes(value: u64) -> String {
    if value >= 1024 * 1024 {
        format!("{:.1} MiB", value as f64 / f64::from(1024 * 1024))
    } else if value >= 1024 {
        format!("{:.1} KiB", value as f64 / 1024.0)
    } else {
        format!("{value} B")
    }
}

fn remove_file_command(path: &Path) -> String {
    format!(
        "rm -f -- {}",
        crate::runtime::resume::shell_quote(&path.to_string_lossy())
    )
}

fn remove_dir_command(path: &Path) -> String {
    format!(
        "rm -rf -- {}",
        crate::runtime::resume::shell_quote(&path.to_string_lossy())
    )
}

fn remove_database_command(path: &Path) -> String {
    let wal = PathBuf::from(format!("{}-wal", path.to_string_lossy()));
    let shm = PathBuf::from(format!("{}-shm", path.to_string_lossy()));
    format!(
        "rm -f -- {} {} {}",
        crate::runtime::resume::shell_quote(&path.to_string_lossy()),
        crate::runtime::resume::shell_quote(&wal.to_string_lossy()),
        crate::runtime::resume::shell_quote(&shm.to_string_lossy())
    )
}

fn info(findings: &mut Vec<Finding>, check: &'static str, detail: String) {
    findings.push(Finding {
        severity: Severity::Info,
        check,
        detail,
        fix: None,
    });
}

fn warn(findings: &mut Vec<Finding>, check: &'static str, detail: String, fix: Option<String>) {
    findings.push(Finding {
        severity: Severity::Warn,
        check,
        detail,
        fix,
    });
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use time::OffsetDateTime;

    use super::*;
    use crate::model::{Liveness, Locator, PathTrust, SourceId};

    fn session(source: SourceId, id: &str, cwd: &Path) -> SessionSummary {
        SessionSummary {
            id: id.to_owned(),
            source,
            workspace: None,
            recorded_cwd: Some(cwd.to_path_buf()),
            trust: PathTrust::Recorded,
            title: None,
            created: None,
            updated: OffsetDateTime::UNIX_EPOCH,
            model: None,
            parent: None,
            archived: false,
            branch: None,
            subagent: false,
            change: None,
            liveness: Liveness::Stale,
            origin_label: None,
            origin_id: None,
            routes: Vec::new(),
            usage: None,
            size_bytes: None,
            locator: Locator::File {
                path: PathBuf::from("fixture.jsonl"),
            },
        }
    }

    fn fixture_run(home: &Path, sessions: &[SessionSummary]) -> Vec<Finding> {
        run_at(
            home,
            &home.join("cache/index.db"),
            &home.join("state/overlay.db"),
            Path::new("/proc"),
            sessions,
        )
    }

    #[test]
    fn reports_a_broker_pid_that_cannot_exist() {
        let home = tempfile::tempdir().unwrap();
        let daemon = home.path().join(".omp/run/daemons/stale");
        fs::create_dir_all(&daemon).unwrap();
        fs::write(daemon.join("broker.pid"), r#"{"pid":4294967295}"#).unwrap();
        fs::write(daemon.join("scope.json"), r#"{"projectDir":"/"}"#).unwrap();

        let findings = fixture_run(home.path(), &[]);

        assert!(findings.iter().any(|finding| {
            finding.check == "orphaned brokers"
                && finding.severity == Severity::Warn
                && finding.detail.contains("not running")
        }));
    }

    #[test]
    fn does_not_report_a_broker_owned_by_this_process() {
        let home = tempfile::tempdir().unwrap();
        let daemon = home.path().join(".omp/run/daemons/live");
        fs::create_dir_all(&daemon).unwrap();
        fs::write(
            daemon.join("broker.pid"),
            format!(r#"{{"pid":{}}}"#, std::process::id()),
        )
        .unwrap();
        fs::write(
            daemon.join("scope.json"),
            format!(r#"{{"projectDir":"{}"}}"#, home.path().display()),
        )
        .unwrap();

        let findings = fixture_run(home.path(), &[]);

        assert!(
            !findings
                .iter()
                .any(|finding| finding.check == "orphaned brokers")
        );
    }

    #[test]
    fn aggregates_dead_paths_by_source() {
        let home = tempfile::tempdir().unwrap();
        let removed = home.path().join("removed-worktree");
        let sessions = vec![
            session(SourceId::Omp, "one", &removed),
            session(SourceId::Omp, "two", &removed),
            session(SourceId::Omp, "three", &removed),
            session(SourceId::Claude, "four", &removed),
        ];

        let findings = fixture_run(home.path(), &sessions);
        let dead_paths: Vec<_> = findings
            .iter()
            .filter(|finding| finding.check == "dead-path sessions")
            .collect();

        assert_eq!(dead_paths.len(), 2);
        assert!(
            dead_paths
                .iter()
                .any(|finding| finding.detail.contains("omp: 3 session(s)"))
        );
    }

    #[test]
    fn missing_roots_are_quiet() {
        let home = tempfile::tempdir().unwrap();
        assert!(fixture_run(home.path(), &[]).is_empty());
    }

    #[test]
    fn unavailable_process_inventory_never_suggests_cleanup() {
        let home = tempfile::tempdir().unwrap();
        let daemon = home.path().join(".omp/run/daemons/stale");
        fs::create_dir_all(&daemon).unwrap();
        fs::write(daemon.join("broker.pid"), r#"{"pid":42}"#).unwrap();
        let env = home.path().join(".claude/session-env/fixture");
        fs::create_dir_all(&env).unwrap();
        let proc_root = home.path().join("missing-proc");

        assert!(matches!(
            probe_process(&proc_root, 42),
            ProcessProbe::Unreadable(_)
        ));
        assert!(list_processes(&proc_root).is_err());

        let mut findings = Vec::new();
        check_brokers(home.path(), &proc_root, &mut findings);
        check_locks(home.path(), &proc_root, &mut findings);
        assert!(findings.iter().any(|finding| {
            finding.check == "orphaned brokers" && finding.severity == Severity::Info
        }));
        assert!(findings.iter().any(|finding| {
            finding.check == "stale locks" && finding.severity == Severity::Info
        }));
        assert!(!findings.iter().any(|finding| finding.fix.is_some()));

        fs::write(&proc_root, "").unwrap();
        assert!(matches!(
            probe_process(&proc_root, 42),
            ProcessProbe::Unreadable(_)
        ));
        assert!(list_processes(&proc_root).is_err());
        fs::remove_file(&proc_root).unwrap();

        fs::create_dir(&proc_root).unwrap();
        assert!(matches!(probe_process(&proc_root, 42), ProcessProbe::Dead));
        assert!(list_processes(&proc_root).unwrap().is_empty());
    }

    #[test]
    fn unreadable_cmdline_is_not_an_empty_cmdline() {
        let dir = tempfile::tempdir().unwrap();
        let proc_root = dir.path().join("proc");
        let pid_dir = proc_root.join("42");
        fs::create_dir_all(&pid_dir).unwrap();
        fs::write(pid_dir.join("comm"), "omp\n").unwrap();
        fs::write(pid_dir.join("stat"), "42 (omp) S 1 0 0 0 0 0 0 0 0 0 5 7\n").unwrap();

        assert!(matches!(
            probe_process(&proc_root, 42),
            ProcessProbe::Unreadable(error) if error.contains("cmdline")
        ));
        assert!(list_processes(&proc_root).is_err());

        fs::write(pid_dir.join("cmdline"), "").unwrap();
        assert!(matches!(
            probe_process(&proc_root, 42),
            ProcessProbe::Live(process) if process.command.is_empty()
        ));
        assert_eq!(list_processes(&proc_root).unwrap().len(), 1);
    }
}
