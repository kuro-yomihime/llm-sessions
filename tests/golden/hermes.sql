-- Hermes fixture, redacted from the verified schema (version 22). Only the
-- columns the adapter reads are populated; NOT NULL columns that the adapter
-- ignores get defaults so the fixture stays writable.

CREATE TABLE schema_version (version INTEGER NOT NULL);
INSERT INTO schema_version VALUES (22);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    title TEXT,
    cwd TEXT,
    git_branch TEXT,
    model TEXT,
    parent_session_id TEXT,
    started_at REAL NOT NULL,
    ended_at REAL,
    input_tokens INTEGER DEFAULT 0,
    output_tokens INTEGER DEFAULT 0,
    estimated_cost_usd REAL,
    actual_cost_usd REAL,
    archived INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT,
    tool_call_id TEXT,
    tool_calls TEXT,
    tool_name TEXT,
    timestamp REAL NOT NULL,
    finish_reason TEXT,
    reasoning TEXT,
    reasoning_content TEXT,
    active INTEGER NOT NULL DEFAULT 1,
    compacted INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE session_model_usage (
    session_id TEXT NOT NULL,
    model TEXT NOT NULL,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    estimated_cost_usd REAL NOT NULL DEFAULT 0,
    actual_cost_usd REAL NOT NULL DEFAULT 0,
    PRIMARY KEY (session_id, model)
);

-- A plain CLI conversation: question, reasoned answer, one tool round trip.
INSERT INTO sessions VALUES
  ('20260101_000000_plain', 'Fix the failing build', '/workspace/golden', 'feat/golden',
   'glm-5', NULL, 1767225600.0, 1767225700.0, 1200, 300, 0.01, NULL, 0);

INSERT INTO messages (session_id, role, content, timestamp, reasoning_content) VALUES
  ('20260101_000000_plain', 'user', 'The build is red, fix it.', 1767225601.0, NULL),
  ('20260101_000000_plain', 'assistant', 'Reading the log first.', 1767225610.0, 'The error names a missing module.');
INSERT INTO messages (session_id, role, content, tool_calls, tool_name, timestamp, finish_reason) VALUES
  ('20260101_000000_plain', 'assistant', NULL,
   '[{"function":{"name":"read_file","arguments":"{\"path\":\"/workspace/golden/src/lib.rs\"}"}}]',
   NULL, 1767225620.0, 'tool_calls');
INSERT INTO messages (session_id, role, content, tool_name, timestamp, finish_reason) VALUES
  ('20260101_000000_plain', 'tool', 'fn main() {}', 'read_file', 1767225621.0, NULL);
INSERT INTO messages (session_id, role, content, timestamp) VALUES
  ('20260101_000000_plain', 'assistant', 'The module was never declared; fixed.', 1767225630.0);
-- Superseded by a compaction: must not be replayed.
INSERT INTO messages (session_id, role, content, timestamp, compacted) VALUES
  ('20260101_000000_plain', 'user', 'stale pre-compaction turn', 1767225590.0, 1);

INSERT INTO session_model_usage VALUES
  ('20260101_000000_plain', 'glm-5', 1200, 300, 0.01, 0.0);

-- An archived child of the first session.
INSERT INTO sessions VALUES
  ('20260101_000001_child', 'Side task', '/workspace/golden', 'feat/golden',
   'glm-5', '20260101_000000_plain', 1767225800.0, 1767225850.0, 50, 10, NULL, NULL, 1);
INSERT INTO messages (session_id, role, content, timestamp) VALUES
  ('20260101_000001_child', 'user', 'Side question.', 1767225801.0),
  ('20260101_000001_child', 'assistant', 'Side answer.', 1767225810.0);
