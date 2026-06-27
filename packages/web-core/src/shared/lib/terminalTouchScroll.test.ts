import { describe, it, expect } from 'vitest';

import {
  createTouchScrollController,
  decideAxis,
  clampToRect,
  WHEEL_STEP_PX,
  AXIS_LOCK_THRESHOLD_PX,
} from './terminalTouchScroll';

function controllerWith(mode: string) {
  const wheels: Array<1 | -1> = [];
  const ctrl = createTouchScrollController({
    getMouseTrackingMode: () => mode,
    dispatchWheel: (dir) => wheels.push(dir),
  });
  return { ctrl, wheels };
}

describe('decideAxis', () => {
  it('stays undecided until movement passes the lock threshold', () => {
    expect(decideAxis(2, 3)).toBe('undecided');
  });
  it('locks vertical when vertical travel dominates', () => {
    expect(decideAxis(2, AXIS_LOCK_THRESHOLD_PX + 5)).toBe('vertical');
  });
  it('locks ignore when horizontal travel dominates', () => {
    expect(decideAxis(AXIS_LOCK_THRESHOLD_PX + 5, 2)).toBe('ignore');
  });
});

describe('clampToRect', () => {
  it('pulls a point outside the rect back inside its bounds', () => {
    const rect = { left: 10, top: 20, right: 110, bottom: 220 };
    expect(clampToRect(0, 0, rect)).toEqual({ x: 11, y: 21 });
    expect(clampToRect(999, 999, rect)).toEqual({ x: 109, y: 219 });
  });
  it('leaves an interior point unchanged', () => {
    const rect = { left: 0, top: 0, right: 100, bottom: 100 };
    expect(clampToRect(50, 60, rect)).toEqual({ x: 50, y: 60 });
  });
});

describe('createTouchScrollController (mouse tracking active)', () => {
  it('translates an upward swipe into floor(travel / step) scroll-down wheels', () => {
    const { ctrl, wheels } = controllerWith('vt200');
    ctrl.onTouchStart({ touches: 1, clientX: 100, clientY: 300 });
    // Cross the axis threshold, then swipe up a total of 3 full steps.
    ctrl.onTouchMove({
      touches: 1,
      clientX: 100,
      clientY: 300 - (AXIS_LOCK_THRESHOLD_PX + 1),
    });
    const r = ctrl.onTouchMove({
      touches: 1,
      clientX: 100,
      clientY: 300 - 3 * WHEEL_STEP_PX,
    });
    expect(wheels).toEqual([1, 1, 1]); // finger up => scroll down
    expect(r.prevent).toBe(true);
  });

  it('translates a downward swipe into scroll-up wheels', () => {
    const { ctrl, wheels } = controllerWith('any');
    ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    ctrl.onTouchMove({
      touches: 1,
      clientX: 0,
      clientY: AXIS_LOCK_THRESHOLD_PX + 1,
    });
    ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 2 * WHEEL_STEP_PX });
    expect(wheels).toEqual([-1, -1]); // finger down => scroll up (history)
  });

  it('prevents default for a committed vertical gesture even before a full step', () => {
    const { ctrl, wheels } = controllerWith('vt200');
    ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    const r = ctrl.onTouchMove({
      touches: 1,
      clientX: 0,
      clientY: -(AXIS_LOCK_THRESHOLD_PX + 1),
    });
    expect(r.prevent).toBe(true);
    expect(wheels.length).toBe(0); // not a full step yet, but gesture is owned
  });
});

describe('createTouchScrollController (mouse tracking inactive)', () => {
  it('never synthesizes wheels and never prevents — xterm native touch scrolls', () => {
    const { ctrl, wheels } = controllerWith('none');
    ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    const r = ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: -100 });
    expect(wheels.length).toBe(0);
    expect(r.prevent).toBe(false);
  });
});

describe('createTouchScrollController (gesture guards)', () => {
  it('ignores multi-touch (pinch) gestures', () => {
    const { ctrl, wheels } = controllerWith('vt200');
    ctrl.onTouchStart({ touches: 2, clientX: 0, clientY: 0 });
    const r = ctrl.onTouchMove({ touches: 2, clientX: 0, clientY: -100 });
    expect(wheels.length).toBe(0);
    expect(r.prevent).toBe(false);
  });

  it('stays ignored after a partial multi-touch lift until all fingers are up', () => {
    const { ctrl, wheels } = controllerWith('vt200');
    ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    // A second finger lands mid-gesture (pinch) — bridge must bail.
    ctrl.onTouchMove({ touches: 2, clientX: 0, clientY: -50 });
    // Lift one finger; one remains down — must NOT resume scrolling.
    ctrl.onTouchEnd(1);
    const stillIgnored = ctrl.onTouchMove({
      touches: 1,
      clientX: 0,
      clientY: -100,
    });
    expect(stillIgnored.prevent).toBe(false);
    expect(wheels.length).toBe(0);
    // All fingers up — the next clean gesture works again.
    ctrl.onTouchEnd(0);
    ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: WHEEL_STEP_PX });
    expect(wheels).toEqual([-1]);
  });

  it('ignores a horizontal swipe', () => {
    const { ctrl, wheels } = controllerWith('vt200');
    ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    const r = ctrl.onTouchMove({
      touches: 1,
      clientX: AXIS_LOCK_THRESHOLD_PX + 50,
      clientY: 1,
    });
    expect(wheels.length).toBe(0);
    expect(r.prevent).toBe(false);
  });

  it('resets between gestures so a new swipe starts clean', () => {
    const { ctrl, wheels } = controllerWith('vt200');
    ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    // Finger down one full step => one scroll-up (-1) wheel.
    ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: WHEEL_STEP_PX });
    ctrl.onTouchEnd();
    // A fresh horizontal gesture must not inherit the previous vertical lock.
    ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    const r = ctrl.onTouchMove({
      touches: 1,
      clientX: AXIS_LOCK_THRESHOLD_PX + 50,
      clientY: 0,
    });
    expect(r.prevent).toBe(false);
    expect(wheels).toEqual([-1]); // only the first gesture's single step
  });
});
