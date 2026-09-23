//! Subcommands that act on sessions named by id: `show` and `archive`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use llm_sessions::catalog::workspace;
use llm_sessions::error::Error;
use llm_sessions::model::ArchiveOutcome;
use llm_sessions::output::export::{self, Format};
use llm_sessions::output::reader::{LineKind, fit, render};
use llm_sessions::source::{self, remote};
use llm_sessions::{Result, SessionSummary, Transcript};

use super::args::{ArchiveArgs, ShowArgs, ShowFormat};
use super::{Liveliness, Loaded, emit, scan_all};

pub(super) fn show(home: &Path, args: &ShowArgs) -> Result<()> {
    let Loaded {
        sources,
        hosts,
        scan,
        ..
    } = scan_all(home, Liveliness::Skip);
    let session = resolve_one(&scan.sessions, &args.id)?;
    let mut transcript = open_session(home, &sources, &hosts, session)?;
    // Tier 1 already resolved title and workspace authoritatively (Codex
    // titles live in `state_5.sqlite`, not in the rollout; the scan also
    // canonicalized the path and rolled worktrees up). Prefer them.
    transcript.title = transcript.title.take().or_else(|| session.title.clone());
    transcript.workspace = session
        .workspace
        .clone()
        .or_else(|| transcript.workspace.take());

    if let Some(format) = args.format
        && !matches!(format, ShowFormat::Text)
    {
        let format = match format {
            ShowFormat::Md => Format::Markdown,
            _ => Format::Html,
        };
        return deliver(
            &export::render(&transcript, Some(session), format),
            args.out.as_deref(),
        );
    }

    if args.json {
        return deliver(
            &serde_json::to_string_pretty(&transcript)?,
            args.out.as_deref(),
        );
    }

    let mut text = format!(
        "{} · {} · {}\n\n",
        transcript.source,
        transcript.title.as_deref().unwrap_or("—"),
        transcript
            .workspace
            .as_ref()
            .map_or_else(|| "?".to_string(), |p| workspace::display(p, home)),
    );
    // The reader's gutter and flush-right detail are part of the line, not
    // decoration the TUI adds: plain text keeps both, separated by spaces.
    // The dump has no column of its own, so tables take their natural width
    // and prose is left for the terminal to wrap.
    for line in fit(
        render(
            &transcript,
            !args.thinking,
            !args.tools,
            true,
            &HashMap::new(),
            llm_sessions::clock::offset(),
        ),
        usize::MAX,
    ) {
        if line.kind == LineKind::Rule {
            match line.text.as_str() {
                "" => text.push_str(&"─".repeat(60)),
                label => {
                    let _ = write!(text, "──── {label} ────");
                }
            }
            text.push('\n');
            continue;
        }
        let body = match &line.meta {
            Some(meta) if line.text.is_empty() => meta.clone(),
            Some(meta) => format!("{}  {meta}", line.text),
            None => line.text.clone(),
        };
        text.push_str(line.rail.glyph(line.first));
        text.push_str(body.trim_end());
        text.push('\n');
    }
    deliver(&text, args.out.as_deref())
}

/// Print the rendered output, or write it to `-o` and say so on stderr.
fn deliver(rendered: &str, out: Option<&Path>) -> Result<()> {
    match out {
        Some(path) => {
            std::fs::write(path, rendered).map_err(llm_sessions::error::io(path))?;
            eprintln!("llm-sessions: wrote {}", path.display());
        }
        None => emit(rendered),
    }
    Ok(())
}

pub(super) fn archive(home: &Path, args: &ArchiveArgs) -> Result<()> {
    let Loaded {
        sources,
        overlay,
        scan,
        ..
    } = scan_all(home, Liveliness::Skip);
    let archived = !args.undo;
    for id in &args.ids {
        let session = resolve_one(&scan.sessions, id)?;
        // The overlay is local state and the remote store is someone
        // else's; writing a local row for a remote id would lie about
        // where the archive state lives.
        if let Some(host) = &session.host {
            return Err(Error::Remote {
                host: host.clone(),
                message: "archiving a remote session is not supported".into(),
            });
        }
        let outcome = source::find(&sources, session.source)
            .map_or(Ok(ArchiveOutcome::Unsupported), |src| {
                src.archive(&session.locator, archived)
            })?;
        let how = match outcome {
            ArchiveOutcome::Native => "native",
            ArchiveOutcome::Unsupported => {
                let overlay = overlay.as_ref().ok_or(Error::NoHome)?;
                overlay.set_archived(session.source, &session.id, archived)?;
                "overlay"
            }
        };
        emit(&format!(
            "{} {} {} ({how})",
            if archived { "archived" } else { "restored" },
            session.source,
            session.id
        ));
    }
    Ok(())
}

/// Resolve an id or unique prefix to exactly one session.
fn resolve_one<'a>(sessions: &'a [SessionSummary], needle: &str) -> Result<&'a SessionSummary> {
    let exact: Vec<&SessionSummary> = sessions.iter().filter(|s| s.id == needle).collect();
    let candidates = if exact.is_empty() {
        sessions
            .iter()
            .filter(|s| s.id.starts_with(needle))
            .collect::<Vec<_>>()
    } else {
        exact
    };
    match candidates.len() {
        1 => Ok(candidates[0]),
        0 => Err(Error::MissingRow {
            db: PathBuf::from("<any source>"),
            id: needle.to_string(),
        }),
        n => {
            eprintln!("llm-sessions: {needle:?} matches {n} sessions:");
            for c in candidates.iter().take(10) {
                eprintln!("  {} {}", c.source, c.id);
            }
            Err(Error::MissingRow {
                db: PathBuf::from("<ambiguous>"),
                id: needle.to_string(),
            })
        }
    }
}

/// Open a session wherever it lives: locally through its adapter, remotely
/// through the host that listed it.
fn open_session(
    home: &Path,
    sources: &[Box<dyn source::Source>],
    hosts: &[remote::Host],
    session: &SessionSummary,
) -> Result<Transcript> {
    match &session.host {
        None => {
            let reader = source::find(sources, session.source).ok_or(Error::Unsupported {
                agent: session.source,
                what: "reading transcripts",
            })?;
            reader.open(&session.locator)
        }
        Some(name) => {
            let host = hosts
                .iter()
                .find(|host| host.name() == name)
                .ok_or_else(|| Error::Remote {
                    host: name.clone(),
                    message: "no host with this name in hosts.json".into(),
                })?;
            host.open(session, home)
        }
    }
}
