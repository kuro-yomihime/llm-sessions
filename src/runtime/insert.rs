//! Insert mode: deliver a user message through an agent CLI or its tmux pane.
//!
//! The agent writes the turn into its store itself; `llm-sessions` never
//! touches the session file. The command runs with stdin closed and output
//! captured, and is waited on off the UI thread, so the TUI keeps drawing and
//! the terminal is never shared with the child. It is fire-and-report, with
//! one exception: a child still running at its deadline is killed, so a hung
//! CLI cannot leave a delivery pending forever.

use crate::model::{Liveness, Locator, SessionSummary, SourceId};
use crate::runtime::live::Fleet;
use crate::runtime::tmux::Pane;
use crate::source::Source;
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
    /// The agent CLI's pid, so a user can find or kill it; `None` for gateway
    /// delivery or a command that never started.
    pub pid: Option<u32>,
    outcome: Receiver<Result<(), String>>,
}

impl Delivery {
    /// `Some` once the harness reports delivery or failure. Gateway admission
    /// does not imply completion of the model's response.
    pub fn poll(&self) -> Option<Result<(), String>> {
        match self.outcome.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("delivery thread vanished".into())),
        }
    }

    /// Run a non-process harness without blocking the terminal event loop.
    pub(crate) fn background(
        label: String,
        work: impl FnOnce() -> crate::Result<()> + Send + 'static,
    ) -> Self {
        let (tx, outcome) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(work().map_err(|error| error.to_string()));
        });
        Self {
            label,
            pid: None,
            outcome,
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

/// The command that delivers `message` to a local `session`, or why none may.
///
/// One writer per conversation: with any process attached — exact or
/// inferred, since an inferred owner may well hold it in memory — the
/// message must go through that process or not at all. Resuming is for
/// sessions nothing is running. The two headless routes are unchanged;
/// what this adds over the old TUI logic is delivery to a running OMP
/// through its tmux pane, and only on an exact (open-fd) join — an
/// inferred owner could be a different conversation entirely, so it is
/// refused.
pub fn local_command(
    session: &SessionSummary,
    sources: &[Box<dyn Source>],
    fleet: &Fleet,
    message: &str,
) -> Result<Vec<String>, String> {
    if !session.is_local() {
        return Err("insert mode sends to local sessions only".into());
    }
    if session.subagent {
        return Err("a subagent takes messages from its parent, not from here".into());
    }
    let source = crate::source::find(sources, session.source);
    let Some(pid) = session.liveness.pid() else {
        return source
            .and_then(|source| source.insert_command(&session.id, message))
            .ok_or_else(|| format!("insert mode does not support {} yet", session.source));
    };
    let held = if matches!(session.liveness, Liveness::Live { .. }) {
        "is open"
    } else {
        "may be open"
    };
    let Some(process) = fleet.get(pid) else {
        return Err(format!(
            "session {held} in pid {pid}, which is gone; Ctrl-R rescans"
        ));
    };
    if let Some(command) =
        source.and_then(|source| source.live_insert_command(&session.id, message, &process.servers))
    {
        return Ok(command);
    }
    // What remains is an agent with no programmatic route. OMP in a tmux
    // pane can be typed to, but only on the exact join: `Live` means this
    // process holds this session's file open, `Probably` is a guess.
    if let (Liveness::Live { .. }, Some(pane)) = (session.liveness, process.pane.as_ref())
        && let Some(command) = pane_command(pane, process.agent, process.pid, session, message)
    {
        return Ok(command);
    }
    let jump = if process.pane.is_some() {
        "; J jumps to its pane"
    } else {
        ""
    };
    Err(format!(
        "session {held} in pid {pid}, which cannot take a message from here{jump}"
    ))
}

/// Delivery to a running agent's tmux pane, or `None` to fall through to
/// the caller's generic refusal.
///
/// The pane must currently foreground the agent itself: typing into a
/// shell prompt would run the message as a command, and a tool the agent
/// is running could take the text as its own input. Both are the agent's
/// business, not ours.
fn pane_command(
    pane: &Pane,
    agent: SourceId,
    pid: u32,
    session: &SessionSummary,
    message: &str,
) -> Option<Vec<String>> {
    if session.source != SourceId::Omp || agent != SourceId::Omp || pane.command != agent.name() {
        return None;
    }
    let Locator::File { path } = &session.locator else {
        // Exact liveness is an open-fd join on a file, so this is
        // unreachable; still, a non-file locator has nothing to verify.
        return None;
    };
    pane_delivery_argv(PANE_DELIVERY, pane, pid, &path.to_string_lossy(), message)
}

/// The argv that runs a `PANE_DELIVERY`-shaped `script` against `pane`,
/// shared by local delivery and the remote SSH route.
///
/// The script's positional contract is fixed — address, pane pid, agent
/// pid, expected foreground command, session path, message — so a remote
/// wrapper may prepend checks and `shift` before this text. Only an OMP
/// pane can be typed to, and control bytes are refused before any pane
/// hears of them: pasted control bytes are terminal commands, not message
/// content.
pub(crate) fn pane_delivery_argv(
    script: &str,
    pane: &Pane,
    agent_pid: u32,
    session_path: &str,
    message: &str,
) -> Option<Vec<String>> {
    if pane.command != SourceId::Omp.name() {
        return None;
    }
    if message
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return None;
    }
    Some(vec![
        "sh".into(),
        "-c".into(),
        script.into(),
        "llm-sessions-pane-delivery".into(),
        pane.address.clone(),
        pane.pid.to_string(),
        agent_pid.to_string(),
        pane.command.clone(),
        session_path.to_owned(),
        message.to_owned(),
    ])
}

/// Paste `$6` into pane `$1` and press Enter, after re-proving at delivery
/// time — not scan time — everything the route was chosen on.
///
/// Every fact is re-derived: the pane is resolved afresh from its
/// (possibly reused) address and pinned by its stable `%id`; its shell pid
/// and foreground command must be unchanged, so a closed pane whose
/// address a new one took, or a shell prompt left by an exited agent,
/// cannot receive the text. The agent pid must still live under that pane
/// and still hold the session file open — the same exact join the UI made.
/// The ancestry walk proving that is bounded — 32 hops, the ceiling
/// `pane_for` uses — and splits each stat line after the last `)`, since a
/// process name may itself contain one; a missing, malformed, self, or
/// looping parent stops the walk rather than spinning it.
/// A pane foregrounding `omp` is not yet proof that *this* `omp` reads the
/// keyboard: a second session may run in the foreground while the owner
/// sits suspended or backgrounded in the same pane, so the agent's process
/// group must be the foreground group of the pane shell's terminal. The
/// message travels as an argument and on through a tmux buffer, never
/// through a shell or a `send-keys` key name, so no part of it can become
/// a command, a key, or a flag. Bracketed paste keeps newlines from
/// submitting mid-message; only the final Enter is a keypress. If any
/// check fails the script says why and exits nonzero without touching the
/// pane.
pub(crate) const PANE_DELIVERY: &str = r#"set -eu
addr=$1 pane_pid=$2 agent_pid=$3 expected=$4 session_path=$5 message=$6
fail() { echo "$*" >&2; exit 1; }
info=$(tmux display-message -p -t "$addr" '#{pane_id} #{pane_pid} #{bracket_paste_flag} #{pane_current_command}') || fail "tmux pane $addr is gone"
pane_id=${info%% *}
info=${info#* }
got_pid=${info%% *}
info=${info#* }
paste=${info%% *}
command=${info#* }
[ "$got_pid" = "$pane_pid" ] || fail "pane $addr now belongs to pid $got_pid, not $pane_pid"
[ "$command" = "$expected" ] || fail "pane $addr now runs $command, not $expected"
[ "$paste" = "1" ] || fail "pane $addr does not accept pasted text safely"
[ -n "$(tr -d '\000' < "/proc/$agent_pid/cmdline" 2>/dev/null)" ] || fail "agent pid $agent_pid is gone"
walk=$agent_pid
found=0
hops=0
while [ "$hops" -lt 32 ]; do
    hops=$((hops + 1))
    if [ "$walk" = "$pane_pid" ]; then found=1; break; fi
    if [ "$walk" -le 1 ]; then break; fi
    line=$(cat "/proc/$walk/stat" 2>/dev/null) || break
    rest=${line##*) }
    parent=${rest#* }
    parent=${parent%% *}
    case "$parent" in ''|*[!0-9]*) break ;; esac
    [ "$parent" -gt 1 ] || break
    [ "$parent" -ne "$walk" ] || break
    walk=$parent
done
[ "$found" = 1 ] || fail "agent pid $agent_pid no longer runs in pane $addr"
held=0
for fd in /proc/$agent_pid/fd/*; do
    if [ "$(readlink "$fd" 2>/dev/null)" = "$session_path" ]; then held=1; break; fi
done
[ "$held" = 1 ] || fail "agent pid $agent_pid no longer holds $session_path open"
pane_stat=$(cat "/proc/$pane_pid/stat" 2>/dev/null) || fail "pane shell $pane_pid is gone"
agent_stat=$(cat "/proc/$agent_pid/stat" 2>/dev/null) || fail "agent pid $agent_pid is gone"
read -r _ _ _ _ _ foreground _ <<EOF
${pane_stat##*) }
EOF
read -r _ _ group _ <<EOF
${agent_stat##*) }
EOF
case "$foreground" in ''|*[!0-9]*) fail "pane $addr has no foreground process group" ;; esac
[ "$group" = "$foreground" ] || fail "agent pid $agent_pid is not the foreground job of pane $addr"
buffer="llm-sessions-insert-$$"
trap 'tmux delete-buffer -b "$buffer" 2>/dev/null || :' EXIT
printf '%s' "$message" | tmux load-buffer -b "$buffer" - \
    && tmux paste-buffer -p -d -b "$buffer" -t "$pane_id" \
    && tmux send-keys -t "$pane_id" Enter
"#;
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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

    use crate::model::{Liveness, Locator, SessionSummary, SourceId};
    use crate::runtime::live::{Fleet, Process};
    use crate::runtime::tmux::Pane;
    use crate::source::omp::Omp;
    use std::path::Path;

    fn omp_session(liveness: Liveness) -> SessionSummary {
        let mut session = SessionSummary::new(
            "s".into(),
            SourceId::Omp,
            time::OffsetDateTime::now_utc(),
            Locator::File {
                path: Path::new("/tmp/s.jsonl").into(),
            },
        );
        session.liveness = liveness;
        session
    }

    fn pane(command: &str) -> Pane {
        Pane {
            address: "work:0.1".into(),
            session: "work".into(),
            window: "0".into(),
            pid: 4242,
            command: command.into(),
            path: Path::new("/w").into(),
        }
    }

    fn sources() -> Vec<Box<dyn crate::source::Source>> {
        vec![Box::new(Omp::new(Path::new("/nonexistent-home")))]
    }

    #[test]
    fn a_remote_or_subagent_session_is_refused() {
        let mut remote = omp_session(Liveness::Stale);
        remote.origin_label = Some("elsewhere".into());
        assert_eq!(
            local_command(&remote, &sources(), &Fleet::default(), "x"),
            Err("insert mode sends to local sessions only".into())
        );
        let mut sub = omp_session(Liveness::Stale);
        sub.subagent = true;
        assert_eq!(
            local_command(&sub, &sources(), &Fleet::default(), "x"),
            Err("a subagent takes messages from its parent, not from here".into())
        );
    }

    #[test]
    fn a_running_process_that_vanished_between_scan_and_send_is_refused() {
        let refusal = local_command(
            &omp_session(Liveness::Live { pid: 999_999 }),
            &sources(),
            &Fleet::default(),
            "x",
        )
        .unwrap_err();
        assert!(refusal.contains("which is gone"), "{refusal}");
    }

    /// An inferred owner may be a different conversation in the same
    /// worktree; typing into its pane would be a confident lie.
    #[test]
    fn an_inferred_omp_owner_is_never_typed_to() {
        let fleet = Fleet::of(vec![Process::stub(7, SourceId::Omp, Some(pane("omp")))]);
        let refusal = local_command(
            &omp_session(Liveness::Probably { pid: 7 }),
            &sources(),
            &fleet,
            "hello",
        )
        .unwrap_err();
        assert!(refusal.contains("may be open"), "{refusal}");
        assert!(refusal.contains("cannot take a message"), "{refusal}");
    }

    #[test]
    fn a_pane_foregrounding_a_shell_or_tool_is_refused() {
        for foreground in ["bash", "zsh", "python"] {
            let fleet = Fleet::of(vec![Process::stub(
                7,
                SourceId::Omp,
                Some(pane(foreground)),
            )]);
            assert!(
                local_command(
                    &omp_session(Liveness::Live { pid: 7 }),
                    &sources(),
                    &fleet,
                    "hello"
                )
                .is_err(),
                "{foreground} must not receive pasted text"
            );
        }
    }

    /// Terminal control bytes are not message content; they never reach a
    /// pane in any spelling of the route.
    #[test]
    fn control_bytes_are_refused_before_any_pane_hears_of_them() {
        let fleet = Fleet::of(vec![Process::stub(7, SourceId::Omp, Some(pane("omp")))]);
        for message in ["\x1b", "line one\r\nline two", "\x07ping\x1b[A"] {
            assert!(
                local_command(
                    &omp_session(Liveness::Live { pid: 7 }),
                    &sources(),
                    &fleet,
                    message
                )
                .is_err(),
                "{message:?} must be refused"
            );
        }
    }

    /// A stand-in `tmux` on PATH, so the delivery script's behavior can be
    /// tested against the real `/proc` without a server or a pane. A `cat`
    /// beside it serves `/proc/<pid>/stat` from a `stat-<pid>` file when one
    /// exists — a test process has no terminal whose foreground could be
    /// set — and reads the real file otherwise.
    struct FakeTmux {
        bin: PathBuf,
        log: PathBuf,
    }

    impl FakeTmux {
        fn new(dir: &Path) -> Self {
            let bin = dir.join("tmux");
            let log = dir.join("log");
            std::fs::write(
                &bin,
                r#"#!/bin/sh
printf '%s\n' "$*" >> "$TMUX_LOG"
case "$1" in
    display-message) printf '%s\n' "$FAKE_REPORT" ;;
    load-buffer) cat > "$TMUX_LOG.buffer" ;;
    paste-buffer)
        { printf '\033[200~'; tr '\n' '\r' < "$TMUX_LOG.buffer"; printf '\033[201~'; } >> "$TMUX_LOG.input"
        ;;
    send-keys)
        for key; do :; done
        [ "$key" = Enter ] || exit 1
        printf '\r' >> "$TMUX_LOG.input"
        ;;
    delete-buffer) rm -f "$TMUX_LOG.buffer" ;;
    *) exit 1 ;;
esac
"#,
            )
            .unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            let cat = dir.join("cat");
            std::fs::write(
                &cat,
                r#"#!/bin/sh
case "$1" in
    /proc/*/stat)
        pid=${1#/proc/}; pid=${pid%/stat}
        stub="$(dirname "$0")/stat-$pid"
        [ -f "$stub" ] && exec /bin/cat "$stub"
        ;;
esac
exec /bin/cat "$@"
"#,
            )
            .unwrap();
            std::fs::set_permissions(&cat, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self { bin, log }
        }

        /// Make `group` the foreground process group of the terminal the
        /// pane shell `pane_pid` runs on.
        fn foreground(&self, pane_pid: &str, group: u32) {
            std::fs::write(
                self.bin.with_file_name(format!("stat-{pane_pid}")),
                format!("{pane_pid} (sh) S 1 {pane_pid} {pane_pid} 34816 {group} 4194560\n"),
            )
            .unwrap();
        }

        fn input(&self) -> Vec<u8> {
            std::fs::read(format!("{}.input", self.log.display())).unwrap_or_default()
        }

        fn log(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    fn run_delivery(tmux: &FakeTmux, report: &str, args: &[&str]) -> std::process::Output {
        let dir = tmux.bin.parent().unwrap();
        std::process::Command::new("sh")
            .arg("-c")
            .arg(PANE_DELIVERY)
            .arg("delivery")
            .args(args)
            .env("PATH", format!("{}:/usr/bin:/bin", dir.display()))
            .env("TMUX_LOG", &tmux.log)
            .env("FAKE_REPORT", report)
            .output()
            .expect("script runs")
    }

    /// A child standing in for the agent: alive, a descendant of this test,
    /// and (when `holds` names a file) holding it open.
    fn agent_child(holds: Option<&Path>) -> std::process::Child {
        let script = match holds {
            Some(path) => format!("exec sleep 60 9< '{}'", path.display()),
            None => "exec sleep 60".into(),
        };
        std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    /// A real agent stand-in that already holds `session` open when spawned
    /// and leads its own process group, as a shell's job would.
    fn owner(program: &Path, session: &Path) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        Command::new(program)
            .arg("60")
            .stdin(File::open(session).unwrap())
            .process_group(0)
            .spawn()
            .unwrap()
    }

    fn cleanup(mut child: std::process::Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn an_owned_pane_receives_one_literal_multiline_submission_with_a_parenthesis_in_comm() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session.jsonl");
        std::fs::write(&session, "{}").unwrap();
        let program = dir.path().join("omp) S 9999");
        // A link, not a copy: a freshly written executable can still be open
        // for writing in a sibling test's forked child, and exec then fails
        // with ETXTBSY. The kernel names the process after the link anyway.
        std::os::unix::fs::symlink("/bin/sleep", &program).unwrap();
        let agent = owner(&program, &session);
        let tmux = FakeTmux::new(dir.path());
        let parent = std::process::id().to_string();
        tmux.foreground(&parent, agent.id());
        let message = "-n 'literal' $(not-executed)\nsecond line \t with unicode λ";
        let output = run_delivery(
            &tmux,
            &format!("%7 {parent} 1 omp"),
            &[
                "work:0.1",
                &parent,
                &agent.id().to_string(),
                "omp",
                session.to_str().unwrap(),
                message,
            ],
        );
        cleanup(agent);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let received = tmux.input();
        let expected = format!("\x1b[200~{}\x1b[201~\r", message.replace('\n', "\r"));
        assert_eq!(
            received,
            expected.as_bytes(),
            "the entire message is one bracketed paste followed by one submission"
        );
    }

    /// Two sessions share one pane: A's owner was backgrounded and B now
    /// reads the keyboard. Both hold their own transcript and both descend
    /// from the pane shell, so only the terminal's foreground process group
    /// tells them apart — and A's message must never reach B's composer.
    #[test]
    fn a_backgrounded_owner_is_refused_while_another_session_holds_the_foreground() {
        let dir = tempfile::tempdir().unwrap();
        let a_session = dir.path().join("a.jsonl");
        let b_session = dir.path().join("b.jsonl");
        std::fs::write(&a_session, "{}").unwrap();
        std::fs::write(&b_session, "{}").unwrap();
        let sleep = Path::new("/bin/sleep");
        let a = owner(sleep, &a_session);
        let b = owner(sleep, &b_session);
        let tmux = FakeTmux::new(dir.path());
        let pane_pid = std::process::id().to_string();
        tmux.foreground(&pane_pid, b.id());
        let report = format!("%7 {pane_pid} 1 omp");
        let deliver = |agent: &std::process::Child, session: &Path, message: &str| {
            run_delivery(
                &tmux,
                &report,
                &[
                    "work:0.1",
                    &pane_pid,
                    &agent.id().to_string(),
                    "omp",
                    session.to_str().unwrap(),
                    message,
                ],
            )
        };
        let to_a = deliver(&a, &a_session, "MESSAGE_FOR_SESSION_A");
        let a_input = tmux.input();
        let to_b = deliver(&b, &b_session, "MESSAGE_FOR_SESSION_B");
        cleanup(a);
        cleanup(b);
        assert!(!to_a.status.success());
        let stderr = String::from_utf8_lossy(&to_a.stderr);
        assert!(stderr.contains("not the foreground job"), "{stderr}");
        assert!(
            a_input.is_empty(),
            "the foreground session heard A's message"
        );
        assert!(
            to_b.status.success(),
            "{}",
            String::from_utf8_lossy(&to_b.stderr)
        );
        assert_eq!(tmux.input(), b"\x1b[200~MESSAGE_FOR_SESSION_B\x1b[201~\r");
    }

    #[test]
    fn malformed_self_parent_and_cyclic_ancestry_refuse_without_hanging() {
        for kind in ["malformed", "self", "cycle"] {
            let dir = tempfile::tempdir().unwrap();
            let agent = agent_child(None);
            let tmux = FakeTmux::new(dir.path());
            let pid = agent.id();
            let parent = match kind {
                "malformed" => "bad".to_owned(),
                "self" => pid.to_string(),
                _ => "7".to_owned(),
            };
            std::fs::write(
                dir.path().join(format!("stat-{pid}")),
                format!("{pid} (odd) S 9999) S {parent} 0\n"),
            )
            .unwrap();
            std::fs::write(dir.path().join("stat-7"), "7 (cycle) S 8 0\n").unwrap();
            std::fs::write(dir.path().join("stat-8"), "8 (cycle) S 7 0\n").unwrap();
            let report = "%7 987654 1 omp";
            let argv = vec![
                "env".into(),
                format!("PATH={}:/usr/bin:/bin", dir.path().display()),
                format!("TMUX_LOG={}", tmux.log.display()),
                format!("FAKE_REPORT={report}"),
                "sh".into(),
                "-c".into(),
                PANE_DELIVERY.into(),
                "delivery".into(),
                "work:0.1".into(),
                "987654".into(),
                pid.to_string(),
                "omp".into(),
                "/unused/session.jsonl".into(),
                "must not submit".into(),
            ];
            let result = wait_for(&send(
                "ancestry".into(),
                &argv,
                None,
                Duration::from_secs(2),
            ));
            cleanup(agent);
            assert!(
                result.unwrap_err().contains("no longer runs in pane"),
                "{kind}"
            );
            assert!(!std::path::Path::new(&format!("{}.input", tmux.log.display())).exists());
        }
    }

    #[test]
    fn the_script_refuses_a_pane_that_changed_under_the_address() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent_child(None);
        let tmux = FakeTmux::new(dir.path());
        let report = format!("%7 {} 1 omp", agent.id());
        let output = run_delivery(
            &tmux,
            &report,
            &[
                "work:0.1",
                &std::process::id().to_string(),
                &agent.id().to_string(),
                "omp",
                "/tmp/none.jsonl",
                "hello",
            ],
        );
        cleanup(agent);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("now belongs to pid"), "{stderr}");
        assert!(!tmux.log().contains("paste-buffer"));
    }

    #[test]
    fn the_script_refuses_a_shell_prompt_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent_child(None);
        let pane_pid = std::process::id().to_string();
        let tmux = FakeTmux::new(dir.path());
        let output = run_delivery(
            &tmux,
            &format!("%7 {pane_pid} 1 bash"),
            &[
                "work:0.1",
                &pane_pid,
                &agent.id().to_string(),
                "omp",
                "/tmp/none.jsonl",
                "rm -rf ~",
            ],
        );
        cleanup(agent);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("now runs"), "{stderr}");
        assert!(!tmux.log().contains("send-keys"));
    }

    #[test]
    fn the_script_refuses_an_agent_that_left_the_pane_or_let_go_of_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let pane_pid = std::process::id().to_string();
        let tmux = FakeTmux::new(dir.path());
        // pid 1 is alive but is not inside this pane.
        let outside = run_delivery(
            &tmux,
            &format!("%7 {pane_pid} 1 omp"),
            &["work:0.1", &pane_pid, "1", "omp", "/tmp/none.jsonl", "x"],
        );
        assert!(String::from_utf8_lossy(&outside.stderr).contains("no longer runs in pane"));
        // A live descendant that holds nothing open.
        let unbound = agent_child(None);
        let no_fd = run_delivery(
            &tmux,
            &format!("%7 {pane_pid} 1 omp"),
            &[
                "work:0.1",
                &pane_pid,
                &unbound.id().to_string(),
                "omp",
                "/tmp/none.jsonl",
                "x",
            ],
        );
        cleanup(unbound);
        assert!(String::from_utf8_lossy(&no_fd.stderr).contains("no longer holds"));
    }
}
