//! The parts of [`App`] that reach outside its own state: writing archive
//! flags through a source or the overlay, building the argv a resume hands
//! to the shell, moving tmux focus, and reading a transcript body through
//! its source or host.

use crate::model::{ArchiveOutcome, SourceId};
use crate::output::reader::plural;
use crate::runtime::tmux;
use crate::tui::app::{App, fingerprint};

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
