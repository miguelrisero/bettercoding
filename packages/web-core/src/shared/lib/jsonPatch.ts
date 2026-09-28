import { applyPatch, Pointer, type Operation } from 'rfc6902';

export function applyUpsertPatch(target: object, ops: Operation[]): void {
  ops.forEach((op) => {
    const [error] = applyPatch(target, [op]);

    if (op.op === 'replace' && error?.name === 'MissingError') {
      applyPatch(target, [{ ...op, op: 'add' }]);
    }
  });
}

/**
 * Apply every operation or throw. For streams whose patches address exact
 * array indexes, a failed operation means the local copy has drifted from the
 * server and must be resynchronised rather than patched further.
 *
 * Besides the errors `rfc6902` reports, an `add` past the end of an array
 * fails, as RFC 6902 requires (`rfc6902` would silently append instead).
 */
export function applyPatchStrict(target: object, ops: Operation[]): void {
  for (const op of ops) {
    if (op.op === 'add') {
      const { parent, key } = Pointer.fromJSON(op.path).evaluate(target);
      if (Array.isArray(parent) && key !== '-' && Number(key) > parent.length) {
        throw new Error(`add past the end of an array: ${op.path}`);
      }
    }
    const [error] = applyPatch(target, [op]);
    if (error) {
      throw error;
    }
  }
}
