import { describe, expect, it } from 'vitest';

import { retainedCliWorkspace } from './cliPaneRetention';

describe('retainedCliWorkspace', () => {
  it('retains the workspace once its CLI pane is shown', () => {
    expect(retainedCliWorkspace(null, 'a', true)).toBe('a');
  });

  it('keeps the pane while chat is shown for the same workspace', () => {
    expect(retainedCliWorkspace('a', 'a', false)).toBe('a');
  });

  it('mounts nothing for a workspace whose CLI pane was never shown', () => {
    expect(retainedCliWorkspace(null, 'a', false)).toBeNull();
  });

  it('drops the pane when another workspace is selected in chat mode', () => {
    expect(retainedCliWorkspace('a', 'b', false)).toBeNull();
  });

  it('moves to the new workspace when it opens in CLI mode', () => {
    expect(retainedCliWorkspace('a', 'b', true)).toBe('b');
  });

  it('drops the pane when no workspace is selected', () => {
    expect(retainedCliWorkspace('a', null, false)).toBeNull();
    expect(retainedCliWorkspace('a', null, true)).toBeNull();
  });
});
