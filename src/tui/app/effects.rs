//! The parts of [`App`] that reach outside its own state: writing archive
//! flags through a source or the overlay, building the argv a resume hands
//! to the shell, handing an insert-mode message to an agent's CLI, moving
//! tmux focus, and reading a transcript body through its source or host.

use std::time::Duration;

use crate::model::{ArchiveOutcome, Liveness, SessionSummary, SourceId};
use crate::output::reader::plural;
use crate::runtime::{insert, tmux};
use crate::tui::app::{App, Prompt, fingerprint};

impl App {
    pub(super) fn apply_archive(&mut self, archive: bool) {
        let targets = self.targets();
        let mut native = 0usize;
        let mut overlaid = 0usize;
        let mut errors = Vec::new();

        for (source_id, id) in &targets {
            let Some(position) = self
                .sessions
                .iter()
                .position(|s| s.source == *source_id && s.id == *id)
            else {
                continue;
            };
            let locator = self.sessions[position].locator.clone();
            let outcome = crate::source::find(&self.sources, *source_id)
                .map_or(Ok(ArchiveOutcome::Unsupported), |src| {
                    src.archive(&locator, archive)
                });

            match outcome {
                Ok(ArchiveOutcome::Native) => native += 1,
                Ok(ArchiveOutcome::Unsupported) => match &self.overlay {
                    Some(overlay) => match overlay.set_archived(*source_id, id, archive) {
                        Ok(()) => overlaid += 1,
                        Err(e) => errors.push(e.to_string()),
                    },
                    None => errors.push(format!("{source_id}: no overlay store")),
                },
                Err(e) => errors.push(e.to_string()),
            }
            self.sessions[position].archived = archive;
        }

        self.marked.clear();
        self.marked_groups.clear();
        let verb = if archive { "archived" } else { "unarchived" };
        self.status = Some(if errors.is_empty() {
            format!(
                "{verb} {} ({native} native, {overlaid} overlay)",
                plural(targets.len(), "session")
            )
        } else {
            format!(
                "{verb} with {}: {}",
                plural(errors.len(), "error"),
                errors[0]
            )
        });
        self.regroup();
    }

    pub(super) fn request_resume(&mut self) {
        match self.resume_argv() {
            Ok(command) => {
                self.resume = Some(command);
                self.quit = true;
            }
            Err(message) => self.status = Some(message),
        }
    }

    /// `yr`: the same shell-quoted line `r` prints, without quitting.
    pub(super) fn yank_resume(&mut self) {
        if self.copy_in_progress() {
            return;
        }
        let command = match self.resume_argv() {
            Ok(command) => command,
            Err(message) => {
                self.status = Some(message);
                return;
            }
        };
        let line = crate::runtime::resume::shell_line(&command);
        self.start_copy(line, "resume command".into());
    }

    /// `yi` / `yp`: the selected session's id, or the file it is stored in.
    pub(super) fn yank_session_field(&mut self, path: bool) {
        if self.copy_in_progress() {
            return;
        }
        let Some(session) = self.selected_session() else {
            self.status = Some("no session selected".into());
            return;
        };
        let (text, what) = if path {
            (session.store_path().display().to_string(), "path")
        } else {
            (session.id.clone(), "session id")
        };
        self.start_copy(text, what.into());
    }

    /// Resolve resume argv without changing UI state; callers display refusals.
    fn resume_argv(&self) -> Result<Vec<String>, String> {
        let session = self
            .selected_session()
            .ok_or_else(|| "no session selected to resume".to_owned())?;
        let source_id = session.source;
        let id = session.id.clone();
        let host = session.host.clone();
        let inner =
            crate::source::find(&self.sources, source_id).and_then(|s| s.resume_command(&id));
        let command = match (host, inner) {
            // A remote session resumes through its host: the argv is the same
            // one the local agent understands, wrapped in an inspectable `ssh -t`.
            (Some(host), Some(inner)) => {
                let mut command = vec!["ssh".into(), "-t".into()];
                command.push(
                    self.remote
                        .host(&host)
                        .map_or_else(|| host.clone(), |config| config.ssh.clone()),
                );
                command.extend(inner);
                Some(command)
            }
            (None, inner) => inner,
            (Some(_), None) => None,
        };
        command.ok_or_else(|| format!("{source_id} has no resume command"))
    }
}

/// Longest an agent CLI may take over one inserted turn before it is killed.
/// Generous: a turn that runs tools can legitimately take many minutes, and
/// the pid is on the status line for anyone who wants it gone sooner.
const INSERT_DEADLINE: Duration = Duration::from_mins(30);

impl App {
    pub(super) fn start_insert(&mut self) {
        let Some(session) = self.selected_session() else {
            self.status = Some("nothing selected to send to".into());
            return;
        };
        if let Err(refusal) = self.insert_route(session, "") {
            self.status = Some(refusal);
            return;
        }
        self.prompt = Prompt::Insert {
            target: (session.source, session.id.clone()),
            text: String::new(),
        };
    }

    /// The route is chosen again on send: the session may have gone live,
    /// or gone quiet, while the message was being typed.
    pub(super) fn send_insert(&mut self, target: &(SourceId, String), text: &str) {
        let message = text.trim();
        if message.is_empty() {
            self.status = Some("empty message; nothing sent".into());
            return;
        }
        let Some(session) = self
            .sessions
            .iter()
            .find(|s| s.host.is_none() && s.source == target.0 && s.id == target.1)
        else {
            self.status = Some("that session is gone; nothing sent".into());
            return;
        };
        let command = match self.insert_route(session, message) {
            Ok(command) => command,
            Err(refusal) => {
                self.status = Some(refusal);
                return;
            }
        };
        // The agent resolves some sessions relative to the project it was
        // started in, so it runs where the session did, when that still exists.
        let cwd = [&session.recorded_cwd, &session.workspace]
            .into_iter()
            .flatten()
            .find(|dir| dir.is_dir())
            .cloned();
        let label = format!("{} {}", session.source, session.id);
        let delivery = insert::send(label, &command, cwd, INSERT_DEADLINE);
        self.status = Some(match delivery.pid {
            Some(pid) => format!("sending to {} (pid {pid})…", delivery.label),
            None => format!("sending to {}…", delivery.label),
        });
        self.deliveries.push(delivery);
    }

    /// The command that delivers `message` to `session`, or why none may.
    ///
    /// One writer per conversation: with any process attached — exact or
    /// inferred, since an inferred owner may well hold it in memory — the
    /// message must go through that process or not at all. Resuming is for
    /// sessions nothing is running.
    fn insert_route(&self, session: &SessionSummary, message: &str) -> Result<Vec<String>, String> {
        if self.read_only {
            return Err("--read-only: refusing to send messages".into());
        }
        if session.host.is_some() {
            return Err("insert mode sends to local sessions only".into());
        }
        if session.subagent {
            return Err("a subagent takes messages from its parent, not from here".into());
        }
        let source = crate::source::find(&self.sources, session.source);
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
        let Some(process) = self.fleet.get(pid) else {
            return Err(format!(
                "session {held} in pid {pid}, which is gone; Ctrl-R rescans"
            ));
        };
        source
            .and_then(|source| source.live_insert_command(&session.id, message, &process.servers))
            .ok_or_else(|| {
                let jump = if process.pane.is_some() {
                    "; J jumps to its pane"
                } else {
                    ""
                };
                format!("session {held} in pid {pid}, which cannot take a message from here{jump}")
            })
    }

    /// Report every message whose agent has exited since the last look.
    pub(super) fn collect_deliveries(&mut self) {
        let mut reports = Vec::new();
        self.deliveries.retain(|delivery| match delivery.poll() {
            None => true,
            Some(Ok(())) => {
                reports.push(format!("{}: message delivered", delivery.label));
                false
            }
            Some(Err(reason)) => {
                reports.push(format!("{}: send failed: {reason}", delivery.label));
                false
            }
        });
        if !reports.is_empty() {
            self.status = Some(reports.join(" · "));
            // The agent wrote a turn; show it without waiting for the timer.
            self.rescan();
            self.reload_if_changed();
        }
    }
}

impl App {
    /// Moving focus outside this application always requires an explicit action.
    pub(super) fn jump_live(&mut self) {
        if self.read_only {
            self.status = Some("--read-only: refusing to move your tmux focus".into());
            return;
        }
        let Some(pane) = self
            .selected_session()
            .and_then(|session| session.liveness.pid())
            .and_then(|pid| self.fleet.get(pid))
            .and_then(|process| process.pane.as_ref())
        else {
            self.status = Some("selected session has no live tmux pane".into());
            return;
        };
        self.status = Some(match tmux::focus(pane) {
            Ok(()) => format!("focused tmux {}", pane.address),
            Err(e) => e,
        });
    }
}

impl App {
    /// `yy` / `yA`: copy the reply under the reader's cursor (the last one
    /// while the reader is not focused), or the whole transcript as the
    /// reader shows it now, folds included.
    pub(super) fn yank(&mut self, whole: bool) {
        if self.copy_in_progress() {
            return;
        }
        let Some(transcript) = &self.transcript else {
            self.status = Some("no transcript open to yank".into());
            return;
        };
        let (text, what) = if whole {
            (
                crate::output::export::plain_lines(
                    transcript,
                    &self.home,
                    crate::output::reader::render(
                        transcript,
                        self.fold_thinking,
                        self.fold_tools,
                        self.show_files,
                        &self.subagents_of_selected(),
                        self.offset,
                    ),
                ),
                "transcript",
            )
        } else {
            // The cursor only means something while the reader has focus;
            // from the lists, the reply people want is the latest.
            let reading = self.zen || self.focus == crate::tui::view::Pane::Transcript;
            let reply = if reading {
                self.rendered
                    .iter()
                    .take(self.cursor.saturating_add(1))
                    .rev()
                    .find_map(|line| line.turn)
                    .and_then(|turn| transcript.reply_at(turn))
            } else {
                transcript.last_reply()
            };
            let Some(reply) = reply else {
                self.status = Some(
                    if reading {
                        "no assistant reply at or above the cursor"
                    } else {
                        "no assistant reply to yank"
                    }
                    .into(),
                );
                return;
            };
            (reply, if reading { "reply" } else { "last reply" })
        };
        let description = format!("{what} ({})", plural(text.lines().count(), "line"));
        self.start_copy(text, description);
    }

    fn start_copy(&mut self, text: String, description: String) {
        if self.copy_in_progress() {
            return;
        }
        match (self.clipboard)(text) {
            Ok(pending) => {
                self.status = Some(format!("copying {description}…"));
                self.clipboard_pending = Some((pending, description));
                self.collect_clipboard();
            }
            Err(error) => self.status = Some(error.to_string()),
        }
    }

    fn copy_in_progress(&mut self) -> bool {
        if self.clipboard_pending.is_some() {
            self.status = Some("clipboard copy already in progress".into());
            true
        } else {
            false
        }
    }

    pub(super) fn collect_clipboard(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let Some((pending, description)) = &self.clipboard_pending else {
            return;
        };
        let status = match pending.try_recv() {
            Ok(Ok(helper)) => format!("yanked {description} via {helper}"),
            Ok(Err(error)) => error.to_string(),
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => "clipboard: copy worker stopped".into(),
        };
        self.clipboard_pending = None;
        self.status = Some(status);
    }
}

impl App {
    pub(super) fn open_selected(&mut self, key: (SourceId, String)) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let print = fingerprint(session);
        // A remote row's locator names a path on *that* machine: opening it
        // here would read a local file of the same name, or fail claiming the
        // host's store is unreadable. The host transport owns this body, and
        // it costs an ssh round trip, so it runs off-thread.
        if let Some(host) = session.host.clone() {
            let session = session.clone();
            self.transcript = None;
            self.transcript_key = Some(key);
            self.transcript_print = print;
            self.status = Some(match self.remote.request_transcript(&session) {
                Ok(()) => format!("{host}: reading {}…", session.id),
                Err(message) => message,
            });
            self.rerender();
            return;
        }
        let (source_id, locator) = (session.source, session.locator.clone());
        let opened = crate::source::find(&self.sources, source_id)
            .ok_or_else(|| format!("{source_id}: no reader"))
            .and_then(|src| src.open(&locator).map_err(|e| e.to_string()));

        self.transcript_key = Some(key);
        self.transcript_print = print;
        match opened {
            Ok(transcript) => self.transcript = Some(transcript),
            Err(message) => {
                self.transcript = None;
                self.status = Some(message);
            }
        }
        self.rerender();
    }

    /// Install a finished remote fetch, if the row that asked for it is still
    /// the open one.
    pub(super) fn collect_remote_transcript(&mut self) {
        let Some((key, result)) = self.remote.poll_transcript() else {
            return;
        };
        if self.transcript_key.as_ref() != Some(&key) {
            return;
        }
        match result {
            Ok(transcript) => {
                self.transcript = Some(transcript);
                self.status = None;
            }
            Err(e) => {
                self.transcript = None;
                self.status = Some(e.to_string());
            }
        }
        self.rerender();
    }
}
