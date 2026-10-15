//! How a remote host is reached: the `ssh` invocation itself, the one-time
//! probe for a remote `llm-sessions` and `sqlite3`, and the `sh` scripts the
//! shell transport sends.

use std::fmt::Write as _;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::error::{Error, Result};
use crate::federation::Capabilities;
use crate::model::normalize_origin;
use crate::source::remote::{Host, Transport};

/// Newest transcripts considered per source per host. A fleet machine holds
/// thousands; the interesting ones are the recent ones, and every extra path
/// costs a bounded `head` in the same round trip.
const REMOTE_FILES_PER_SOURCE: usize = 200;

/// Bytes read from each remote transcript, matching OMP's own head read: the
/// header record plus its optional title slot.
const REMOTE_HEAD_BYTES: usize = 8 * 1024;
/// Run over SSH on stdin, where a non-login shell may have a restricted PATH.
const PROBE_SCRIPT: &str = r#"echo "home:$HOME"
marker="${XDG_STATE_HOME:-$HOME/.local/state}/llm-sessions/origin-id"
if [ -r "$marker" ]; then
    IFS= read -r origin < "$marker"
    printf 'origin:%s\n' "$origin"
fi
if path=$(command -v llm-sessions); then
    printf 'llm-sessions:%s\n' "$path"
elif [ -x "$HOME/.local/bin/llm-sessions" ]; then
    printf 'llm-sessions:%s\n' "$HOME/.local/bin/llm-sessions"
elif [ -x "$HOME/.cargo/bin/llm-sessions" ]; then
    printf 'llm-sessions:%s\n' "$HOME/.cargo/bin/llm-sessions"
fi
command -v sqlite3 >/dev/null && echo sqlite3:yes
# A missing sqlite3 must not fail the probe.
exit 0
"#;

/// What probing a host learned, once per process.
#[derive(Clone, Debug)]
pub(super) struct Probe {
    pub(super) home: PathBuf,
    /// sshd's default PATH may omit user-writable directories, including
    /// `~/.local/bin` and `~/.cargo/bin`.
    pub(super) llm_sessions: Option<String>,
    pub(super) sqlite3: bool,
    pub(super) origin_id: String,
    /// The federation protocol version the peer's binary speaks: `0` when
    /// it answers no capabilities handshake, `1` for the legacy SSH-only
    /// contract, and [`crate::federation::PROTOCOL_VERSION`] when it can
    /// take this observer's mixed ssh/http routes.
    pub(super) federation: u32,
}

/// SSH concatenates its command arguments for the remote login shell.
/// Quote every word once, then pass the entire command as one argument.
pub(super) fn shell_command<'a>(argv: impl IntoIterator<Item = &'a str>) -> String {
    argv.into_iter()
        .map(crate::runtime::resume::shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn home_environment(home: &std::path::Path) -> [String; 5] {
    [
        format!("HOME={}", home.display()),
        format!("XDG_CONFIG_HOME={}", home.join(".config").display()),
        format!("XDG_STATE_HOME={}", home.join(".local/state").display()),
        format!("XDG_CACHE_HOME={}", home.join(".cache").display()),
        format!("XDG_DATA_HOME={}", home.join(".local/share").display()),
    ]
}

#[cfg(test)]
#[derive(Default)]
pub(super) struct Scripted {
    pub(super) replies:
        std::sync::Mutex<std::collections::VecDeque<std::result::Result<String, String>>>,
    pub(super) calls: std::sync::Mutex<Vec<Vec<String>>>,
    pub(super) scripts: std::sync::Mutex<Vec<String>>,
}

impl Host {
    fn probe_script(&self) -> std::borrow::Cow<'static, str> {
        if let Some(home) = self.configured_home() {
            let environment = home_environment(home);
            let assignments = shell_command(environment.iter().map(String::as_str));
            std::borrow::Cow::Owned(format!("export {assignments}\n{PROBE_SCRIPT}"))
        } else {
            std::borrow::Cow::Borrowed(PROBE_SCRIPT)
        }
    }

    /// A configured home selects an account/session namespace consistently for
    /// marker probing, its config, and every guarded binary invocation.
    pub(super) fn ssh_binary(&self, home: &std::path::Path, args: &[&str]) -> Result<String> {
        if self.configured_home().is_some() {
            let environment = home_environment(home);
            let mut command = Vec::with_capacity(args.len() + environment.len() + 1);
            command.push("env");
            command.extend(environment.iter().map(String::as_str));
            command.extend_from_slice(args);
            self.ssh(&command)
        } else {
            self.ssh(args)
        }
    }

    pub(super) fn probed(&self) -> Result<&Probe> {
        let probe = self.probe_once().map_err(|message| self.fail(message))?;
        self.check_origin(&probe.origin_id)?;
        Ok(probe)
    }

    pub(super) fn probe_once(&self) -> std::result::Result<&Probe, String> {
        self.probe
            .get_or_init(|| {
                // The probe travels on stdin like the listing script: ssh
                // hands argv to the remote login shell for re-parsing, and
                // this script's quoting would not survive that round trip.
                let raw = self.ssh_script(&self.probe_script()).map_err(|e| match e {
                    Error::Remote { message, .. } => message,
                    other => other.to_string(),
                })?;
                let mut reported = None;
                let mut llm_sessions = None;
                let mut sqlite3 = false;
                let mut origin_id = None;
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
                    } else if let Some(origin) = line.strip_prefix("origin:")
                        && crate::federation::valid_origin(origin)
                    {
                        origin_id = Some(normalize_origin(origin));
                    }
                }
                let home = self
                    .configured_home()
                    .map(std::path::Path::to_path_buf)
                    .or(reported)
                    .ok_or_else(|| "host reported no home directory".to_string())?;
                let origin_id = origin_id.ok_or_else(|| {
                    "canonical origin identity unavailable: missing or malformed origin-id marker under $XDG_STATE_HOME (default ~/.local/state)/llm-sessions".to_string()
                })?;
                let federation = if let Some(binary) = llm_sessions.as_deref() {
                    match self.ssh_binary(&home, &[binary, "federation", "capabilities"]) {
                        Ok(raw) => {
                            if let Ok(capability) = serde_json::from_str::<Capabilities>(&raw) {
                                if capability.origin_id != origin_id {
                                    return Err("federation capability origin does not match origin marker".to_string());
                                }
                                capability.version
                            } else {
                                0
                            }
                        }
                        Err(_) => 0,
                    }
                } else {
                    0
                };
                Ok(Probe {
                    home,
                    llm_sessions,
                    sqlite3,
                    origin_id,
                    federation,
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
        #[cfg(test)]
        if let Some(scripted) = &self.scripted {
            scripted
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(argv.iter().map(|part| (*part).to_owned()).collect());
            if let Some(script) = stdin {
                scripted
                    .scripts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(script.to_owned());
            }
            return scripted
                .replies
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .expect("unexpected remote command")
                .map_err(|message| self.fail(message));
        }
        let Transport::Ssh { ssh, .. } = &self.config.transport else {
            return Err(self.fail("an HTTP peer is not an ssh destination"));
        };
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
        command.arg("--").arg(ssh);
        command.arg(shell_command(argv.iter().copied()));
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
        // A command that dies before reading its script — an unreachable
        // host, a refused connection — closes the pipe, so feeding it fails
        // with a broken pipe. That is not the diagnosis: the command's own
        // status and stderr are, and they are collected below regardless,
        // so a feeding error never replaces what the command actually said.
        if let Some(script) = stdin {
            let mut pipe = child.stdin.take().expect("piped stdin");
            let _ = pipe
                .write_all(script.as_bytes())
                .and_then(|()| pipe.flush());
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
///
/// A host without the database emits nothing: no data is not a problem.
pub(super) fn sql_script(home: &str, agent: &str, db: &str, queries: &[&str]) -> String {
    let db = crate::runtime::resume::shell_quote(&format!("{home}/{db}"));
    let attempts = queries
        .iter()
        .map(|query| {
            format!(
                "payload=$({SQLITE3} {db} {} 2>/dev/null)",
                crate::runtime::resume::shell_quote(query)
            )
        })
        .collect::<Vec<_>>()
        .join(" || ");
    format!(
        "if [ -e {db} ]; then payload=''; if ! {{ {attempts}; }}; then payload=''; fi; printf 'S\\t{agent}\\t'; printf '%s' \"$payload\" | base64 | tr -d '\\n'; printf '\\n'; fi\n"
    )
}

/// `sqlite3` read-only, as JSON, waiting out a writer's lock briefly rather
/// than failing a scan on a live WAL database.
const SQLITE3: &str = "sqlite3 -readonly -json -cmd '.timeout 2000'";

/// For a source whose store needs `sqlite3` the host lacks: say so only when
/// the store exists. A host the agent never ran on stays silent.
pub(super) fn missing_sqlite3_script(home: &str, agent: &str, db: &str) -> String {
    let db = crate::runtime::resume::shell_quote(&format!("{home}/{db}"));
    format!("if [ -e {db} ]; then printf 'M\\t{agent}\\n'; fi\n")
}

/// Read one session's rows on the host: each named SELECT becomes one
/// `R\t<name>\t<base64 json>` record, so only that session crosses the
/// wire, never the database. A missing database emits nothing.
pub(super) fn open_sql_script(db: &str, queries: &[(&str, String)]) -> String {
    let db = crate::runtime::resume::shell_quote(db);
    let mut script = format!("if [ -e {db} ]; then\n");
    for (name, query) in queries {
        // Writing into a `String` cannot fail.
        let _ = writeln!(
            script,
            "payload=$({SQLITE3} {db} {} 2>/dev/null) || payload=''; printf 'R\\t{name}\\t'; printf '%s' \"$payload\" | base64 | tr -d '\\n'; printf '\\n'",
            crate::runtime::resume::shell_quote(query)
        );
    }
    script.push_str("fi\n");
    script
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn configured_home_probe_reads_its_marker_not_the_login_account_marker() {
        let login = tempfile::tempdir().unwrap();
        let configured = login.path().join("another namespace with ' spaces");
        let login_origin = crate::federation::ensure_origin(login.path()).unwrap();
        let configured_origin = crate::federation::ensure_origin(&configured).unwrap();
        let host = Host::new(crate::source::remote::HostConfig {
            name: "namespace".into(),
            origin_id: Some(configured_origin.clone()),
            transport: crate::source::remote::Transport::Ssh {
                ssh: "namespace".into(),
                home: Some(configured.clone()),
            },
            sources: Vec::new(),
            mode: crate::source::remote::Mode::Auto,
        });
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(host.probe_script().as_ref())
            .env("HOME", login.path())
            .env("PATH", login.path())
            .env("XDG_CONFIG_HOME", "/wrong/config")
            .env("XDG_STATE_HOME", "/wrong/state")
            .env("XDG_CACHE_HOME", "/wrong/cache")
            .env("XDG_DATA_HOME", "/wrong/data")
            .output()
            .unwrap();
        assert!(output.status.success());
        let raw = String::from_utf8(output.stdout).unwrap();
        assert!(raw.contains(&format!("origin:{configured_origin}")));
        assert!(!raw.contains(&format!("origin:{login_origin}")));
        assert!(
            raw.lines()
                .any(|line| line == format!("home:{}", configured.display()))
        );
    }

    #[test]
    fn shell_command_preserves_json_paths_and_hostile_ids() {
        let words = [
            "/home/o'neil/bin/llm-sessions",
            "federation",
            "show",
            "--id",
            "spaces ' $(false); echo wrong",
            "--route",
            r#"[{"origin_id":"id","name":"a b","ssh":"u@a"}]"#,
            "",
        ];
        let line = shell_command(words);
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("set -- {line}; printf '%s\\0' \"$@\""))
            .output()
            .unwrap();
        assert!(output.status.success());
        let actual: Vec<_> = output
            .stdout
            .split(|byte| *byte == 0)
            .take(words.len())
            .map(|word| std::str::from_utf8(word).unwrap())
            .collect();
        assert_eq!(actual, words);
    }

    #[test]
    fn shell_probe_reads_the_same_provisioned_marker_without_a_binary() {
        let dir = tempfile::tempdir().unwrap();
        let origin = crate::federation::ensure_origin(dir.path()).unwrap();
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(PROBE_SCRIPT)
            .env("HOME", dir.path())
            .env("PATH", dir.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        let raw = String::from_utf8(output.stdout).unwrap();
        assert!(raw.lines().any(|line| line == format!("origin:{origin}")));
        assert!(!raw.contains("llm-sessions:"));
        assert_eq!(crate::federation::read_origin(dir.path()).unwrap(), origin);
    }

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
    fn probe_finds_cargo_installed_binary_outside_ssh_path() {
        let dir = tempfile::tempdir().unwrap();
        let cargo_bin = dir.path().join(".cargo/bin");
        std::fs::create_dir_all(&cargo_bin).unwrap();
        let binary = cargo_bin.join("llm-sessions");
        std::fs::write(&binary, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(dir.path().join("empty-path")).unwrap();

        let mut child = Command::new("/bin/sh")
            .arg("-s")
            .env("HOME", dir.path())
            // An llm-sessions on this workstation's PATH (including /usr/bin
            // and /bin) would satisfy the first branch; the fixture is about
            // the cargo fallback, so no directory may resolve it.
            .env("PATH", dir.path().join("empty-path"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(PROBE_SCRIPT.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .any(|line| { line == format!("llm-sessions:{}", binary.display()) })
        );
    }

    #[test]
    fn probe_script_succeeds_without_sqlite3() {
        // A host without sqlite3 used to fail the whole probe: the script's
        // last command short-circuited to a non-zero exit, which ssh
        // propagates, and stderr is empty so the error read as
        // "ssh exited non-zero".
        let dir = tempfile::tempdir().unwrap(); // empty PATH: no sqlite3

        let mut child = Command::new("/bin/sh")
            .arg("-s")
            .env("HOME", dir.path())
            .env("PATH", dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(PROBE_SCRIPT.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
    }

    #[test]
    fn failed_sql_attempt_does_not_mix_partial_json_with_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let fake_sqlite = dir.path().join("sqlite3");
        std::fs::write(
            &fake_sqlite,
            "#!/bin/sh\ncase \"$*\" in *first*) printf '[{partial'; exit 1;; esac\nprintf '[{\"id\":\"second\"}]'\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_sqlite, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("store.db"), "").unwrap();
        let home = dir.path().to_string_lossy().into_owned();
        let script = sql_script(&home, "opencode", "store.db", &["first", "second"]);
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

        let script = sql_script(&home, "opencode", "store.db", &["first"]);
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

    fn run_sh(script: &str, path: Option<&str>) -> String {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        if let Some(path) = path {
            command.env("PATH", path);
        }
        let output = command.output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn an_absent_store_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_string_lossy();
        let missing = missing_sqlite3_script(&home, "copilot", ".copilot/session-store.db");
        assert_eq!(run_sh(&missing, None), "");
        let listing = sql_script(&home, "copilot", ".copilot/session-store.db", &["SELECT 1"]);
        assert_eq!(run_sh(&listing, None), "");

        std::fs::create_dir_all(dir.path().join(".copilot")).unwrap();
        std::fs::write(dir.path().join(".copilot/session-store.db"), "").unwrap();
        assert_eq!(run_sh(&missing, None), "M\tcopilot\n");
    }

    #[test]
    fn open_sql_script_quotes_path_and_query() {
        let script = open_sql_script(
            "/home/o'neil/.copilot/session-store.db",
            &[("session", "SELECT 1 WHERE id = 'x'".into())],
        );
        assert!(
            script.contains("'/home/o'\\''neil/.copilot/session-store.db'"),
            "{script}"
        );
        assert!(script.contains("id = '\\''x'\\''"), "{script}");
        assert!(script.contains("-readonly -json"), "{script}");
        assert!(script.contains("printf 'R\\tsession\\t'"), "{script}");
    }

    #[test]
    fn open_sql_script_frames_each_query() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("store.db");
        std::fs::write(&db, "").unwrap();
        let fake_sqlite = dir.path().join("sqlite3");
        // The query is the last argument; echo it back so quoting is proven.
        std::fs::write(
            &fake_sqlite,
            "#!/bin/sh\nfor last; do :; done\ncase \"$last\" in *fail*) exit 1;; esac\nprintf '[{\"q\":\"%s\"}]' \"$last\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_sqlite, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = open_sql_script(
            &db.to_string_lossy(),
            &[
                ("session", "SELECT 'it''s'".into()),
                ("turns", "fail".into()),
            ],
        );
        let path = format!(
            "{}:{}",
            dir.path().display(),
            std::env::var("PATH").unwrap()
        );
        let raw = run_sh(&script, Some(&path));
        let rows = crate::source::remote::parse::parse_open_rows(&raw);
        assert_eq!(
            rows["session"],
            vec![serde_json::json!({"q": "SELECT 'it''s'"})]
        );
        assert!(rows["turns"].is_empty(), "a failed query is no rows");
    }
}
