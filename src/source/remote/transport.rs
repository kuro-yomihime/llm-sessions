//! How a remote host is reached: the `ssh` invocation itself, the one-time
//! probe for a remote `llm-sessions` and `sqlite3`, and the `sh` scripts the
//! shell transport sends.

use std::fmt::Write as _;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::error::{Error, Result};
use crate::source::remote::Host;

/// Newest transcripts considered per source per host. A fleet machine holds
/// thousands; the interesting ones are the recent ones, and every extra path
/// costs a bounded `head` in the same round trip.
const REMOTE_FILES_PER_SOURCE: usize = 200;

/// Bytes read from each remote transcript, matching OMP's own head read: the
/// header record plus its optional title slot.
const REMOTE_HEAD_BYTES: usize = 8 * 1024;

/// What probing a host learned, once per process.
#[derive(Clone, Debug)]
pub(super) struct Probe {
    pub(super) home: PathBuf,
    /// How to invoke a remote `llm-sessions`, when one exists. The path matters:
    /// sshd's default PATH has no user-writable entries, so a binary in
    /// `~/.local/bin` is invisible to `command -v` over ssh.
    pub(super) llm_sessions: Option<String>,
    pub(super) sqlite3: bool,
}

impl Host {
    pub(super) fn probed(&self) -> Result<&Probe> {
        self.probe_once().map_err(|message| self.fail(message))
    }

    pub(super) fn probe_once(&self) -> std::result::Result<&Probe, String> {
        self.probe
            .get_or_init(|| {
                // The probe travels on stdin like the listing script: ssh
                // hands argv to the remote login shell for re-parsing, and
                // this script's quoting would not survive that round trip.
                let raw = self
                    .ssh_script(
                        "echo \"home:$HOME\"\n\
                         { command -v llm-sessions || [ -x \"$HOME/.local/bin/llm-sessions\" ] \\\n\
                           && printf 'llm-sessions:%s\\n' \"$(command -v llm-sessions || echo \"$HOME/.local/bin/llm-sessions\")\"; }\n\
                         command -v sqlite3 >/dev/null && echo sqlite3:yes\n",
                    )
                    .map_err(|e| match e {
                        Error::Remote { message, .. } => message,
                        other => other.to_string(),
                    })?;
                let mut reported = None;
                let mut llm_sessions = None;
                let mut sqlite3 = false;
                for line in raw.lines() {
                    let line = line.trim();
                    if let Some(path) = line.strip_prefix("llm-sessions:")
                        && !path.is_empty()
                    {
                        llm_sessions = Some(path.to_owned());
                    } else if line == "sqlite3:yes" {
                        sqlite3 = true;
                    } else if let Some(home) = line.strip_prefix("home:")
                        && !home.is_empty()
                    {
                        reported = Some(PathBuf::from(home));
                    }
                }
                let home = self
                    .config
                    .home
                    .clone()
                    .or(reported)
                    .ok_or_else(|| "host reported no home directory".to_string())?;
                Ok(Probe {
                    home,
                    llm_sessions,
                    sqlite3,
                })
            })
            .as_ref()
            .map_err(String::clone)
    }

    /// Run a command on the host. Never prompts: a host that needs a
    /// passphrase or an unknown key fails fast and is reported.
    pub(super) fn ssh(&self, argv: &[&str]) -> Result<String> {
        self.run(argv, None)
    }

    /// Run a generated `sh` script, passed on stdin so quoting survives.
    pub(super) fn ssh_script(&self, script: &str) -> Result<String> {
        self.run(&["sh", "-s"], Some(script))
    }

    fn run(&self, argv: &[&str], stdin: Option<&str>) -> Result<String> {
        let mut command = Command::new("ssh");
        command.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ServerAliveInterval=5",
            "-o",
            "ServerAliveCountMax=2",
        ]);
        command.arg(&self.config.ssh);
        command.args(argv);
        command
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .map_err(|e| self.fail(format!("cannot run ssh: {e}")))?;
        if let Some(script) = stdin {
            let mut pipe = child.stdin.take().expect("piped stdin");
            let written = pipe
                .write_all(script.as_bytes())
                .and_then(|()| pipe.flush());
            drop(pipe);
            if let Err(e) = written {
                return Err(self.fail(format!("cannot send the listing script: {e}")));
            }
        }
        let output = child
            .wait_with_output()
            .map_err(|e| self.fail(format!("ssh failed: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let first = stderr.lines().find(|line| !line.trim().is_empty());
            let message: String = first
                .unwrap_or("ssh exited non-zero")
                .chars()
                .take(200)
                .collect();
            return Err(self.fail(message));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// The directory a glob is rooted at: everything up to its first wildcard.
fn literal_prefix(glob: &str) -> &str {
    let cut = glob.find(['*', '?', '[']).unwrap_or(glob.len());
    match glob[..cut].rfind('/') {
        Some(slash) => &glob[..slash],
        None => "",
    }
}

/// `find` the newest transcripts of one source, then emit a bounded head of
/// each. One pipeline, so a host with thousands of files still costs one
/// round trip and at most [`REMOTE_FILES_PER_SOURCE`] head reads.
pub(super) fn files_script(home: &str, agent: &str, globs: &[&str]) -> String {
    let mut finds = String::new();
    for glob in globs {
        let root = format!("{home}/{}", literal_prefix(glob));
        // Writing into a `String` cannot fail.
        let _ = write!(
            finds,
            "find {} -path {} -type f -printf 'F\\t{agent}\\t%p\\t%T@\\t%s\\n' 2>/dev/null; ",
            crate::runtime::resume::shell_quote(root.trim_end_matches('/')),
            crate::runtime::resume::shell_quote(&format!("{home}/{glob}")),
        );
    }
    format!(
        "{{ {finds}}} | sort -t\"$(printf '\\t')\" -k4 -rn | head -n {REMOTE_FILES_PER_SOURCE} | \
         while IFS=\"$(printf '\\t')\" read -r tag src path stamp size; do \
         printf '%s\\t%s\\t%s\\t%s\\t%s\\n' \"$tag\" \"$src\" \"$path\" \"$stamp\" \"$size\"; \
         printf 'H\\t%s\\t' \"$path\"; \
         head -c {REMOTE_HEAD_BYTES} \"$path\" | base64 | tr -d '\\n'; printf '\\n'; done\n"
    )
}

/// One read-only listing of a remote store, base64'd so a newline in a
/// title cannot break the record framing. `queries` run in order until one
/// succeeds. Buffer each attempt separately: even if a query emits some rows
/// and then fails, its partial JSON must not contaminate the next result.
pub(super) fn sql_script(home: &str, agent: &str, db: &str, queries: &[&str]) -> String {
    let db = crate::runtime::resume::shell_quote(&format!("{home}/{db}"));
    let attempts = queries
        .iter()
        .map(|query| {
            format!(
                "payload=$(sqlite3 -readonly -json {db} {} 2>/dev/null)",
                crate::runtime::resume::shell_quote(query)
            )
        })
        .collect::<Vec<_>>()
        .join(" || ");
    format!(
        "payload=''; if ! {{ {attempts}; }}; then payload=''; fi; printf 'S\\t{agent}\\t'; printf '%s' \"$payload\" | base64 | tr -d '\\n'; printf '\\n'\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_glob_is_rooted_at_its_literal_prefix() {
        assert_eq!(
            literal_prefix(".omp/agent/sessions/*/*.jsonl"),
            ".omp/agent/sessions"
        );
        assert_eq!(
            literal_prefix(".codex/sessions/rollout-*.jsonl"),
            ".codex/sessions"
        );
        assert_eq!(literal_prefix("*.jsonl"), "");
    }

    #[test]
    fn failed_sql_attempt_does_not_mix_partial_json_with_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let fake_sqlite = dir.path().join("sqlite3");
        std::fs::write(
            &fake_sqlite,
            "#!/bin/sh\ncase \"$4\" in *first*) printf '[{partial'; exit 1;; esac\nprintf '[{\"id\":\"second\"}]'\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_sqlite, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = sql_script("/home/me", "opencode", "store.db", &["first", "second"]);
        let output = Command::new("sh")
            .arg("-c")
            .arg(script)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    dir.path().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .output()
            .unwrap();
        assert!(output.status.success());
        let line = String::from_utf8(output.stdout).unwrap();
        let encoded = line.trim_end().split('\t').nth(2).unwrap();
        assert_eq!(
            crate::source::remote::parse::decode_base64(encoded).unwrap(),
            br#"[{"id":"second"}]"#
        );

        let script = sql_script("/home/me", "opencode", "store.db", &["first"]);
        let output = Command::new("sh")
            .arg("-c")
            .arg(script)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    dir.path().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "S\topencode\t\n");
    }
}
