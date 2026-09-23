-- Golden fixture: structure copied verbatim from a real store,
-- every payload redacted. Regenerate only from real data.

PRAGMA foreign_keys = OFF;

CREATE TABLE IF NOT EXISTS `project` (
	`id` text PRIMARY KEY,
	`worktree` text NOT NULL,
	`vcs` text,
	`name` text,
	`icon_url` text,
	`icon_color` text,
	`time_created` integer NOT NULL,
	`time_updated` integer NOT NULL,
	`time_initialized` integer,
	`sandboxes` text NOT NULL
, `commands` text, `icon_url_override` text);
INSERT INTO "project" ("id", "worktree", "vcs", "name", "icon_url", "icon_color", "time_created", "time_updated", "time_initialized", "sandboxes", "commands", "icon_url_override") VALUES ('global', '/workspace', NULL, NULL, NULL, NULL, 1770902227270, 1780758806926, NULL, '[]', NULL, NULL);

CREATE TABLE IF NOT EXISTS `session` (
	`id` text PRIMARY KEY,
	`project_id` text NOT NULL,
	`parent_id` text,
	`slug` text NOT NULL,
	`directory` text NOT NULL,
	`title` text NOT NULL,
	`version` text NOT NULL,
	`share_url` text,
	`summary_additions` integer,
	`summary_deletions` integer,
	`summary_files` integer,
	`summary_diffs` text,
	`revert` text,
	`permission` text,
	`time_created` integer NOT NULL,
	`time_updated` integer NOT NULL,
	`time_compacting` integer,
	`time_archived` integer, `workspace_id` text, `path` text, `agent` text, `model` text,
	CONSTRAINT `fk_session_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
);
INSERT INTO "session" ("id", "project_id", "parent_id", "slug", "directory", "title", "version", "share_url", "summary_additions", "summary_deletions", "summary_files", "summary_diffs", "revert", "permission", "time_created", "time_updated", "time_compacting", "time_archived", "workspace_id", "path", "agent", "model") VALUES ('ses_2ffe21e3affeR8H1RfNW2u75G1', 'global', NULL, 'proud-comet', '/workspace', 'gamma delta epsilon zeta', '1.0.21', NULL, 0, 0, 0, NULL, NULL, NULL, 1773823451589, 1773823451661, NULL, 1789041019000, NULL, NULL, NULL, NULL);

CREATE TABLE IF NOT EXISTS `message` (
	`id` text PRIMARY KEY,
	`session_id` text NOT NULL,
	`time_created` integer NOT NULL,
	`time_updated` integer NOT NULL,
	`data` text NOT NULL,
	CONSTRAINT `fk_message_session_id_session_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session`(`id`) ON DELETE CASCADE
);
INSERT INTO "message" ("id", "session_id", "time_created", "time_updated", "data") VALUES ('msg_d001de1c7001oC1QpNyikMLhMk', 'ses_2ffe21e3affeR8H1RfNW2u75G1', 1773823451599, 1780588769783, '{"role": "user", "time": {"created": 1773823451599}, "summary": {"diffs": []}, "agent": "code", "model": {"providerID": "kilo", "modelID": "moonshotai/kimi-k2.5"}}');
INSERT INTO "message" ("id", "session_id", "time_created", "time_updated", "data") VALUES ('msg_d001de1ef001XHThKuWeQwdwr2', 'ses_2ffe21e3affeR8H1RfNW2u75G1', 1773823451631, 1780588769783, '{"role": "assistant", "time": {"created": 1773823451631, "completed": 1773823451968}, "error": {"name": "APIError", "data": {"message": "eta theta alpha beta gamma delta epsilon zeta eta theta", "statusCode": 402, "isRetryable": false, "responseHeaders": {"cache-control": "delta epsilon zeta", "content-type": "eta", "cross-origin-opener-policy": "eta", "date": "epsilon zeta eta theta alpha beta", "server": "Vercel", "strict-transport-security": "epsilon zeta", "transfer-encoding": "chunked", "vary": "zeta eta theta alpha", "x-content-type-options": "nosniff", "x-frame-options": "SAMEORIGIN", "x-matched-path": "/workspace/file.py", "x-vercel-cache": "MISS", "x-vercel-id": "gamma"}, "responseBody": "gamma delta epsilon zeta eta theta alpha beta gamma delta", "metadata": {"url": "epsilon"}}}, "parentID": "gamma", "modelID": "moonshotai/kimi-k2.5", "providerID": "kilo", "mode": "code", "agent": "code", "path": {"cwd": "/workspace/file.py", "root": "/workspace/file.py"}, "cost": 0, "tokens": {"input": 0, "output": 0, "reasoning": 0, "cache": {"read": 0, "write": 0}}}');

CREATE TABLE IF NOT EXISTS `part` (
	`id` text PRIMARY KEY,
	`message_id` text NOT NULL,
	`session_id` text NOT NULL,
	`time_created` integer NOT NULL,
	`time_updated` integer NOT NULL,
	`data` text NOT NULL,
	CONSTRAINT `fk_part_message_id_message_id_fk` FOREIGN KEY (`message_id`) REFERENCES `message`(`id`) ON DELETE CASCADE
);
INSERT INTO "part" ("id", "message_id", "session_id", "time_created", "time_updated", "data") VALUES ('prt_d001de1c8002jX7Es4oyOWv1HD', 'msg_d001de1c7001oC1QpNyikMLhMk', 'ses_2ffe21e3affeR8H1RfNW2u75G1', 1780588769783, 1780588769783, '{"type": "file", "mime": "text/plain", "filename": "epsilon", "url": "epsilon", "source": {"text": {"value": "zeta", "start": 8, "end": 25}, "type": "file", "path": "delta"}}');
INSERT INTO "part" ("id", "message_id", "session_id", "time_created", "time_updated", "data") VALUES ('prt_d001de1db001dugQxcA2mZBAKK', 'msg_d001de1c7001oC1QpNyikMLhMk', 'ses_2ffe21e3affeR8H1RfNW2u75G1', 1780588769783, 1780588769783, '{"type": "text", "text": "beta gamma delta epsilon zeta eta theta alpha beta gamma", "synthetic": true}');
INSERT INTO "part" ("id", "message_id", "session_id", "time_created", "time_updated", "data") VALUES ('prt_d001de1d00016R27y2Wu3rKPgA', 'msg_d001de1c7001oC1QpNyikMLhMk', 'ses_2ffe21e3affeR8H1RfNW2u75G1', 1780588769783, 1780588769783, '{"type": "text", "text": "alpha beta gamma delta epsilon zeta eta theta alpha", "synthetic": true}');
INSERT INTO "part" ("id", "message_id", "session_id", "time_created", "time_updated", "data") VALUES ('prt_d001de1c8001q3IZZ2DOQ5NKyn', 'msg_d001de1c7001oC1QpNyikMLhMk', 'ses_2ffe21e3affeR8H1RfNW2u75G1', 1780588769783, 1780588769783, '{"type": "text", "text": "alpha beta gamma delta"}');
