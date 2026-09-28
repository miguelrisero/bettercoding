import { describe, expect, it } from 'vitest';

import { retainedPaneWorkspace } from './paneRetention';

describe('retainedPaneWorkspace', () => {
  it('retains the workspace once its pane is shown', () => {
    expect(retainedPaneWorkspace(null, 'a', true)).toBe('a');
  });

  it('keeps the pane while the other pane is shown for the same workspace', () => {
    expect(retainedPaneWorkspace('a', 'a', false)).toBe('a');
  });

  it('mounts nothing for a workspace whose pane was never shown', () => {
    expect(retainedPaneWorkspace(null, 'a', false)).toBeNull();
  });

  it('drops the pane when another workspace is selected with it hidden', () => {
    expect(retainedPaneWorkspace('a', 'b', false)).toBeNull();
  });

  it('moves to the new workspace when it opens with the pane shown', () => {
    expect(retainedPaneWorkspace('a', 'b', true)).toBe('b');
  });

  it('drops the pane when no workspace is selected', () => {
    expect(retainedPaneWorkspace('a', null, false)).toBeNull();
    expect(retainedPaneWorkspace('a', null, true)).toBeNull();
  });
});
