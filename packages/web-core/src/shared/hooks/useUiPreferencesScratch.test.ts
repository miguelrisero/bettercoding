import { describe, expect, it } from 'vitest';
import { resolvePersistedMainPaneMode } from './useUiPreferencesScratch';

describe('resolvePersistedMainPaneMode', () => {
  it('flips everything to cli before the one-time migration marker', () => {
    // Pre-migration payloads: persisted "chat" was the old default, not a
    // user choice — the rollout turns CLI mode on for all workspaces.
    expect(resolvePersistedMainPaneMode('chat', false)).toBe('cli');
    expect(resolvePersistedMainPaneMode('cli', false)).toBe('cli');
    expect(resolvePersistedMainPaneMode(undefined, false)).toBe('cli');
    expect(resolvePersistedMainPaneMode(null, false)).toBe('cli');
  });

  it('honors explicit chat after the migration marker is set', () => {
    expect(resolvePersistedMainPaneMode('chat', true)).toBe('chat');
    expect(resolvePersistedMainPaneMode('cli', true)).toBe('cli');
  });

  it('defaults missing or unknown values to cli after migration', () => {
    expect(resolvePersistedMainPaneMode(undefined, true)).toBe('cli');
    expect(resolvePersistedMainPaneMode(null, true)).toBe('cli');
    expect(resolvePersistedMainPaneMode('garbage', true)).toBe('cli');
  });
});
