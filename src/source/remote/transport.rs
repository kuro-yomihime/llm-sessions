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

/// Run over SSH on stdin, where a non-login shell may have a restricted PATH.
/// Pure POSIX `sh`, so a peer needs no `llm-sessions` binary of any version:
/// keep a valid existing identity, generate one when the marker is missing
/// (or malformed, which locks the peer out just the same), and replace even a
/// valid one only when `force` was asked for. Creation never clobbers — a
/// concurrent initializer (the peer's own newer binary, say) may win, and
/// whoever's file persisted is authoritative. Repair never clobbers either:
/// runs publish one at a time under `origin-id.lock`, the malformed marker is
/// renamed aside, and if what moved turns out to be a valid identity
/// published since the last read, it is put back.
const INIT_ORIGIN_SCRIPT: &str = r#"state="${XDG_STATE_HOME:-$HOME/.local/state}"
dir="$state/llm-sessions"
marker="$dir/origin-id"
read_origin() {
    origin=$(cat "${1:-$marker}" 2>/dev/null) || origin=
    # Trim surrounding whitespace only; embedded whitespace remains invalid.
    origin=${origin#"${origin%%[![:space:]]*}"}
    origin=${origin%"${origin##*[![:space:]]}"}
}
read_origin
valid() {
    case "$1" in
        ????????-????-????-????-????????????) ;;
        *) return 1 ;;
    esac
    hex=$(printf '%s' "$1" | tr -d '-' | tr 'ABCDEF' 'abcdef')
    [ ${#hex} -eq 32 ] || return 1
    case "$hex" in *[!0-9a-f]*) return 1 ;; esac
}
if valid "$origin" && [ "$force" != yes ]; then
    printf 'origin:%s\n' "$(printf '%s' "$origin" | tr 'ABCDEF' 'abcdef')"
    exit 0
fi
hex=$(head -c 16 /dev/urandom 2>/dev/null | od -An -tx1 | tr -d ' \n\t\r')
if [ ${#hex} -ne 32 ]; then
    echo "origin init: cannot read 16 bytes from /dev/urandom" >&2
    exit 1
fi
a=$(printf '%s' "$hex" | cut -c1-8)
b=$(printf '%s' "$hex" | cut -c9-12)
c=4$(printf '%s' "$hex" | cut -c14-16)
d=$(printf '%s' "$hex" | cut -c17-20)
e=$(printf '%s' "$hex" | cut -c21-32)
# Version 4 and RFC 4122 variant, without shell arithmetic on hex digits:
# the variant nibble keeps its low two bits, so 0,4,8,c all map to 8.
case $(printf '%s' "$d" | cut -c1) in
    0|4|8|c) d=8$(printf '%s' "$d" | cut -c2-4) ;;
    1|5|9|d) d=9$(printf '%s' "$d" | cut -c2-4) ;;
    2|6|a|e) d=a$(printf '%s' "$d" | cut -c2-4) ;;
    *) d=b$(printf '%s' "$d" | cut -c2-4) ;;
esac
uuid="$a-$b-$c-$d-$e"
mkdir -p "$dir" || { echo "origin init: cannot create $dir" >&2; exit 1; }
# Publish complete files only. A marker created after our initial read wins.
tmp=$(mktemp "$marker.init.XXXXXX") || exit 1
aside="$tmp.old"
lock="$marker.lock"
locked=
trap 'rm -f "$tmp" "$aside"; [ -z "$locked" ] || rmdir "$lock"' 0
trap 'exit 1' 1 2 13 15
if ! printf '%s\n' "$uuid" > "$tmp"; then
    echo "origin init: cannot write $tmp" >&2
    exit 1
fi
# Runs of this script publish one at a time, so none can move aside a
# marker another just published. The peer's own binary only ever creates a
# missing marker without clobbering, which needs no turn.
tries=0
until mkdir "$lock" 2>/dev/null; do
    tries=$((tries + 1))
    if [ "$tries" -ge 100 ]; then
        echo "origin init: $lock is held by another initializer; remove it if none is running" >&2
        exit 1
    fi
    sleep 0.1 2>/dev/null || sleep 1
done
locked=yes
read_origin
if valid "$origin" && [ "$force" != yes ]; then
    :
elif [ "$force" = yes ]; then
    mv -f "$tmp" "$marker" || exit 1
else
    # Rename is atomic, so whatever is moved is exactly what gets judged.
    if mv "$marker" "$aside" 2>/dev/null; then
        read_origin "$aside"
        if valid "$origin"; then
            ln "$aside" "$marker" 2>/dev/null || :
        fi
    fi
    ln "$tmp" "$marker" 2>/dev/null || :
fi
read_origin
rmdir "$lock"
locked=
if ! valid "$origin"; then
    echo "origin init: $marker does not hold a UUID" >&2
    exit 1
fi
printf 'origin:%s\n' "$(printf '%s' "$origin" | tr 'ABCDEF' 'abcdef')"
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

    /// The origin-init script, with `force` bound and a configured home's
    /// namespace exported ahead of it, exactly as the probe script does.
    fn init_script(&self, force: bool) -> String {
        let script = format!(
            "force={}\n{INIT_ORIGIN_SCRIPT}",
            if force { "yes" } else { "no" }
        );
        match self.configured_home() {
            Some(home) => {
                let environment = home_environment(home);
                let assignments = shell_command(environment.iter().map(String::as_str));
                format!("export {assignments}\n{script}")
            }
            None => script,
        }
    }

    /// Create or replace the peer's origin-id marker over the connection and
    /// return the identity that resulted. The reply is parsed like the
    /// probe's, so a login profile's noise cannot masquerade as an origin.
    pub(super) fn ssh_init_origin(&self, force: bool) -> Result<String> {
        let raw = self.ssh_script(&self.init_script(force))?;
        raw.lines()
            .map(str::trim)
            .find_map(|line| {
                let origin = line.strip_prefix("origin:")?;
                crate::federation::valid_origin(origin).then(|| normalize_origin(origin))
            })
            .ok_or_else(|| self.fail("unreadable origin init output: no origin line"))
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
                    format!(
                        "canonical origin identity unavailable: missing or malformed origin-id marker under $XDG_STATE_HOME (default ~/.local/state)/llm-sessions; run `llm-sessions hosts refresh --init {}` to generate one on the peer",
                        self.name()
                    )
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

    fn ssh_host(name: &str, home: Option<PathBuf>) -> Host {
        Host::new(crate::source::remote::HostConfig {
            name: name.into(),
            origin_id: None,
            transport: Transport::Ssh {
                ssh: name.into(),
                home,
            },
            sources: Vec::new(),
            mode: crate::source::remote::Mode::Auto,
        })
    }

    fn run_init(host: &Host, force: bool, home: &std::path::Path) -> String {
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(host.init_script(force))
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin")
            .env_remove("XDG_STATE_HOME")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let raw = String::from_utf8(output.stdout).unwrap();
        raw.lines()
            .find_map(|line| line.strip_prefix("origin:"))
            .unwrap_or_else(|| panic!("no origin line in {raw}"))
            .to_owned()
    }

    #[test]
    fn init_preserves_an_identity_created_after_its_initial_read() {
        let peer = tempfile::tempdir().unwrap();
        let host = ssh_host("worker", None);
        let winner = "00000000-0000-4000-8000-0000000000aa";
        // Pause at directory creation by publishing the competing winner there.
        let script = format!(
            "mkdir() {{ command mkdir \"$@\" || return; if [ \"$1\" = -p ]; then printf '%s\\n' '{winner}' > \"$2/origin-id\"; fi; }}\n{}",
            host.init_script(false)
        );
        let output = Command::new("/bin/sh")
            .args(["-c", &script])
            .env("HOME", peer.path())
            .env_remove("XDG_STATE_HOME")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(crate::federation::read_origin(peer.path()).unwrap(), winner);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            format!("origin:{winner}")
        );
    }

    #[test]
    fn repair_preserves_an_identity_published_after_its_final_read() {
        let peer = tempfile::tempdir().unwrap();
        let host = ssh_host("worker", None);
        let marker = peer.path().join(".local/state/llm-sessions/origin-id");
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, "not a uuid\n").unwrap();
        let winner = "00000000-0000-4000-8000-0000000000aa";
        // The first `mv` is the repair's publication step, after every read:
        // a concurrent initializer's valid marker lands just before it.
        let script = format!(
            "mv() {{ [ -n \"$raced\" ] || {{ raced=1; printf '%s\\n' '{winner}' > \"$marker\"; }}; command mv \"$@\"; }}\n{}",
            host.init_script(false)
        );
        let output = Command::new("/bin/sh")
            .args(["-c", &script])
            .env("HOME", peer.path())
            .env_remove("XDG_STATE_HOME")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(crate::federation::read_origin(peer.path()).unwrap(), winner);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            format!("origin:{winner}")
        );
        let leftovers: Vec<_> = std::fs::read_dir(marker.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != "origin-id")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn concurrent_repairs_agree_on_one_identity() {
        let host = ssh_host("worker", None);
        let script = host.init_script(false);
        for _ in 0..5 {
            let peer = tempfile::tempdir().unwrap();
            let marker = peer.path().join(".local/state/llm-sessions/origin-id");
            std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
            std::fs::write(&marker, "not a uuid\n").unwrap();
            let children: Vec<_> = (0..8)
                .map(|_| {
                    Command::new("/bin/sh")
                        .args(["-c", &script])
                        .env("HOME", peer.path())
                        .env("PATH", "/usr/bin:/bin")
                        .env_remove("XDG_STATE_HOME")
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .unwrap()
                })
                .collect();
            let reported: Vec<_> = children
                .into_iter()
                .map(|child| child.wait_with_output().unwrap())
                .collect();
            let persisted = crate::federation::read_origin(peer.path()).unwrap();
            for output in reported {
                assert!(output.status.success(), "{output:?}");
                assert_eq!(
                    String::from_utf8(output.stdout).unwrap().trim(),
                    format!("origin:{persisted}"),
                    "every run reports the identity that persisted"
                );
            }
        }
    }

    #[test]
    fn init_generates_a_v4_marker_keeps_it_and_replaces_it_only_by_force() {
        let peer = tempfile::tempdir().unwrap();
        let host = ssh_host("worker", None);
        let marker = peer.path().join(".local/state/llm-sessions/origin-id");

        let generated = run_init(&host, false, peer.path());
        assert!(crate::federation::valid_origin(&generated));
        assert_eq!(generated.as_bytes()[14], b'4', "UUID version nibble");
        assert!(matches!(generated.as_bytes()[19], b'8'..=b'b'), "variant");
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), generated);
        let mode = std::fs::metadata(&marker).unwrap().permissions();
        assert_eq!(mode.mode() & 0o777, 0o600);

        // A valid identity is stable: reruns report it, never roll it.
        assert_eq!(run_init(&host, false, peer.path()), generated);
        for padded in [format!(" {generated} \n"), format!("{generated}\r\n")] {
            std::fs::write(&marker, padded).unwrap();
            assert_eq!(run_init(&host, false, peer.path()), generated);
            assert_eq!(
                crate::federation::read_origin(peer.path()).unwrap(),
                generated
            );
        }
        std::fs::write(&marker, format!("{generated}\ntrailing garbage\n")).unwrap();
        let recovered = run_init(&host, false, peer.path());
        assert_eq!(
            crate::federation::read_origin(peer.path()).unwrap(),
            recovered
        );
        assert_ne!(recovered, generated);

        // A malformed marker locks a peer out; init regenerates unprompted.
        std::fs::write(&marker, "not a uuid\n").unwrap();
        let repaired = run_init(&host, false, peer.path());
        assert!(crate::federation::valid_origin(&repaired));
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), repaired);

        // Force deliberately rekeys; the new identity lands in the marker.
        let forced = run_init(&host, true, peer.path());
        assert!(crate::federation::valid_origin(&forced));
        assert_ne!(forced, repaired);
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), forced);
        // And force is not sticky: the rekeyed identity is kept from now on.
        assert_eq!(run_init(&host, false, peer.path()), forced);
    }

    #[test]
    fn init_writes_the_configured_home_namespace_not_the_login_one() {
        let login = tempfile::tempdir().unwrap();
        let configured = login.path().join("another namespace with ' spaces");
        let host = ssh_host("namespace", Some(configured.clone()));
        let origin = run_init(&host, false, login.path());
        assert!(crate::federation::valid_origin(&origin));
        assert_eq!(
            std::fs::read_to_string(configured.join(".local/state/llm-sessions/origin-id"))
                .unwrap()
                .trim(),
            origin
        );
        assert!(
            !login
                .path()
                .join(".local/state/llm-sessions/origin-id")
                .exists()
        );
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
