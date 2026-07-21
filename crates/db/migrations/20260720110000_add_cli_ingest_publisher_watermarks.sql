CREATE TABLE cli_ingest_publisher_watermarks (
    session_id     BLOB PRIMARY KEY,
    published_seq  INTEGER NOT NULL DEFAULT 0 CHECK (published_seq >= 0),
    updated_at     TEXT NOT NULL DEFAULT (datetime('now', 'subsec')),
    FOREIGN KEY (session_id) REFERENCES sessions(id) ON DELETE CASCADE
);
