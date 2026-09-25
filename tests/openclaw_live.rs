//! Opt-in check against a real store: read-only, prints counts never content.
//!
//! `OPENCLAW_LIVE_HOME=$HOME cargo test --test openclaw_live -- --ignored --nocapture`

use llm_sessions::model::Block;
use llm_sessions::source::{Source, openclaw::OpenClaw};

#[test]
#[ignore = "needs a live OpenClaw store: set OPENCLAW_LIVE_HOME"]
fn every_live_session_lists_and_opens() {
    let Ok(home) = std::env::var("OPENCLAW_LIVE_HOME") else {
        return;
    };
    let source = OpenClaw::new(std::path::Path::new(&home));
    let rows = source.list().expect("list");
    assert!(!rows.is_empty(), "a live store lists sessions");
    let (mut turns, mut unknown, mut tools, mut results, mut thinking) = (0, 0, 0, 0, 0);
    let mut with_cwd = 0;
    for row in &rows {
        with_cwd += usize::from(row.recorded_cwd.is_some());
        let transcript = source.open(&row.locator).expect("open");
        assert!(!transcript.turns.is_empty(), "{} opens to turns", row.id);
        turns += transcript.turns.len();
        unknown += transcript.unknown_records;
        for block in transcript.turns.iter().flat_map(|turn| &turn.blocks) {
            match block {
                Block::ToolCall { .. } => tools += 1,
                Block::ToolResult { .. } => results += 1,
                Block::Thinking { .. } => thinking += 1,
                _ => {}
            }
        }
    }
    println!(
        "sessions={} with_cwd={with_cwd} titled={} usage={} subagent={} archived={} \
         turns={turns} tool_calls={tools} tool_results={results} thinking={thinking} unknown={unknown}",
        rows.len(),
        rows.iter().filter(|row| row.title.is_some()).count(),
        rows.iter().filter(|row| row.usage.is_some()).count(),
        rows.iter().filter(|row| row.subagent).count(),
        rows.iter().filter(|row| row.archived).count(),
    );
    assert_eq!(unknown, 0, "every live record is classified");
}
