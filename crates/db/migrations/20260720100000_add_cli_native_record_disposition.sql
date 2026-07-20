ALTER TABLE cli_native_records
ADD COLUMN disposition TEXT NOT NULL DEFAULT 'renderable'
    CHECK (disposition IN ('renderable', 'bookkeeping', 'sidechain', 'unknown'));
