use std::path::Path;

use llm_sessions::runtime::resume;
use llm_sessions::storage::index::Index;
use llm_sessions::tui::app::App;
use llm_sessions::{Result, tui};

use super::{Cached, Liveliness, Loaded, Remote, emit, scan_all_with};

/// The default command: the interactive browser, printing the resume command
/// the user picked, if any, for the shell to run.
pub(super) fn run(home: &Path, read_only: bool) -> Result<()> {
    // Remote rows belong to the App's background cache: scanning them inline
    // here would duplicate every host the moment the cache filled.
    let Loaded {
        sources,
        overlay,
        fleet,
        scan,
        ..
    } = scan_all_with(home, Liveliness::Needed, Cached::Borrow, Remote::Cached);
    let index = Index::open(&Index::default_path(home)).ok();
    let mut app = App::new(
        home,
        sources,
        scan.sessions,
        scan.failures,
        overlay,
        index,
        fleet,
    );
    app.set_read_only(read_only);
    if let Some(command) = tui::run(&mut app)? {
        emit(&resume::shell_line(&command));
    }
    Ok(())
}
