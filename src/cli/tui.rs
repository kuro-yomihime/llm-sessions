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
        loader,
        fleet,
        scan,
    } = scan_all_with(home, Liveliness::Needed, Cached::Borrow, Remote::Skip);
    let index = Index::open(&Index::default_path(home)).ok();
    let mut app = App::new_with_mode(
        home,
        loader.sources,
        scan.sessions,
        scan.failures,
        loader.overlay,
        index,
        fleet,
        read_only,
    );
    if let Some(command) = tui::run(&mut app)? {
        emit(&resume::shell_line(&command));
    }
    Ok(())
}
