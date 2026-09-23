mod cli;

use std::process::ExitCode;

fn main() -> ExitCode {
    // Capture the local offset before scanning starts worker threads.
    llm_sessions::clock::init();
    match cli::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("llm-sessions: {error}");
            ExitCode::FAILURE
        }
    }
}
