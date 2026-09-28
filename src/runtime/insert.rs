//! Insert mode: hand one user message to an agent's own headless CLI.
//!
//! The agent writes the turn into its store itself; `llm-sessions` never
//! touches the session file. The command runs with stdin closed and output
//! captured, and is waited on off the UI thread, so the TUI keeps drawing and
//! the terminal is never shared with the child. It is fire-and-report, with
//! one exception: a child still running at its deadline is killed, so a hung
//! CLI cannot leave a delivery pending forever.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// How often the waiting thread checks on the child.
const POLL: Duration = Duration::from_millis(50);

/// A message on its way. Poll it until it reports an outcome.
pub struct Delivery {
    /// What the status line calls the target, e.g. `omp 01a0-7b22`.
    pub label: String,
    /// The agent CLI's pid, so a user can find or kill it; `None` if it never
    /// started.
    pub pid: Option<u32>,
    outcome: Receiver<Result<(), String>>,
}

impl Delivery {
    /// `Some` once the agent has exited or been killed: `Ok` on a zero
    /// status, otherwise the reason in one line.
    pub fn poll(&self) -> Option<Result<(), String>> {
        match self.outcome.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("delivery thread vanished".into())),
        }
    }
}

/// Start `command` in `cwd` and wait for it on a background thread, killing
/// it once `deadline` has passed.
pub fn send(
    label: String,
    command: &[String],
    cwd: Option<PathBuf>,
    deadline: Duration,
) -> Delivery {
    let (tx, outcome) = mpsc::channel();
    let pid = match spawn(command, cwd) {
        Ok((child, log)) => {
            let pid = child.id();
            std::thread::spawn(move || {
                // A closed receiver means the TUI quit; the agent still ran.
                let _ = tx.send(wait(child, log, deadline));
            });
            Some(pid)
        }
        Err(error) => {
            let _ = tx.send(Err(error));
            None
        }
    };
    Delivery {
        label,
        pid,
        outcome,
    }
}

/// The child writes stdout and stderr to one unlinked temporary file rather
/// than to pipes: a pipe reader waits for every writer to close, and an agent
/// CLI may leave a daemon behind holding it — `opencode run` can start the
/// background service. A file is readable the moment the child exits.
fn spawn(command: &[String], cwd: Option<PathBuf>) -> Result<(Child, File), String> {
    let (program, args) = command.split_first().ok_or("empty command")?;
    let log = scratch_file().map_err(|e| format!("cannot capture {program}'s output: {e}"))?;
    let clone = |file: &File| {
        file.try_clone()
            .map_err(|e| format!("cannot capture {program}'s output: {e}"))
    };
    let mut child = Command::new(program);
    child
        .args(args)
        .stdin(Stdio::null())
        .stdout(clone(&log)?)
        .stderr(clone(&log)?);
    if let Some(cwd) = cwd {
        child.current_dir(cwd);
    }
    let child = child
        .spawn()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    Ok((child, log))
}

fn wait(mut child: Child, mut log: File, deadline: Duration) -> Result<(), String> {
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < deadline => std::thread::sleep(POLL),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "no answer after {}s; killed pid {}",
                    deadline.as_secs(),
                    child.id()
                ));
            }
            Err(e) => return Err(format!("cannot wait for pid {}: {e}", child.id())),
        }
    };
    if status.success() {
        return Ok(());
    }
    // CLIs put the reason last.
    let mut output = Vec::new();
    let _ = log
        .seek(SeekFrom::Start(0))
        .and_then(|_| log.read_to_end(&mut output));
    Err(String::from_utf8_lossy(&output)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map_or_else(|| status.to_string(), str::to_owned))
}

/// A fresh file nobody else can open: created exclusively, then unlinked,
/// so nothing is left in the temp dir whatever happens to this process.
fn scratch_file() -> std::io::Result<File> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "llm-sessions-insert-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    std::fs::remove_file(&path)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GENEROUS: Duration = Duration::from_secs(10);

    fn wait_for(delivery: &Delivery) -> Result<(), String> {
        let deadline = Instant::now() + GENEROUS;
        loop {
            if let Some(outcome) = delivery.poll() {
                return outcome;
            }
            assert!(Instant::now() < deadline, "delivery never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn a_failing_agent_reports_its_last_output_line() {
        let delivery = send(
            "x".into(),
            &sh("echo noise; echo 'session not found' >&2; exit 3"),
            None,
            GENEROUS,
        );
        assert_eq!(wait_for(&delivery), Err("session not found".into()));
    }

    #[test]
    fn a_missing_binary_is_an_error_not_a_hang() {
        let delivery = send(
            "x".into(),
            &["llm-sessions-no-such-agent".into()],
            None,
            GENEROUS,
        );
        assert_eq!(delivery.pid, None);
        assert!(
            wait_for(&delivery)
                .unwrap_err()
                .contains("llm-sessions-no-such-agent")
        );
    }

    /// A hung CLI is killed at its deadline instead of pending forever.
    #[test]
    fn a_hung_agent_is_killed_at_its_deadline() {
        let delivery = send(
            "x".into(),
            &sh("sleep 30"),
            None,
            Duration::from_millis(200),
        );
        let pid = delivery.pid.unwrap();
        let reason = wait_for(&delivery).unwrap_err();
        assert!(reason.contains(&format!("killed pid {pid}")), "{reason}");
        assert!(!PathBuf::from(format!("/proc/{pid}")).exists());
    }

    /// A daemon the CLI leaves behind keeps its output open; the delivery
    /// still resolves when the CLI itself exits.
    #[test]
    fn a_lingering_grandchild_does_not_hold_the_delivery_open() {
        let started = Instant::now();
        let delivery = send("x".into(), &sh("sleep 5 & exit 0"), None, GENEROUS);
        assert_eq!(wait_for(&delivery), Ok(()));
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
