//! Five-state session status: running, idle, waiting, failed, done.
//!
//! [`crate::runtime::live`] knows whether a process is attached and whether
//! its CPU clock moves; it cannot tell a tool awaiting approval from a
//! question awaiting an answer, or a finished session from a failed one. The
//! transcript tail can. Status joins the two:
//!
//! | liveness        | activity  | transcript tail          | status            |
//! |-----------------|-----------|--------------------------|-------------------|
//! | not running     | —         | unrecovered failure      | `failed`          |
//! | not running     | —         | anything else            | `done`            |
//! | running         | working   | —                        | `running`         |
//! | running         | waiting   | unanswered tool call     | `waiting/approval`|
//! | running         | waiting   | prose ending in `?`      | `waiting/input`   |
//! | running         | waiting   | anything else            | `idle`            |
//! | running         | unknown   | written < 90 s ago       | `running`         |
//! | running         | unknown   | older                    | `idle`            |
//!
//! Every non-`running` answer needs the transcript, a tier-2 read, so
//! callers derive status for the sessions they watch, never for a whole
//! listing. Status is derived and heuristic: "failed" rests on the phrases in
//! [`reports_failure`]. Only the agent process's own CPU is measured, so an
//! agent waiting on a tool it is executing reads `waiting/approval`.
//!
//! Pure: no I/O, no clock. Callers pass `now`.

use std::time::Duration;

use serde::Serialize;
use time::OffsetDateTime;

use crate::model::{Block, Role, Transcript, Turn};
use crate::runtime::live::Activity;

/// A live session with unknown activity counts as running this long after
/// its last write.
pub const UNKNOWN_RUNNING_WINDOW: Duration = Duration::from_secs(90);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    Idle,
    Waiting,
    Failed,
    Done,
}

/// What a [`Status::Waiting`] session waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WaitingReason {
    /// A tool call has no result yet.
    Approval,
    /// The agent's last words were a question.
    Input,
}

/// What status derivation needs from a transcript, computed once per
/// `(session, updated)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// The last turn is an assistant tool call that has no result yet.
    pub pending_tool_call: bool,
    /// The last turn is assistant prose ending in a question.
    pub trailing_question: bool,
    /// The session's final event is a failed tool result, or final assistant
    /// prose reporting a failure.
    pub unrecovered_failure: bool,
}

/// The latest conversational event, which decides failure.
#[derive(Clone, Copy)]
enum Last<'a> {
    Text(&'a str),
    Call,
    Result { ok: bool },
}

/// One pass over a transcript.
pub fn analyze(transcript: &Transcript) -> Facts {
    let mut facts = Facts::default();

    let conversational: Vec<&Turn> = transcript
        .turns
        .iter()
        .filter(|turn| turn.role != Role::System && turn.blocks.iter().any(is_conversational))
        .collect();

    // The final event decides failure.
    let mut last: Option<Last> = None;
    for turn in &conversational {
        for block in &turn.blocks {
            match block {
                Block::Text { text } if turn.role == Role::Assistant && !text.trim().is_empty() => {
                    last = Some(Last::Text(text));
                }
                Block::ToolCall { .. } => last = Some(Last::Call),
                Block::ToolResult { ok, .. } => last = Some(Last::Result { ok: *ok }),
                _ => {}
            }
        }
    }
    facts.unrecovered_failure = match last {
        Some(Last::Result { ok }) => !ok,
        Some(Last::Text(text)) => reports_failure(text),
        Some(Last::Call) | None => false,
    };

    if let Some(turn) = conversational.last()
        && turn.role == Role::Assistant
    {
        // Within the turn: was the last tool-ish block a call (no result yet),
        // and does prose come after every tool call?
        let mut pending = false;
        let mut question = false;
        for block in &turn.blocks {
            match block {
                Block::ToolCall { .. } => {
                    pending = true;
                    question = false;
                }
                Block::ToolResult { .. } => pending = false,
                Block::Text { text } if !text.trim().is_empty() => {
                    question = ends_with_question(text);
                }
                _ => {}
            }
        }
        facts.pending_tool_call = pending;
        facts.trailing_question = !pending && question;
    }
    facts
}

/// The status the table in the module documentation assigns.
///
/// `live` is whether a process is attached ([`crate::model::Liveness::is_running`]);
/// `activity` is that process's; `updated` is the session's last write.
pub fn derive_status(
    live: bool,
    activity: Activity,
    facts: &Facts,
    updated: OffsetDateTime,
    now: OffsetDateTime,
) -> (Status, Option<WaitingReason>) {
    if !live {
        return if facts.unrecovered_failure {
            (Status::Failed, None)
        } else {
            (Status::Done, None)
        };
    }
    match activity {
        Activity::Working => (Status::Running, None),
        Activity::Waiting if facts.pending_tool_call => {
            (Status::Waiting, Some(WaitingReason::Approval))
        }
        Activity::Waiting if facts.trailing_question => {
            (Status::Waiting, Some(WaitingReason::Input))
        }
        Activity::Waiting => (Status::Idle, None),
        Activity::Unknown => {
            // A clock running behind the transcript is not "old".
            let age: Duration = (now - updated).try_into().unwrap_or(Duration::ZERO);
            if age < UNKNOWN_RUNNING_WINDOW {
                (Status::Running, None)
            } else {
                (Status::Idle, None)
            }
        }
    }
}

/// The status a reader sees, debounced against poll-to-poll flapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settled {
    pub status: Status,
    pub reason: Option<WaitingReason>,
    /// When the published status was first observed.
    pub since: OffsetDateTime,
    /// A different status seen on the latest poll only, and when.
    pub candidate: Option<(Status, Option<WaitingReason>, OffsetDateTime)>,
}

/// Fold one poll's derived status into the published one.
///
/// A new status must be observed on two consecutive polls before it is
/// published, and its `since` is the first of those observations: CPU
/// sampling between two tool calls routinely reads one idle poll. A session
/// seen for the first time publishes at once with `since = first_sight`
/// (its last write).
pub fn settle(
    previous: Option<Settled>,
    status: Status,
    reason: Option<WaitingReason>,
    first_sight: OffsetDateTime,
    now: OffsetDateTime,
) -> Settled {
    let Some(previous) = previous else {
        return Settled {
            status,
            reason,
            since: first_sight,
            candidate: None,
        };
    };
    if (status, reason) == (previous.status, previous.reason) {
        return Settled {
            candidate: None,
            ..previous
        };
    }
    match previous.candidate {
        Some((s, r, seen)) if (s, r) == (status, reason) => Settled {
            status,
            reason,
            // Only the waiting reason changed: the status itself did not.
            since: if status == previous.status {
                previous.since
            } else {
                seen
            },
            candidate: None,
        },
        _ => Settled {
            candidate: Some((status, reason, now)),
            ..previous
        },
    }
}

fn is_conversational(block: &Block) -> bool {
    match block {
        Block::Text { text } => !text.trim().is_empty(),
        Block::ToolCall { .. } | Block::ToolResult { .. } => true,
        _ => false,
    }
}

/// Trailing markdown emphasis and quotes do not hide the question mark.
fn ends_with_question(text: &str) -> bool {
    text.trim_end_matches(|c: char| {
        c.is_whitespace() || matches!(c, '*' | '_' | '`' | '"' | '\'' | ')')
    })
    .ends_with('?')
}

/// Final assistant prose that reports an unrecovered failure. Deliberately
/// phrase-based: a bare "error" matches every "added error handling".
pub fn reports_failure(text: &str) -> bool {
    const FAILED: &[&str] = &[
        "failed",
        "failing",
        "still fail",
        "error:",
        "unable to",
        "could not",
        "couldn't",
        "panicked",
        "i'm blocked",
        "i am blocked",
    ];
    const RECOVERED: &[&str] = &[
        "all tests pass",
        "tests pass",
        "now pass",
        "passing",
        "fixed",
        "resolved",
        "succeeded",
        "0 failed",
        "no errors",
    ];
    let lower = text.to_lowercase();
    FAILED.iter().any(|p| lower.contains(p)) && !RECOVERED.iter().any(|p| lower.contains(p))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::model::SourceId;

    fn at(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000 + secs).unwrap()
    }

    fn turn(role: Role, blocks: Vec<Block>) -> Turn {
        let mut turn = Turn::new(role);
        turn.blocks = blocks;
        turn
    }

    fn transcript(turns: Vec<Turn>) -> Transcript {
        let mut t = Transcript::new("s", SourceId::Claude);
        for turn in turns {
            t.push(turn);
        }
        t
    }

    fn call(name: &str, input: Value) -> Block {
        Block::ToolCall {
            name: name.into(),
            input,
        }
    }

    fn result(ok: bool, output: &str) -> Block {
        Block::ToolResult {
            name: None,
            ok,
            output: output.into(),
        }
    }

    fn live(activity: Activity, t: &Transcript) -> (Status, Option<WaitingReason>) {
        derive_status(true, activity, &analyze(t), at(0), at(1))
    }

    fn dead(t: &Transcript) -> Status {
        derive_status(false, Activity::Unknown, &analyze(t), at(0), at(1)).0
    }

    #[test]
    fn an_unanswered_tool_call_waits_for_approval() {
        let t = transcript(vec![
            turn(Role::User, vec![Block::text("fix the auth tests")]),
            turn(
                Role::Assistant,
                vec![
                    Block::thinking("need to run them"),
                    Block::text("Running the suite."),
                    call("Bash", json!({"command": "cargo test -p auth"})),
                ],
            ),
        ]);
        assert_eq!(
            live(Activity::Waiting, &t),
            (Status::Waiting, Some(WaitingReason::Approval))
        );
        assert_eq!(live(Activity::Working, &t), (Status::Running, None));
    }

    #[test]
    fn a_status_change_publishes_only_after_two_consecutive_polls() {
        let first = settle(None, Status::Running, None, at(-50), at(0));
        assert_eq!(
            (first.status, first.since),
            (Status::Running, at(-50)),
            "first sight is immediate"
        );

        // One stray idle poll between tool calls never reaches the reader.
        let blip = settle(Some(first), Status::Idle, None, at(-50), at(3));
        assert_eq!((blip.status, blip.since), (Status::Running, at(-50)));
        let back = settle(Some(blip), Status::Running, None, at(-50), at(6));
        assert_eq!(back, first);

        // Alternating statuses never settle on the new one.
        let a = settle(Some(back), Status::Idle, None, at(-50), at(9));
        let b = settle(Some(a), Status::Done, None, at(-50), at(12));
        assert_eq!(b.status, Status::Running);

        // Twice in a row: published, dated from the first observation.
        let c = settle(Some(b), Status::Done, None, at(-50), at(15));
        assert_eq!(
            (c.status, c.since, c.candidate),
            (Status::Done, at(12), None)
        );
    }

    #[test]
    fn a_new_waiting_reason_keeps_the_waiting_since() {
        let approval = settle(
            None,
            Status::Waiting,
            Some(WaitingReason::Approval),
            at(0),
            at(0),
        );
        let once = settle(
            Some(approval),
            Status::Waiting,
            Some(WaitingReason::Input),
            at(0),
            at(3),
        );
        assert_eq!(once.reason, Some(WaitingReason::Approval));
        let twice = settle(
            Some(once),
            Status::Waiting,
            Some(WaitingReason::Input),
            at(0),
            at(6),
        );
        assert_eq!(
            (twice.reason, twice.since),
            (Some(WaitingReason::Input), at(0))
        );
    }

    #[test]
    fn a_call_answered_in_the_same_turn_is_not_pending() {
        // OpenCode-style stores keep call and result in one assistant turn.
        let t = transcript(vec![turn(
            Role::Assistant,
            vec![call("bash", json!({"command": "ls"})), result(true, "a b")],
        )]);
        assert_eq!(live(Activity::Waiting, &t), (Status::Idle, None));
    }

    #[test]
    fn a_trailing_question_waits_for_input_but_only_after_the_last_call() {
        let asks = transcript(vec![
            turn(Role::User, vec![Block::text("add dark mode")]),
            turn(
                Role::Assistant,
                vec![Block::text("Should it follow the **system theme**?**\n")],
            ),
        ]);
        assert_eq!(
            live(Activity::Waiting, &asks),
            (Status::Waiting, Some(WaitingReason::Input))
        );

        // The question came before a tool call: the call is what's pending.
        let then_calls = transcript(vec![turn(
            Role::Assistant,
            vec![
                Block::text("Shall I check the config?"),
                call("read", json!({"path": "app.toml"})),
            ],
        )]);
        assert_eq!(
            live(Activity::Waiting, &then_calls),
            (Status::Waiting, Some(WaitingReason::Approval))
        );

        // A user turn after the question means the agent owes the next move.
        let answered = transcript(vec![
            turn(Role::Assistant, vec![Block::text("Light or dark?")]),
            turn(Role::User, vec![Block::text("dark")]),
        ]);
        assert_eq!(live(Activity::Waiting, &answered), (Status::Idle, None));
    }

    #[test]
    fn a_failed_final_tool_result_fails_a_dead_session() {
        let t = transcript(vec![
            turn(
                Role::Assistant,
                vec![call("bash", json!({"command": "cargo test"}))],
            ),
            turn(Role::Tool, vec![result(false, "3 failed; 41 passed")]),
        ]);
        assert_eq!(dead(&t), Status::Failed);
    }

    #[test]
    fn a_failure_the_agent_recovered_from_is_done() {
        let t = transcript(vec![
            turn(Role::Tool, vec![result(false, "1 failed")]),
            turn(
                Role::Assistant,
                vec![Block::text("Fixed the fixture; all tests pass now.")],
            ),
        ]);
        assert_eq!(dead(&t), Status::Done);
    }

    #[test]
    fn final_prose_reporting_a_failure_fails_a_dead_session() {
        let t = transcript(vec![turn(
            Role::Assistant,
            vec![Block::text(
                "I couldn't get the migration to apply; the schema lock is held.",
            )],
        )]);
        assert_eq!(dead(&t), Status::Failed);
        assert!(!reports_failure("Added error handling to the parser."));
    }

    #[test]
    fn unknown_activity_is_running_only_within_90_seconds_of_the_last_write() {
        let facts = Facts::default();
        let updated = at(0);
        let derive = |secs| derive_status(true, Activity::Unknown, &facts, updated, at(secs)).0;
        assert_eq!(derive(89), Status::Running);
        assert_eq!(derive(90), Status::Idle);
        // A clock running behind the transcript is not "old".
        assert_eq!(derive(-5), Status::Running);
    }
}
