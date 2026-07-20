import type { Terminal } from '@xterm/xterm';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import type { ArrowKey } from './terminalKeySequences';
import { getTerminalMobileState } from './terminalMobileState';
import {
  BADGE_OFFSET_PX,
  createTouchGestureController,
  DEMOTE_GUARD_MS,
  dpadBadgeText,
  dpadDirection,
  dpadZone,
  dpadZoneWithHysteresis,
  DOUBLE_TAP_MS,
  DPAD_DEAD_ZONE_PX,
  DPAD_ZONES,
  installTerminalTouchGestures,
  LONG_PRESS_MS,
  MIN_ARROW_INTERVAL_MS,
  TAP_SLOP_PX,
  ZONE_HYSTERESIS_PX,
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

describe('dpadZone', () => {
  it('uses half-open distance boundaries', () => {
    expect(dpadZone(DPAD_DEAD_ZONE_PX)).toBe(1);
    expect(dpadZone(47)).toBe(1);
    expect(dpadZone(48)).toBe(2);
    expect(dpadZone(87)).toBe(2);
    expect(dpadZone(88)).toBe(3);
    expect(dpadZone(127)).toBe(3);
    expect(dpadZone(128)).toBe(4);
    expect(dpadZone(10_000)).toBe(4);
  });
});

describe('dpadZoneWithHysteresis', () => {
  it('enters zones above and leaves zones below their hysteresis bands', () => {
    expect(
      dpadZoneWithHysteresis(DPAD_DEAD_ZONE_PX + ZONE_HYSTERESIS_PX - 1, 0)
    ).toBe(0);
    expect(
      dpadZoneWithHysteresis(DPAD_DEAD_ZONE_PX + ZONE_HYSTERESIS_PX, 0)
    ).toBe(1);

    for (const zone of [2, 3, 4] as const) {
      const boundary = DPAD_ZONES[zone - 1].minDistance;
      expect(
        dpadZoneWithHysteresis(boundary + ZONE_HYSTERESIS_PX - 1, zone - 1)
      ).toBe(zone - 1);
      expect(
        dpadZoneWithHysteresis(boundary + ZONE_HYSTERESIS_PX, zone - 1)
      ).toBe(zone);
      expect(dpadZoneWithHysteresis(boundary - ZONE_HYSTERESIS_PX, zone)).toBe(
        zone
      );
      expect(
        dpadZoneWithHysteresis(boundary - ZONE_HYSTERESIS_PX - 1, zone)
      ).toBe(zone - 1);
    }

    expect(
      dpadZoneWithHysteresis(DPAD_DEAD_ZONE_PX - ZONE_HYSTERESIS_PX, 1)
    ).toBe(1);
    expect(
      dpadZoneWithHysteresis(DPAD_DEAD_ZONE_PX - ZONE_HYSTERESIS_PX - 1, 1)
    ).toBe(0);
  });

  it('can cross multiple zones in one sample', () => {
    expect(dpadZoneWithHysteresis(10_000, 0)).toBe(4);
    expect(dpadZoneWithHysteresis(0, 4)).toBe(0);
  });
});

describe('dpadBadgeText', () => {
  it('repeats ASCII direction glyphs once per zone', () => {
    const cases: Array<[ArrowKey, string]> = [
      ['left', '<'],
      ['right', '>'],
      ['up', '^'],
      ['down', 'v'],
    ];

    for (const [dir, glyph] of cases) {
      for (let zone = 1; zone <= 4; zone += 1) {
        expect(dpadBadgeText(dir, zone)).toBe(glyph.repeat(zone));
      }
    }
  });

  it('shows a plus in the dead zone regardless of the distance zone', () => {
    for (let zone = 1; zone <= 4; zone += 1) {
      expect(dpadBadgeText(null, zone)).toBe('+');
    }
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

  it('fires exactly once on zone-1 entry and never repeats', () => {
    const { ctrl, arrows } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    const t0 = LONG_PRESS_MS + 10;
    ctrl.onTouchMove(
      p(100, 100 + DPAD_DEAD_ZONE_PX + ZONE_HYSTERESIS_PX + 1),
      t0
    );
    expect(arrows).toEqual(['down']);
    expect(ctrl.nextTimerAt()).toBeNull();
    ctrl.onTimer(t0 + 10_000);
    expect(arrows).toEqual(['down']);
    expect(ctrl.nextTimerAt()).toBeNull();
  });

  it('pumps one arrow per dead-zone to zone-1 re-engagement', () => {
    const { ctrl, arrows } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    const t0 = LONG_PRESS_MS + 10;
    const engagedY = 100 + DPAD_DEAD_ZONE_PX + ZONE_HYSTERESIS_PX + 1;
    const deadY = 100 + DPAD_DEAD_ZONE_PX - ZONE_HYSTERESIS_PX - 1;
    ctrl.onTouchMove(p(100, engagedY), t0);
    ctrl.onTouchMove(p(100, deadY), t0 + MIN_ARROW_INTERVAL_MS / 2);
    ctrl.onTouchMove(p(100, engagedY), t0 + MIN_ARROW_INTERVAL_MS);
    ctrl.onTouchMove(p(100, deadY), t0 + MIN_ARROW_INTERVAL_MS * 1.5);
    ctrl.onTouchMove(p(100, engagedY), t0 + MIN_ARROW_INTERVAL_MS * 2);
    expect(arrows).toEqual(['down', 'down', 'down']);
    expect(ctrl.nextTimerAt()).toBeNull();
  });

  it('repeats zone 2 every 1000ms', () => {
    const { ctrl, arrows } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    const t0 = LONG_PRESS_MS + 10;
    ctrl.onTouchMove(p(100, 164), t0);
    expect(DPAD_ZONES[1].repeatMs).toBe(1_000);
    expect(arrows).toEqual(['down']);
    expect(ctrl.nextTimerAt()).toBe(t0 + 1_000);
    ctrl.onTimer(t0 + 999);
    expect(arrows).toEqual(['down']);
    runTimersUntil(ctrl, t0 + 2_000);
    expect(arrows).toEqual(['down', 'down', 'down']);
  });

  it('fires immediately on an outward jump to zone 4, then every 250ms', () => {
    const { ctrl, arrows } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    const t0 = LONG_PRESS_MS + 10;
    ctrl.onTouchMove(
      p(100, 100 + DPAD_DEAD_ZONE_PX + ZONE_HYSTERESIS_PX + 1),
      t0
    );
    const outwardAt = t0 + MIN_ARROW_INTERVAL_MS;
    ctrl.onTouchMove(p(100, 264), outwardAt);
    expect(DPAD_ZONES[3].repeatMs).toBe(250);
    expect(arrows).toEqual(['down', 'down']);
    expect(ctrl.nextTimerAt()).toBe(outwardAt + 250);
    runTimersUntil(ctrl, outwardAt + 500);
    expect(arrows).toEqual(['down', 'down', 'down', 'down']);
  });

  it('retreats from zone 4 to zone 2 without an immediate arrow', () => {
    const { ctrl, arrows } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    const t0 = LONG_PRESS_MS + 10;
    ctrl.onTouchMove(p(100, 264), t0);
    expect(ctrl.nextTimerAt()).toBe(t0 + 250);

    ctrl.onTouchMove(p(100, 164), t0 + 100);
    expect(arrows).toEqual(['down']);
    // Keep the sooner zone-4 deadline, then adopt zone 2's slower cadence.
    expect(ctrl.nextTimerAt()).toBe(t0 + 250);
    ctrl.onTimer(t0 + 250);
    expect(arrows).toEqual(['down', 'down']);
    expect(ctrl.nextTimerAt()).toBe(t0 + 1_250);
  });

  it('stops repeats when retreating to zone 1', () => {
    const { ctrl, arrows } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    const t0 = LONG_PRESS_MS + 10;
    ctrl.onTouchMove(
      p(100, 100 + DPAD_ZONES[2].minDistance + ZONE_HYSTERESIS_PX + 1),
      t0
    );
    expect(ctrl.nextTimerAt()).not.toBeNull();

    ctrl.onTouchMove(p(100, 120), t0 + 100);
    expect(arrows).toEqual(['down']);
    expect(ctrl.nextTimerAt()).toBeNull();
    ctrl.onTimer(t0 + 10_000);
    expect(arrows).toEqual(['down']);
  });

  it('fires on a direction change and restarts the current zone cadence', () => {
    const { ctrl, arrows } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    const t0 = LONG_PRESS_MS + 10;
    ctrl.onTouchMove(p(100, 170), t0);
    ctrl.onTouchMove(p(170, 100), t0 + 400);
    expect(arrows).toEqual(['down', 'right']);
    expect(ctrl.nextTimerAt()).toBe(t0 + 1_400);
    ctrl.onTimer(t0 + 1_000);
    expect(arrows).toEqual(['down', 'right']);
    ctrl.onTimer(t0 + 1_400);
    expect(arrows).toEqual(['down', 'right', 'right']);
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

describe('D-pad hysteresis and dispatch floor', () => {
  it('absorbs 20 jitter moves around every zone boundary', () => {
    const { ctrl, arrows } = harness();

    for (const [index, zone] of ([2, 3, 4] as const).entries()) {
      const startedAt = index * 5_000;
      const boundary = DPAD_ZONES[zone - 1].minDistance;
      ctrl.onTouchStart(p(100, 100), startedAt);
      runTimersUntil(ctrl, startedAt + LONG_PRESS_MS);
      const engagedAt = startedAt + LONG_PRESS_MS + 10;
      ctrl.onTouchMove(
        p(100, 100 + boundary + ZONE_HYSTERESIS_PX + 1),
        engagedAt
      );

      for (let move = 0; move < 20; move += 1) {
        const distance = boundary + (move % 2 === 0 ? -5 : 5);
        ctrl.onTouchMove(p(100, 100 + distance), engagedAt + (move + 1) * 5);
      }

      expect(arrows).toHaveLength(index + 1);
      ctrl.onTouchEnd(0, engagedAt + 110);
    }
  });

  it('does not re-arm on dead-zone-edge thumb jitter', () => {
    const { ctrl, arrows } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    const engagedAt = LONG_PRESS_MS + 10;
    ctrl.onTouchMove(
      p(100, 100 + DPAD_DEAD_ZONE_PX + ZONE_HYSTERESIS_PX + 1),
      engagedAt
    );

    for (let move = 0; move < 20; move += 1) {
      const distance = DPAD_DEAD_ZONE_PX + (move % 2 === 0 ? -5 : 5);
      ctrl.onTouchMove(p(100, 100 + distance), engagedAt + (move + 1) * 10);
    }

    expect(arrows).toEqual(['down']);
  });

  it('holds the incumbent axis through a near-45-degree drag', () => {
    const { ctrl, arrows } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    const engagedAt = LONG_PRESS_MS + 10;
    ctrl.onTouchMove(p(130, 128), engagedAt);

    for (let move = 0; move < 10; move += 1) {
      const [dx, dy] = move % 2 === 0 ? [28, 30] : [30, 28];
      ctrl.onTouchMove(p(100 + dx, 100 + dy), engagedAt + (move + 1) * 150);
    }

    expect(arrows).toEqual(['right']);
    ctrl.onTouchMove(p(120, 131), engagedAt + 1_650);
    expect(arrows).toEqual(['right', 'down']);
  });

  it('drops an arrow dispatched inside the global rate floor', () => {
    const { ctrl, arrows, dpad } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    const engagedAt = LONG_PRESS_MS + 10;
    ctrl.onTouchMove(p(100, 130), engagedAt);
    ctrl.onTouchMove(p(140, 100), engagedAt + MIN_ARROW_INTERVAL_MS - 1);

    expect(arrows).toEqual(['down']);
    expect(dpad.at(-1)).toEqual({ active: true, dir: 'right' });

    ctrl.onTouchMove(p(60, 100), engagedAt + MIN_ARROW_INTERVAL_MS);
    expect(arrows).toEqual(['down', 'left']);
  });
});

describe('scroll handoff', () => {
  it('accepts a drag whose event timestamp proves the dwell', () => {
    const { ctrl, arrows, dpad } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    const r = ctrl.onTouchMove(p(100, 130), 400);
    expect(r.prevent).toBe(true);
    expect(dpad[0]).toEqual({ active: true, dir: null });
    expect(arrows).toEqual(['down']);
  });

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

  it('stays ignored after an early swipe moves again past the deadline', () => {
    const { ctrl, arrows, dpad } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    expect(ctrl.onTouchMove(p(100, 100 + TAP_SLOP_PX + 5), 100)).toEqual({
      prevent: false,
    });
    expect(ctrl.onTouchMove(p(100, 160), 400)).toEqual({ prevent: false });
    expect(dpad).toEqual([]);
    expect(arrows).toEqual([]);
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

  it('does not paste after the sequence promoted and fired D-pad arrows', () => {
    const { ctrl, arrows, events } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    ctrl.onTouchMove(p(100, 140), LONG_PRESS_MS + 10);
    ctrl.onTouchStart(p(120, 100, 2), LONG_PRESS_MS + 20);
    ctrl.onTouchStart(p(140, 100, 3), LONG_PRESS_MS + 30);
    ctrl.onTouchEnd(2, LONG_PRESS_MS + 80);
    ctrl.onTouchEnd(1, LONG_PRESS_MS + 90);
    ctrl.onTouchEnd(0, LONG_PRESS_MS + 100);

    expect(arrows).toEqual(['down']);
    expect(events).not.toContain('paste');
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
  it('promotes from timestamped dwell evidence when the timer never fired', () => {
    const { ctrl, arrows, dpad } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    // No onTimer call at all — an in-slop sample proves the dwell, then an
    // outward move can safely engage a direction.
    expect(ctrl.onTouchMove(p(100, 100 + TAP_SLOP_PX), 380).prevent).toBe(true);
    const outward = ctrl.onTouchMove(p(100, 140), 420);
    expect(outward.prevent).toBe(true);
    expect(dpad[0]).toEqual({ active: true, dir: null });
    expect(arrows).toEqual(['down']);
  });

  it('demotes timer promotion when the next move predates the deadline', () => {
    const { ctrl, arrows, dpad } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    ctrl.onTimer(400);
    expect(dpad).toEqual([{ active: true, dir: null }]);

    const r = ctrl.onTouchMove(p(100, 100 + TAP_SLOP_PX + 5), 140);
    expect(r.prevent).toBe(false);
    expect(dpad).toEqual([
      { active: true, dir: null },
      { active: false, dir: null },
    ]);
    expect(arrows).toEqual([]);
  });

  it('keeps demotion armed across an innocent queued in-slop move', () => {
    const { ctrl, arrows, dpad } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    ctrl.onTimer(400);

    expect(ctrl.onTouchMove(p(100, 105), 100, 400)).toEqual({
      prevent: true,
    });
    const result = ctrl.onTouchMove(p(100, 180), 200, 400);

    expect(result).toEqual({ prevent: false });
    expect(arrows).toEqual([]);
    expect(dpad.at(-1)).toEqual({ active: false, dir: null });
    expect(ctrl.nextTimerAt()).toBeNull();
  });

  it('keeps a near-deadline drag in D-pad mode inside the guard band', () => {
    const { ctrl, arrows, dpad } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    ctrl.onTimer(400);
    const dragAt = LONG_PRESS_MS - Math.floor(DEMOTE_GUARD_MS / 3);

    const result = ctrl.onTouchMove(p(100, 180), dragAt, 400);

    expect(result).toEqual({ prevent: true });
    expect(arrows).toEqual(['down']);
    expect(dpad.at(-1)).toEqual({ active: true, dir: 'down' });
  });

  it('keeps timer promotion after an in-slop post-deadline move', () => {
    const { ctrl, arrows, dpad } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    ctrl.onTimer(400);

    const r = ctrl.onTouchMove(p(100, 100 + TAP_SLOP_PX), 400);
    expect(r.prevent).toBe(true);
    expect(dpad).toEqual([{ active: true, dir: null }]);
    expect(arrows).toEqual([]);
  });

  it('still hands an early move to the scroll bridge', () => {
    const { ctrl, dpad } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    const r = ctrl.onTouchMove(p(100, 160), LONG_PRESS_MS - 50);
    expect(r.prevent).toBe(false);
    expect(dpad).toEqual([]);
  });

  it('recovers queued pre-deadline touchends as double-tap input', () => {
    const { ctrl, events } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    ctrl.onTimer(400);
    ctrl.onTouchEnd(0, 120);

    ctrl.onTouchStart(p(100, 100), 200);
    ctrl.onTimer(600);
    ctrl.onTouchEnd(0, 320);

    expect(events).toEqual(['tab']);
  });

  it('schedules repeats from delivery time, not a stale move timestamp', () => {
    const { ctrl, arrows } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    ctrl.onTimer(1_000);
    ctrl.onTouchMove(p(100, 240), 400, 1_000);

    expect(arrows).toEqual(['down']);
    expect(ctrl.nextTimerAt()).toBe(1_250);
    ctrl.onTimer(1_000);
    expect(arrows).toEqual(['down']);
  });

  it('cancel() releases an active D-pad and stops repeats', () => {
    const { ctrl, arrows, dpad } = harness();
    ctrl.onTouchStart(p(100, 100), 0);
    runTimersUntil(ctrl, LONG_PRESS_MS);
    ctrl.onTouchMove(p(100, 170), LONG_PRESS_MS + 10);
    expect(ctrl.nextTimerAt()).not.toBeNull();
    ctrl.cancel();
    expect(dpad.at(-1)).toEqual({ active: false, dir: null });
    expect(ctrl.nextTimerAt()).toBeNull();
    ctrl.onTimer(10_000);
    expect(arrows).toEqual(['down']);
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

type CapturedTouchListener = EventListenerOrEventListenerObject;

class FakeTouchEventTarget {
  private readonly listeners = new Map<string, Set<CapturedTouchListener>>();

  addEventListener(type: string, listener: CapturedTouchListener) {
    const listeners = this.listeners.get(type) ?? new Set();
    listeners.add(listener);
    this.listeners.set(type, listeners);
  }

  removeEventListener(type: string, listener: CapturedTouchListener) {
    this.listeners.get(type)?.delete(listener);
  }

  dispatch(type: string, event: TouchEvent) {
    for (const listener of [...(this.listeners.get(type) ?? [])]) {
      if (typeof listener === 'function') {
        listener(event);
      } else {
        listener.handleEvent(event);
      }
    }
  }

  listenerCount(type: string) {
    return this.listeners.get(type)?.size ?? 0;
  }
}

class FakeOverlay {
  className = '';
  textContent: string | null = null;
  readonly style: Record<string, string> = {};
  readonly offsetWidth = 40;
  readonly offsetHeight = 28;
  readonly remove = vi.fn();
}

class FakeTerminalElement extends FakeTouchEventTarget {
  overlay: FakeOverlay | null = null;

  constructor(readonly bounds = { left: 0, top: 0, width: 390, height: 600 }) {
    super();
  }

  appendChild(overlay: FakeOverlay) {
    this.overlay = overlay;
  }

  contains(target: unknown) {
    return target === this || target === this.overlay;
  }

  getBoundingClientRect() {
    const { left, top, width, height } = this.bounds;
    return {
      left,
      top,
      width,
      height,
      x: left,
      y: top,
      right: left + width,
      bottom: top + height,
      toJSON: () => ({}),
    };
  }
}

class FakeDocument extends FakeTouchEventTarget {
  readonly createElement = vi.fn(() => new FakeOverlay());
}

interface FakeTouch {
  identifier: number;
  clientX: number;
  clientY: number;
}

function touch(
  identifier: number,
  clientX: number,
  clientY: number
): FakeTouch {
  return { identifier, clientX, clientY };
}

function touchEvent({
  target,
  targetTouches,
  touches = targetTouches,
  changedTouches = targetTouches,
  timeStamp = performance.now(),
}: {
  target: unknown;
  targetTouches: FakeTouch[];
  touches?: FakeTouch[];
  changedTouches?: FakeTouch[];
  timeStamp?: number;
}): TouchEvent {
  return {
    target,
    targetTouches,
    touches,
    changedTouches,
    timeStamp,
    cancelable: true,
    preventDefault: vi.fn(),
  } as unknown as TouchEvent;
}

const adapterDisposers: Array<() => void> = [];

function adapterHarness(bounds = { left: 0, top: 0, width: 390, height: 600 }) {
  const documentTarget = new FakeDocument();
  const element = new FakeTerminalElement(bounds);
  vi.stubGlobal('document', documentTarget);
  const terminal = { element } as unknown as Terminal;
  const arrows: ArrowKey[] = [];
  const events: string[] = [];
  const dispose = installTerminalTouchGestures(terminal, {
    sendArrow: (direction) => arrows.push(direction),
    sendTab: () => events.push('tab'),
    paste: () => events.push('paste'),
  });
  adapterDisposers.push(dispose);

  return {
    terminal,
    element,
    documentTarget,
    overlay: element.overlay!,
    arrows,
    events,
    dispatchTerminal(type: string, event: TouchEvent) {
      element.dispatch(type, event);
    },
  };
}

describe('terminal touch gesture DOM adapter', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    for (const dispose of adapterDisposers.splice(0)) dispose();
    vi.clearAllTimers();
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it('ends the D-pad when its terminal touch lifts before an outside touch', async () => {
    const harness = adapterHarness();
    const primary = touch(1, 100, 100);
    harness.dispatchTerminal(
      'touchstart',
      touchEvent({
        target: harness.element,
        targetTouches: [primary],
        changedTouches: [primary],
        timeStamp: 0,
      })
    );
    await vi.advanceTimersByTimeAsync(LONG_PRESS_MS);
    await vi.advanceTimersByTimeAsync(MIN_ARROW_INTERVAL_MS);
    const draggedPrimary = touch(1, 100, 170);
    harness.dispatchTerminal(
      'touchmove',
      touchEvent({
        target: harness.element,
        targetTouches: [draggedPrimary],
        changedTouches: [draggedPrimary],
      })
    );
    expect(getTerminalMobileState(harness.terminal).dpadActive).toBe(true);
    expect(harness.arrows).toEqual(['down']);

    const outside = touch(2, 20, 20);
    harness.dispatchTerminal(
      'touchend',
      touchEvent({
        target: harness.element,
        targetTouches: [],
        touches: [outside],
        changedTouches: [draggedPrimary],
      })
    );

    expect(getTerminalMobileState(harness.terminal).dpadActive).toBe(false);
    expect(harness.documentTarget.listenerCount('touchstart')).toBe(0);
    await vi.advanceTimersByTimeAsync(10_000);
    expect(harness.arrows).toEqual(['down']);
  });

  it('cancels an active D-pad when a touch starts elsewhere in the document', async () => {
    const harness = adapterHarness();
    const primary = touch(1, 100, 100);
    harness.dispatchTerminal(
      'touchstart',
      touchEvent({
        target: harness.element,
        targetTouches: [primary],
        changedTouches: [primary],
        timeStamp: 0,
      })
    );
    await vi.advanceTimersByTimeAsync(LONG_PRESS_MS);
    expect(harness.documentTarget.listenerCount('touchstart')).toBe(1);

    const outsideTarget = {};
    const outside = touch(2, 20, 20);
    harness.documentTarget.dispatch(
      'touchstart',
      touchEvent({
        target: outsideTarget,
        targetTouches: [outside],
        touches: [primary, outside],
        changedTouches: [outside],
      })
    );

    expect(getTerminalMobileState(harness.terminal).dpadActive).toBe(false);
    expect(harness.documentTarget.listenerCount('touchstart')).toBe(0);
    expect(harness.overlay.style.display).toBe('none');
  });

  it('positions the badge opposite every engaged drag direction', async () => {
    const harness = adapterHarness();
    const primary = touch(1, 200, 200);
    harness.dispatchTerminal(
      'touchstart',
      touchEvent({
        target: harness.element,
        targetTouches: [primary],
        changedTouches: [primary],
        timeStamp: 0,
      })
    );
    await vi.advanceTimersByTimeAsync(LONG_PRESS_MS);

    expect(harness.overlay.style.cssText).toContain('pointer-events:none');
    expect(harness.overlay.style.left).toBe('180px');
    expect(harness.overlay.style.top).toBe(
      `${200 - BADGE_OFFSET_PX - harness.overlay.offsetHeight}px`
    );

    for (const [x, y, expectedLeft, expectedTop] of [
      [200, 130, 180, 200 + BADGE_OFFSET_PX],
      [200, 270, 180, 200 - BADGE_OFFSET_PX - 28],
      [130, 200, 200 + BADGE_OFFSET_PX, 186],
      [270, 200, 200 - BADGE_OFFSET_PX - 40, 186],
    ] as const) {
      await vi.advanceTimersByTimeAsync(MIN_ARROW_INTERVAL_MS);
      const movedPrimary = touch(1, x, y);
      harness.dispatchTerminal(
        'touchmove',
        touchEvent({
          target: harness.element,
          targetTouches: [movedPrimary],
          changedTouches: [movedPrimary],
        })
      );
      expect(harness.overlay.style.left).toBe(`${expectedLeft}px`);
      expect(harness.overlay.style.top).toBe(`${expectedTop}px`);
    }
  });

  it('flips the neutral badge before clamping at the top edge', async () => {
    const harness = adapterHarness({
      left: 0,
      top: 0,
      width: 200,
      height: 200,
    });
    const primary = touch(1, 100, 20);
    harness.dispatchTerminal(
      'touchstart',
      touchEvent({
        target: harness.element,
        targetTouches: [primary],
        changedTouches: [primary],
        timeStamp: 0,
      })
    );
    await vi.advanceTimersByTimeAsync(LONG_PRESS_MS);

    expect(harness.overlay.style.top).toBe(`${20 + BADGE_OFFSET_PX}px`);
    expect(
      Number.parseFloat(harness.overlay.style.left)
    ).toBeGreaterThanOrEqual(0);
    expect(
      Number.parseFloat(harness.overlay.style.top) +
        harness.overlay.offsetHeight
    ).toBeLessThanOrEqual(200);
  });

  it('flips a horizontal badge before clamping at the side edge', async () => {
    const harness = adapterHarness({
      left: 0,
      top: 0,
      width: 200,
      height: 200,
    });
    const primary = touch(1, 20, 100);
    harness.dispatchTerminal(
      'touchstart',
      touchEvent({
        target: harness.element,
        targetTouches: [primary],
        changedTouches: [primary],
        timeStamp: 0,
      })
    );
    await vi.advanceTimersByTimeAsync(LONG_PRESS_MS);
    await vi.advanceTimersByTimeAsync(MIN_ARROW_INTERVAL_MS);
    const movedPrimary = touch(1, 90, 100);
    harness.dispatchTerminal(
      'touchmove',
      touchEvent({
        target: harness.element,
        targetTouches: [movedPrimary],
        changedTouches: [movedPrimary],
      })
    );

    expect(harness.overlay.style.left).toBe(`${20 + BADGE_OFFSET_PX}px`);
    expect(
      Number.parseFloat(harness.overlay.style.left) +
        harness.overlay.offsetWidth
    ).toBeLessThanOrEqual(200);
  });
});
