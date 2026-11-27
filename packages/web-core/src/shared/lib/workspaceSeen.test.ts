import { describe, expect, it } from 'vitest';
import { shouldMarkWorkspaceSeen } from './workspaceSeen';

const open = (workspaceId: string, hasUnseenActivity = false) => ({
  workspaceId,
  isCreateMode: false,
  hasUnseenActivity,
});

describe('shouldMarkWorkspaceSeen', () => {
  it('marks a workspace seen when the user opens it', () => {
    expect(shouldMarkWorkspaceSeen(null, open('a'))).toBe(true);
    expect(
      shouldMarkWorkspaceSeen(
        { workspaceId: 'a', hasUnseenActivity: false },
        open('b')
      )
    ).toBe(true);
  });

  it('marks the open workspace seen again when its agent finishes', () => {
    expect(
      shouldMarkWorkspaceSeen(
        { workspaceId: 'a', hasUnseenActivity: false },
        open('a', true)
      )
    ).toBe(true);
  });

  it('does not mark again once the unread state clears', () => {
    expect(
      shouldMarkWorkspaceSeen(
        { workspaceId: 'a', hasUnseenActivity: true },
        open('a', false)
      )
    ).toBe(false);
  });

  it('marks nothing without an open workspace', () => {
    expect(
      shouldMarkWorkspaceSeen(null, {
        workspaceId: undefined,
        isCreateMode: false,
        hasUnseenActivity: true,
      })
    ).toBe(false);
    expect(
      shouldMarkWorkspaceSeen(null, {
        workspaceId: 'a',
        isCreateMode: true,
        hasUnseenActivity: true,
      })
    ).toBe(false);
  });
});
