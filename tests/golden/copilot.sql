-- Golden fixture: structure copied verbatim from a real store,
-- every payload redacted. Regenerate only from real data.

PRAGMA foreign_keys = OFF;

CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                cwd TEXT,
                repository TEXT,
                host_type TEXT,
                branch TEXT,
                summary TEXT,
                created_at TEXT DEFAULT (datetime('now')),
                updated_at TEXT DEFAULT (datetime('now'))
            );
INSERT INTO "sessions" ("id", "cwd", "repository", "host_type", "branch", "summary", "created_at", "updated_at") VALUES ('0688c579-6aae-4941-a1e9-a48d30fe5c82', '/workspace', NULL, NULL, NULL, NULL, '2026-06-04T16:00:30.602Z', '2026-06-04T16:00:35.853Z');

CREATE TABLE IF NOT EXISTS turns (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id),
                turn_index INTEGER NOT NULL,
                user_message TEXT,
                assistant_response TEXT,
                timestamp TEXT DEFAULT (datetime('now')),
                UNIQUE(session_id, turn_index)
            );

CREATE TABLE IF NOT EXISTS forge_trajectory_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL REFERENCES sessions(id),
                tool_call_id TEXT,
                turn_index INTEGER,
                event_type TEXT NOT NULL,
                command TEXT,
                output TEXT,
                exit_code INTEGER,
                event_key TEXT,
                event_value TEXT,
                created_at TEXT DEFAULT (datetime('now'))
            );
