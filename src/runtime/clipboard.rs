//! Copy text to the system clipboard through whichever helper is installed.

use std::io::{ErrorKind, Seek, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::Result;
use crate::error::Error;

/// Clipboard helpers in preference order: the display server's own tool
/// first, then the platform ones. Each reads the text from stdin.
const HELPERS: &[(&str, &[&str])] = &[
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
    ("pbcopy", &[]),
    ("clip.exe", &[]),
];

pub type CopyResult = Result<&'static str>;
pub type PendingCopy = mpsc::Receiver<CopyResult>;

/// Run helper discovery and copying off the UI thread.
pub fn start(text: String) -> Result<PendingCopy> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("clipboard-copy".into())
        .spawn(move || {
            let _ = sender.send(copy(&text));
        })
        .map_err(|error| Error::Clipboard {
            message: error.to_string(),
        })?;
    Ok(receiver)
}

/// Hand `text` to the first helper that runs for this session; returns its name.
///
/// Wayland helpers are skipped without `WAYLAND_DISPLAY` and X11 ones without
/// `DISPLAY`: they would start, fail to connect, and mask a working fallback.
pub fn copy(text: &str) -> Result<&'static str> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    let mut failures = Vec::new();
    for &(program, args) in HELPERS {
        match program {
            "wl-copy" if !wayland => continue,
            "xclip" | "xsel" if !x11 => continue,
            _ => {}
        }
        match run(program, args, text) {
            Ok(()) => return Ok(program),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => failures.push(format!("{program}: {e}")),
        }
    }
    Err(Error::Clipboard {
        message: if failures.is_empty() {
            "no clipboard helper found (wl-copy, xclip, xsel, pbcopy, clip.exe)".into()
        } else {
            failures.join("; ")
        },
    })
}

fn run(program: &str, args: &[&str], text: &str) -> std::io::Result<()> {
    run_with_timeout(program, args, text, Duration::from_secs(2))
}

fn run_with_timeout(
    program: &str,
    args: &[&str],
    text: &str,
    timeout: Duration,
) -> std::io::Result<()> {
    wait_helper(spawn_helper(program, args, text)?, timeout)
}

fn spawn_helper(program: &str, args: &[&str], text: &str) -> std::io::Result<Child> {
    // A secure unnamed temporary file avoids pipe backpressure entirely.
    // A descendant retaining stdin cannot strand a writer or block retries.
    // tempfile removes the file automatically; never use a named transcript.
    let mut input = tempfile::tempfile()?;
    input.write_all(text.as_bytes())?;
    input.rewind()?;
    Command::new(program)
        .args(args)
        .stdin(Stdio::from(input))
        // xclip and wl-copy fork a daemon that keeps serving the selection;
        // pipes it inherits would never reach EOF.
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

fn wait_helper(mut child: Child, timeout: Duration) -> std::io::Result<()> {
    let deadline = Instant::now() + timeout;
    let outcome = (|| {
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    ErrorKind::TimedOut,
                    "clipboard helper timed out",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    })();
    // Always reap the helper on failure; no detached writer needs joining.
    if outcome.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let status = outcome?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("exited with {status}")))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct HeldHelper {
        child: Option<Child>,
        release: std::path::PathBuf,
    }

    impl Drop for HeldHelper {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.release, "release");
            if let Some(child) = &mut self.child {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[test]
    fn helper_receives_text_and_eof() {
        run("sh", &["-c", "test \"$(cat)\" = 'hello'"], "hello").unwrap();
    }

    #[test]
    fn early_exit_with_large_input_is_reported() {
        assert!(run("sh", &["-c", "exit 1"], &"x".repeat(1_000_000)).is_err());
    }

    #[test]
    fn deadline_covers_helpers_that_do_not_read_or_exit() {
        for text in [String::new(), "x".repeat(1_000_000)] {
            let started = Instant::now();
            let error = run_with_timeout(
                "sh",
                &["-c", "while :; do :; done"],
                &text,
                Duration::from_millis(100),
            )
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::TimedOut);
            assert!(started.elapsed() < Duration::from_secs(2));
        }
    }

    #[test]
    fn inherited_stdin_does_not_block_retry() {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let release = dir.path().join("release");
        // Explicit release, not a sleep-based lifetime; deleting the directory
        // is a second escape hatch if the assertion unwinds during cleanup.
        let script = "sh -c 'touch \"$1\"; while [ -d \"$3\" ] && [ ! -e \"$2\" ]; do sleep 0.01; done' held \"$1\" \"$2\" \"$3\" <&0 & wait";
        let child = spawn_helper(
            "sh",
            &[
                "-c",
                script,
                "helper",
                ready.to_str().unwrap(),
                release.to_str().unwrap(),
                dir.path().to_str().unwrap(),
            ],
            &"x".repeat(1_000_000),
        )
        .unwrap();
        let mut held = HeldHelper {
            child: Some(child),
            release,
        };
        let startup_deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                Instant::now() < startup_deadline,
                "descendant did not start"
            );
            thread::sleep(Duration::from_millis(10));
        }
        // Start the timeout only after the descendant confirms it holds stdin.
        let error =
            wait_helper(held.child.take().unwrap(), Duration::from_millis(100)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        assert!(
            ready.exists(),
            "descendant started and holds stdin until explicitly released"
        );
        run("sh", &["-c", "cat >/dev/null"], "retry").unwrap();
    }
}
