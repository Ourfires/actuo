//! Schema migrations, applied in order. `PRAGMA user_version` is the number applied.
//! Never edit a migration that has shipped; add a new one.

pub const MIGRATIONS: &[&str] = &[
    // 1: the schema.
    r#"
    CREATE TABLE projects (
        id          TEXT PRIMARY KEY,
        root        TEXT UNIQUE NOT NULL,
        created_at  TEXT NOT NULL
    );

    CREATE TABLE sessions (
        id                   TEXT PRIMARY KEY,
        project_id           TEXT NOT NULL REFERENCES projects(id),
        agent                TEXT NOT NULL,
        agent_version        TEXT,
        level                TEXT NOT NULL,
        clean                INTEGER NOT NULL,
        cwd                  TEXT NOT NULL,
        worktree_path        TEXT,
        branch               TEXT,
        acp_session_id       TEXT,
        can_resume           INTEGER,
        can_load             INTEGER,
        config_json          TEXT NOT NULL DEFAULT '{}',
        commands_json        TEXT,
        agent_pid            INTEGER,
        agent_started_at     TEXT,
        state                TEXT NOT NULL,
        end_reason           TEXT,
        title                TEXT,
        head_seq             INTEGER NOT NULL DEFAULT 0,
        created_at           TEXT NOT NULL,
        updated_at           TEXT NOT NULL,
        base                 TEXT,
        sandbox_json         TEXT,
        web_tools            INTEGER NOT NULL DEFAULT 1,
        telemetry            INTEGER NOT NULL DEFAULT 1,
        config_options_json  TEXT
    );
    CREATE INDEX sessions_by_project ON sessions(project_id);

    CREATE TABLE turns (
        session_id     TEXT NOT NULL REFERENCES sessions(id),
        n              INTEGER NOT NULL,
        text           TEXT NOT NULL,
        started_at     TEXT NOT NULL,
        completed_at   TEXT,
        stop_reason    TEXT,
        tokens_json    TEXT,
        cost_amount    REAL,
        cost_currency  TEXT,
        PRIMARY KEY (session_id, n)
    );

    CREATE TABLE checkpoints (
        session_id  TEXT NOT NULL REFERENCES sessions(id),
        n           INTEGER NOT NULL,
        commit_sha  TEXT NOT NULL,
        ref         TEXT NOT NULL,
        files_json  TEXT,
        created_at  TEXT NOT NULL,
        PRIMARY KEY (session_id, n)
    );

    CREATE TABLE requests (
        session_id      TEXT NOT NULL REFERENCES sessions(id),
        request_id      TEXT NOT NULL,
        turn            INTEGER NOT NULL,
        item_id         TEXT,
        tool_call_json  TEXT NOT NULL,
        options_json    TEXT NOT NULL,
        opened_at       TEXT NOT NULL,
        resolved_at     TEXT,
        outcome_json    TEXT,
        resolved_by     TEXT,
        PRIMARY KEY (session_id, request_id)
    );

    CREATE TABLE events (
        session_id  TEXT NOT NULL REFERENCES sessions(id),
        seq         INTEGER NOT NULL,
        ts          TEXT NOT NULL,
        type        TEXT NOT NULL,
        turn        INTEGER,
        item_id     TEXT,
        data_json   TEXT NOT NULL,
        raw_json    TEXT,
        PRIMARY KEY (session_id, seq)
    ) WITHOUT ROWID;

    CREATE TABLE runs (
        id                     TEXT PRIMARY KEY,
        repo                   TEXT NOT NULL,
        issue                  INTEGER NOT NULL,
        issue_url              TEXT NOT NULL,
        title                  TEXT,
        agent                  TEXT NOT NULL,
        level                  TEXT NOT NULL,
        config_json            TEXT NOT NULL DEFAULT '{}',
        limits_json            TEXT NOT NULL,
        state                  TEXT NOT NULL,
        reason                 TEXT,
        step                   TEXT NOT NULL,
        session_id             TEXT REFERENCES sessions(id),
        branch                 TEXT NOT NULL,
        base                   TEXT,
        pr_number              INTEGER,
        pr_url                 TEXT,
        status_comment_id      INTEGER,
        started_by             TEXT NOT NULL,
        previous_run_id        TEXT REFERENCES runs(id),
        check_rounds           INTEGER NOT NULL DEFAULT 0,
        reject_turns           INTEGER NOT NULL DEFAULT 0,
        restart_turns          INTEGER NOT NULL DEFAULT 0,
        last_prompt_at         TEXT,
        pending_comments_json  TEXT,
        created_at             TEXT NOT NULL,
        updated_at             TEXT NOT NULL,
        ended_at               TEXT,
        started_via            TEXT NOT NULL DEFAULT 'label',
        reason_text            TEXT
    );
    CREATE INDEX runs_by_state ON runs(state);
    -- One run at a time per issue, until it ends.
    CREATE UNIQUE INDEX runs_open ON runs(repo, issue)
        WHERE state NOT IN ('pr_opened', 'merged', 'closed', 'failed', 'cancelled');

    CREATE TABLE run_steps (
        run_id          TEXT NOT NULL REFERENCES runs(id),
        n               INTEGER NOT NULL,
        kind            TEXT NOT NULL,
        command         TEXT,
        exit_code       INTEGER,
        output_tail     TEXT,
        log_path        TEXT,
        started_at      TEXT NOT NULL,
        ended_at        TEXT,
        diagnosis_json  TEXT,
        PRIMARY KEY (run_id, n)
    );

    CREATE TABLE run_states (
        run_id  TEXT NOT NULL REFERENCES runs(id),
        n       INTEGER NOT NULL,
        state   TEXT NOT NULL,
        reason  TEXT,
        by      TEXT,
        at      TEXT NOT NULL,
        via     TEXT,
        PRIMARY KEY (run_id, n)
    );

    CREATE TABLE agent_options (
        agent          TEXT PRIMARY KEY,
        version        TEXT,
        own_json       TEXT NOT NULL,
        by_model_json  TEXT NOT NULL,
        probed_at      TEXT NOT NULL
    );

    CREATE TABLE check_runs (
        id          TEXT PRIMARY KEY,
        repo        TEXT NOT NULL,
        state       TEXT NOT NULL,
        started_at  TEXT NOT NULL,
        json        TEXT NOT NULL
    );
    CREATE INDEX check_runs_by_repo ON check_runs(repo, started_at);
    "#,
];
