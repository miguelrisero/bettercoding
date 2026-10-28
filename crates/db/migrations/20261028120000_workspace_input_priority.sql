ALTER TABLE workspace_inputs ADD COLUMN priority TEXT NOT NULL DEFAULT 'P2' CHECK(priority IN ('P0','P1','P2','P3','P4'));
