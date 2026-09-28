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
        let Some(session) = self.selected_session() else {
            return;
        };
        let source_id = session.source;
        let id = session.id.clone();
        // A remote session resumes through its host: the argv is the same one
        // the local agent understands, wrapped in an inspectable `ssh -t`.
        if let Some(host) = session.host.clone() {
            let command = crate::source::find(&self.sources, source_id)
                .and_then(|s| s.resume_command(&id))
                .map(|inner| {
                    let mut command = vec!["ssh".into(), "-t".into()];
                    command.push(
                        self.remote
                            .host(&host)
                            .map_or_else(|| host.clone(), |config| config.ssh.clone()),
                    );
                    command.extend(inner);
                    command
                });
            match command {
                Some(command) => {
                    self.resume = Some(command);
                    self.quit = true;
                }
                None => self.status = Some(format!("{source_id} has no resume command")),
            }
            return;
        }
        match crate::source::find(&self.sources, source_id).and_then(|s| s.resume_command(&id)) {
            Some(command) => {
                self.resume = Some(command);
                self.quit = true;
            }
            None => self.status = Some(format!("{source_id} has no resume command")),
        }
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
