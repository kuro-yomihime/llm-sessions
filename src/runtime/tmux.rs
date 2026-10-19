//! tmux placement for running agents.
//!
//! A pane's pid is an ancestor of whatever it launched, so `#{pane_pid}` plus
//! `/proc` ancestry pins an agent to an exact pane.
//!
//! Every function here tolerates tmux being absent, having no server, or
//! speaking a different format: the result is "no panes", never an error. The
//! explorer must stay usable outside tmux.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

/// One tmux pane, addressed the way `-t` expects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pane {
    /// `session:window.pane`, ready to pass to `-t`.
    pub address: String,
    pub session: String,
    pub window: String,
    pub pid: u32,
    pub command: String,
    pub path: PathBuf,
}

const FORMAT: &str = "#{session_name}\t#{window_index}\t#{pane_index}\t#{pane_pid}\t#{pane_current_command}\t#{pane_current_path}";

/// Every pane on the default server, or nothing.
pub fn panes() -> Vec<Pane> {
    let Some(stdout) = run(&["list-panes", "-a", "-F", FORMAT]) else {
        return Vec::new();
    };
    stdout.lines().filter_map(parse_pane).collect()
}

fn parse_pane(line: &str) -> Option<Pane> {
    let mut fields = line.split('\t');
    let session = fields.next()?.to_owned();
    let window = fields.next()?.to_owned();
    let pane = fields.next()?;
    let pid = fields.next()?.parse().ok()?;
    let command = fields.next()?.to_owned();
    // A path may legitimately contain a tab, so it takes the remainder.
    let path = fields.collect::<Vec<_>>().join("\t");
    Some(Pane {
        address: format!("{session}:{window}.{pane}"),
        session,
        window,
        pid,
        command,
        path: PathBuf::from(path),
    })
}

/// Are we running inside a tmux pane ourselves?
pub fn inside() -> bool {
    std::env::var_os("TMUX").is_some()
}

/// Move the user's focus to `pane`.
///
/// This is the one thing `llm-sessions` does that the user can see happen outside its
/// own window, so it stays behind an explicit keypress and is refused under
/// `--read-only`. Inside tmux we move our own client; outside, `switch-client`
/// retargets the most recently used client, which is the best a detached
/// process can honestly do.
pub fn focus(pane: &Pane) -> std::result::Result<(), String> {
    if inside() {
        run(&[
            "select-window",
            "-t",
            &format!("{}:{}", pane.session, pane.window),
        ])
        .ok_or_else(|| "tmux select-window failed".to_string())?;
        run(&["select-pane", "-t", &pane.address])
            .ok_or_else(|| "tmux select-pane failed".to_string())?;
        return Ok(());
    }
    run(&["switch-client", "-t", &pane.address])
        .map(|_| ())
        .ok_or_else(|| format!("no attached tmux client to switch to {}", pane.address))
}

/// Run a tmux command, returning its stdout only on success.
///
/// stderr is discarded on purpose: "no server running on /tmp/tmux-1000/default"
/// is the normal case outside tmux, not something to show the user.
fn run(args: &[&str]) -> Option<String> {
    let output = Command::new("tmux")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pane_line_becomes_a_targetable_address() {
        let pane = parse_pane("0\t2\t1\t62612\tomp\t/home/u/repos/dstm").unwrap();
        assert_eq!(pane.address, "0:2.1");
        assert_eq!(pane.pid, 62612);
        assert_eq!(pane.command, "omp");
        assert_eq!(pane.path, PathBuf::from("/home/u/repos/dstm"));
    }

    #[test]
    fn a_path_containing_a_tab_survives() {
        let pane = parse_pane("main\t0\t0\t1\tbash\t/home/u/od\td\tir").unwrap();
        assert_eq!(pane.path, PathBuf::from("/home/u/od\td\tir"));
    }

    #[test]
    fn a_truncated_line_is_dropped_rather_than_guessed() {
        assert!(parse_pane("0\t2\t1").is_none());
        assert!(parse_pane("0\t2\t1\tnot-a-pid\tomp\t/x").is_none());
        assert!(parse_pane("").is_none());
    }
}
