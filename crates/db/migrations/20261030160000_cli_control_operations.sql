CREATE TABLE cli_control_operations (
    operation_id BLOB PRIMARY KEY NOT NULL,
    workspace_id BLOB NOT NULL REFERENCES workspaces(id),
    request_id TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    request_json TEXT NOT NULL,
    binding_id BLOB NOT NULL,
    phase TEXT NOT NULL CHECK (phase IN ('reserved', 'command_sent', 'awaiting_confirmation', 'awaiting_applied', 'applied', 'cancelled', 'reconcile_required')),
    pre_receipt_id TEXT,
    confirmation_receipt_id TEXT,
    confirmation_principal TEXT,
    post_receipt_id TEXT,
    UNIQUE(workspace_id, request_id)
);
CREATE UNIQUE INDEX cli_control_one_unresolved ON cli_control_operations(workspace_id)
    WHERE phase NOT IN ('applied', 'cancelled');
CREATE TABLE cli_control_receipts (
    receipt_id TEXT PRIMARY KEY NOT NULL,
    operation_id BLOB NOT NULL REFERENCES cli_control_operations(operation_id),
    receipt_json TEXT NOT NULL
);

-- Guard every binding mutation, including direct handover SQL and discovery.
-- Exact-identity discovery remains valid while the operation holds custody.
CREATE TRIGGER cli_control_binding_update BEFORE UPDATE ON cli_pane_bindings
WHEN (OLD.id IS NOT NEW.id OR OLD.workspace_id IS NOT NEW.workspace_id
   OR OLD.session_id IS NOT NEW.session_id OR OLD.claude_session_id IS NOT NEW.claude_session_id
   OR OLD.released_at IS NOT NEW.released_at OR OLD.bound_via IS NOT NEW.bound_via)
 AND EXISTS (SELECT 1 FROM cli_control_operations o
             WHERE o.workspace_id IN (OLD.workspace_id, NEW.workspace_id)
               AND o.phase NOT IN ('applied', 'cancelled'))
BEGIN SELECT RAISE(ABORT, 'CLI control operation requires reconciliation'); END;
CREATE TRIGGER cli_control_binding_insert BEFORE INSERT ON cli_pane_bindings
WHEN EXISTS (SELECT 1 FROM cli_control_operations o WHERE o.workspace_id = NEW.workspace_id
             AND o.phase NOT IN ('applied', 'cancelled'))
BEGIN SELECT RAISE(ABORT, 'CLI control operation requires reconciliation'); END;
CREATE TRIGGER cli_control_binding_delete BEFORE DELETE ON cli_pane_bindings
WHEN EXISTS (SELECT 1 FROM cli_control_operations o WHERE o.workspace_id = OLD.workspace_id
             AND o.phase NOT IN ('applied', 'cancelled'))
BEGIN SELECT RAISE(ABORT, 'CLI control operation requires reconciliation'); END;
CREATE TRIGGER cli_control_spawn_insert BEFORE INSERT ON workspace_spawn_reservations
WHEN EXISTS (SELECT 1 FROM cli_control_operations o WHERE o.workspace_id = NEW.workspace_id
             AND o.phase NOT IN ('applied', 'cancelled'))
BEGIN SELECT RAISE(ABORT, 'CLI control operation requires reconciliation'); END;

CREATE TRIGGER cli_control_immutable_request BEFORE UPDATE ON cli_control_operations
WHEN OLD.operation_id IS NOT NEW.operation_id OR OLD.workspace_id IS NOT NEW.workspace_id
  OR OLD.request_id IS NOT NEW.request_id OR OLD.request_digest IS NOT NEW.request_digest
  OR OLD.request_json IS NOT NEW.request_json OR OLD.binding_id IS NOT NEW.binding_id
BEGIN SELECT RAISE(ABORT, 'CLI control request identity is immutable'); END;
