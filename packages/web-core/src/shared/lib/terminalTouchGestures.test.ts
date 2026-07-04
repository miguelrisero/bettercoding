import { describe, it, expect } from 'vitest';

import type { ArrowKey } from './terminalKeySequences';
import {
  createTouchGestureController,
  dpadDirection,
  dpadInterval,
  DOUBLE_TAP_MS,
  DPAD_DEAD_ZONE_PX,
  DPAD_TIERS,
  LONG_PRESS_MS,
  TAP_SLOP_PX,
} from './terminalTouchGestures';

function harness({ enabled = true } = {}) {
  const arrows: ArrowKey[] = [];
  const events: string[] = [];
  const dpad: Array<{ active: boolean; dir: ArrowKey | null }> = [];
  const ctrl = createTouchGestureController({
    isEnabled: () => enabled,
    sendArrow: (d) => {
      arrows.push(d);
      events.push(`arrow:${d}`);
    },
    sendTab: () => events.push('tab'),
    paste: () => events.push('paste'),
    setDpad: (active, dir) => dpad.push({ active, dir }),
  });
  return { ctrl, arrows, events, dpad };
}

const p = (x: number, y: number, touches = 1) => ({
  touches,
  clientX: x,
  clientY: y,
});

/** Drive timers exactly as the adapter would: run onTimer at nextTimerAt. */
function runTimersUntil(
  ctrl: ReturnType<typeof createTouchGestureController>,
  until: number
) {
  for (;;) {
    const at = ctrl.nextTimerAt();
    if (at === null || at > until) return;
    ctrl.onTimer(at);
  }
}

describe('dpadDirection', () => {
  it('is null inside the dead zone', () => {
    expect(dpadDirection(DPAD_DEAD_ZONE_PX - 1, 0)).toBeNull();
    expect(dpadDirection(0, -(DPAD_DEAD_ZONE_PX - 1))).toBeNull();
  });

  it('picks the dominant axis', () => {
    expect(dpadDirection(40, 10)).toBe('right');
    expect(dpadDirection(-40, 10)).toBe('left');
    expect(dpadDirection(10, 40)).toBe('down');
    expect(dpadDirection(10, -40)).toBe('up');
  });
});

describe('dpadInterval', () => {
  it('maps distance to the three tiers', () => {
    expect(dpadInterval(20)).toBe(DPAD_TIERS[0].interval);
    expect(dpadInterval(DPAD_TIERS[0].dist)).toBe(DPAD_TIERS[1].interval);
    expect(dpadInterval(DPAD_TIERS[1].dist + 100)).toBe(DPAD_TIERS[2].interval);
  });
});

describe('long-press D-pad', () => {
  it('promotes to D-pad after the long-press delay without movement', () => {
    const { ctrl, dpad } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    expect(ctrl.nextTimerAt()).toBe(LONG_PRESS_MS);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    expect(dpad).toEqual([{ active: true, dir: null }]);
  });

  it('sends an arrow immediately on engaging a direction, then repeats', () => {
    const { ctrl, arrows } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    // Drag down past the dead zone → immediate arrow.
    ctrl.onTouchMove(
      p(100, 100 + DPAD_DEAD_ZONE_PX + 5, 1),
      LONG_PRESS_MS + 10
    );
    expect(arrows).toEqual(['down']);
    // Repeats fire on the timer at tier-1 cadence.
    runTimersUntil(ctrl, LONG_PRESS_MS + 10 + 2 * DPAD_TIERS[0].interval);
    expect(arrows).toEqual(['down', 'down', 'down']);
  });

  it('speeds up in higher tiers and pauses in the dead zone', () => {
    const { ctrl, arrows } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    const t0 = LONG_PRESS_MS + 10;
    ctrl.onTouchMove(p(100, 100 + DPAD_TIERS[1].dist + 20), t0);
    expect(arrows).toEqual(['down']);
    // Fastest tier cadence.
    runTimersUntil(ctrl, t0 + DPAD_TIERS[2].interval);
    expect(arrows).toEqual(['down', 'down']);
    // Back inside the dead zone → repeats stop.
    ctrl.onTouchMove(p(100, 102), t0 + DPAD_TIERS[2].interval + 1);
    expect(ctrl.nextTimerAt()).toBeNull();
  });

  it('fires immediately again on direction change', () => {
    const { ctrl, arrows } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    ctrl.onTouchMove(p(100, 140), LONG_PRESS_MS + 5);
    ctrl.onTouchMove(p(160, 100), LONG_PRESS_MS + 20);
    expect(arrows).toEqual(['down', 'right']);
  });

  it('prevents default while in D-pad mode', () => {
    const { ctrl } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    expect(ctrl.onTouchMove(p(100, 103), LONG_PRESS_MS + 5).prevent).toBe(true);
  });

  it('releases the D-pad when the finger lifts', () => {
    const { ctrl, dpad } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    ctrl.onTouchEnd(0, LONG_PRESS_MS + 50);
    expect(dpad.at(-1)).toEqual({ active: false, dir: null });
    expect(ctrl.nextTimerAt()).toBeNull();
  });

  it('cancels the D-pad when a second finger lands', () => {
    const { ctrl, dpad } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    ctrl.onTouchStart(p(120, 100, 2), LONG_PRESS_MS + 10);
    expect(dpad.at(-1)).toEqual({ active: false, dir: null });
  });
});

describe('scroll handoff', () => {
  it('stands down when the finger moves before the long-press delay', () => {
    const { ctrl, dpad, events } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    const r = ctrl.onTouchMove(p(100, 100 + TAP_SLOP_PX + 5), 50);
    expect(r.prevent).toBe(false);
    expect(ctrl.nextTimerAt()).toBeNull(); // no long-press pending anymore
    runTimersUntil(ctrl, 10_000);
    ctrl.onTouchEnd(0, 120);
    expect(dpad).toEqual([]);
    expect(events).toEqual([]);
  });
});

describe('double-tap = Tab', () => {
  it('sends Tab on two quick taps in place', () => {
    const { ctrl, events } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    ctrl.onTouchEnd(0, 40);
    ctrl.onTouchStart(p(103, 98), 120);
    ctrl.onTouchEnd(0, 160);
    expect(events).toEqual(['tab']);
  });

  it('does not fire when the taps are too far apart in time or space', () => {
    const { ctrl, events } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    ctrl.onTouchEnd(0, 40);
    ctrl.onTouchStart(p(100, 100), 40 + DOUBLE_TAP_MS + 50);
    ctrl.onTouchEnd(0, 40 + DOUBLE_TAP_MS + 90);
    ctrl.onTouchStart(p(300, 300), 40 + DOUBLE_TAP_MS + 200);
    ctrl.onTouchEnd(0, 40 + DOUBLE_TAP_MS + 240);
    expect(events).toEqual([]);
  });

  it('a triple tap yields exactly one Tab', () => {
    const { ctrl, events } = harness();
    for (const [start, end] of [
      [0, 30],
      [100, 130],
      [200, 230],
    ] as const) {
      ctrl.onTouchStart(p(100, 100), start);
      ctrl.onTouchEnd(0, end);
    }
    expect(events).toEqual(['tab']);
  });

  it('a long-press is not a tap', () => {
    const { ctrl, events } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    ctrl.onTouchEnd(0, LONG_PRESS_MS + 20);
    ctrl.onTouchStart(p(100, 100), LONG_PRESS_MS + 60);
    ctrl.onTouchEnd(0, LONG_PRESS_MS + 90);
    expect(events).toEqual([]);
  });
});

describe('three-finger tap = paste', () => {
  it('pastes when three fingers tap quickly', () => {
    const { ctrl, events } = harness();
    ctrl.onTouchStart(p(100, 100, 1), 0);
    ctrl.onTouchStart(p(120, 100, 2), 10);
    ctrl.onTouchStart(p(140, 100, 3), 20);
    ctrl.onTouchEnd(2, 80);
    ctrl.onTouchEnd(1, 90);
    ctrl.onTouchEnd(0, 100);
    expect(events).toEqual(['paste']);
  });

  it('does not paste on a two-finger gesture (pinch)', () => {
    const { ctrl, events } = harness();
    ctrl.onTouchStart(p(100, 100, 1), 0);
    ctrl.onTouchStart(p(120, 100, 2), 10);
    ctrl.onTouchEnd(1, 80);
    ctrl.onTouchEnd(0, 90);
    expect(events).toEqual([]);
  });

  it('does not paste when the fingers dwell too long', () => {
    const { ctrl, events } = harness();
    ctrl.onTouchStart(p(100, 100, 1), 0);
    ctrl.onTouchStart(p(120, 100, 2), 10);
    ctrl.onTouchStart(p(140, 100, 3), 20);
    ctrl.onTouchEnd(0, 2_000);
    expect(events).toEqual([]);
  });
});

describe('select mode (disabled layer)', () => {
  it('ignores the whole touch sequence when disabled', () => {
    const { ctrl, events, dpad } = harness({ enabled: false });
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, 10_000);
    ctrl.onTouchEnd(0, 40);
    ctrl.onTouchStart(p(100, 100), 100);
    ctrl.onTouchEnd(0, 140);
    expect(events).toEqual([]);
    expect(dpad).toEqual([]);
  });
});

describe('timer starvation + cancellation (council round 1)', () => {
  it('promotes to D-pad inside onTouchMove when the timer never fired', () => {
    const { ctrl, arrows, dpad } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    // No onTimer call at all — simulates a starved main thread. First move
    // arrives well past the long-press threshold.
    const r = ctrl.onTouchMove(
      p(100, 100 + DPAD_DEAD_ZONE_PX + 10),
      LONG_PRESS_MS + 200
    );
    expect(r.prevent).toBe(true);
    expect(dpad[0]).toEqual({ active: true, dir: null });
    expect(arrows).toEqual(['down']);
  });

  it('still hands an early move to the scroll bridge', () => {
    const { ctrl, dpad } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    const r = ctrl.onTouchMove(p(100, 160), LONG_PRESS_MS - 50);
    expect(r.prevent).toBe(false);
    expect(dpad).toEqual([]);
  });

  it('cancel() releases an active D-pad and stops repeats', () => {
    const { ctrl, dpad } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    ctrl.onTouchMove(p(100, 150), LONG_PRESS_MS + 10);
    ctrl.cancel();
    expect(dpad.at(-1)).toEqual({ active: false, dir: null });
    expect(ctrl.nextTimerAt()).toBeNull();
    // A stray touchend after cancel is a no-op.
    ctrl.onTouchEnd(0, LONG_PRESS_MS + 100);
    expect(dpad.at(-1)).toEqual({ active: false, dir: null });
  });
});

describe('coalesced multi-touch + cancellation semantics (review round 1)', () => {
  it('three-finger tap works when all fingers land in ONE touchstart', () => {
    const { ctrl, events } = harness();
    // Previous gesture long ago — pressAt must not leak from it.
    ctrl.onTouchStart(p(50, 50), 0);
    ctrl.onTouchEnd(0, 30);
    ctrl.onTouchStart(p(100, 100, 3), 10_000);
    ctrl.onTouchEnd(0, 10_080);
    expect(events).toEqual(['paste']);
  });

  it('coalesced multi-start respects the select-mode gate', () => {
    const { ctrl, events } = harness({ enabled: false });
    ctrl.onTouchStart(p(100, 100, 3), 0);
    ctrl.onTouchEnd(0, 80);
    expect(events).toEqual([]);
  });

  it('a travelling three-finger gesture (system swipe) does not paste', () => {
    const { ctrl, events } = harness();
    ctrl.onTouchStart(p(100, 100, 3), 0);
    ctrl.onTouchMove(p(100 + 3 * TAP_SLOP_PX, 100, 3), 40);
    ctrl.onTouchEnd(0, 90);
    expect(events).toEqual([]);
  });

  it('cancel() wipes double-tap history — a cancelled tap is not tap #1', () => {
    const { ctrl, events } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    ctrl.onTouchEnd(0, 30); // clean tap #1
    ctrl.onTouchStart(p(100, 100), 100);
    ctrl.cancel(); // browser claimed the second touch
    ctrl.onTouchStart(p(100, 100), 200);
    ctrl.onTouchEnd(0, 230);
    expect(events).toEqual([]); // cancelled sequence must not complete a pair
  });
});
