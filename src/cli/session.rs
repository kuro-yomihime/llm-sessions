//! Subcommands that act on sessions named by id: `show` and `archive`.

use std::path::{Path, PathBuf};

use llm_sessions::catalog::loader::{Resolved, resolve};
use llm_sessions::error::Error;
use llm_sessions::model::ArchiveOutcome;
use llm_sessions::output::export::{self, Format};
use llm_sessions::source;
use llm_sessions::{Result, SessionSummary, Transcript};

use super::args::{ArchiveArgs, ShowArgs, ShowFormat};
use super::{Liveliness, Loaded, emit, scan_all};

/// Resolve `id` and open its transcript, with tier-1 title and workspace.
fn load(home: &Path, args: &ShowArgs) -> Result<(Transcript, SessionSummary)> {
    let Loaded { loader, scan, .. } = scan_all(home, Liveliness::Skip);
    let rows: Vec<_> = scan
        .sessions
        .into_iter()
        .filter(|session| {
            args.origin
                .as_deref()
                .is_none_or(|origin| session.origin_id.as_deref() == Some(origin))
                && args.source.is_none_or(|source| session.source == source)
        })
        .collect();
    let session = resolve_one(&rows, &args.id)?;
    Ok((loader.open(session)?, session.clone()))
}

pub(super) fn show(home: &Path, args: &ShowArgs) -> Result<()> {
    let (transcript, session) = load(home, args)?;

    if let Some(format) = args.format
        && !matches!(format, ShowFormat::Text)
    {
        let format = match format {
            ShowFormat::Md => Format::Markdown,
            _ => Format::Html,
        };
        return deliver(
            &export::render(&transcript, Some(&session), format),
            args.out.as_deref(),
        );
    }

    if args.json {
        return deliver(
            &serde_json::to_string_pretty(&transcript)?,
            args.out.as_deref(),
        );
    }

    deliver(
        &export::plain(
            &transcript,
            home,
            !args.thinking,
            !args.tools,
            llm_sessions::clock::offset(),
        ),
        args.out.as_deref(),
    )
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
    let Loaded { loader, scan, .. } = scan_all(home, Liveliness::Skip);
    let archived = !args.undo;
    for id in &args.ids {
        let session = resolve_one(&scan.sessions, id)?;
        // The overlay is local state and the remote store is someone
        // else's; writing a local row for a remote id would lie about
        // where the archive state lives.
        if !session.is_local() {
            return Err(Error::Remote {
                host: session
                    .origin_label
                    .clone()
                    .unwrap_or_else(|| "remote".into()),
                message: "archiving a remote session is not supported".into(),
            });
        }
        let outcome = source::find(&loader.sources, session.source)
            .map_or(Ok(ArchiveOutcome::Unsupported), |src| {
                src.archive(&session.locator, archived)
            })?;
        let how = match outcome {
            ArchiveOutcome::Native => "native",
            ArchiveOutcome::Unsupported => {
                let overlay = loader.overlay.as_ref().ok_or(Error::NoHome)?;
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
    match resolve(sessions, needle) {
        Resolved::One(session) => Ok(session),
        Resolved::Missing => Err(Error::MissingRow {
            db: PathBuf::from("<any source>"),
            id: needle.to_string(),
        }),
        Resolved::Ambiguous(candidates) => {
            eprintln!(
                "llm-sessions: {needle:?} matches {} sessions:",
                candidates.len()
            );
            for c in candidates.iter().take(10) {
                eprintln!(
                    "  {} {} origin={} host={}",
                    c.source,
                    c.id,
                    c.origin_id.as_deref().unwrap_or("<unknown>"),
                    c.origin_label.as_deref().unwrap_or("local")
                );
            }
            Err(Error::MissingRow {
                db: PathBuf::from("<ambiguous>"),
                id: needle.to_string(),
            })
        }
    }
}
