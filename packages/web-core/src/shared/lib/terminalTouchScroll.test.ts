import { describe, expect, it } from 'vitest';

import {
  AXIS_LOCK_THRESHOLD_PX,
  FALLBACK_WHEEL_STEP_PX,
  clampToRect,
  createTouchScrollController,
  decideAxis,
} from './terminalTouchScroll';

interface HarnessOptions {
  mode?: string;
  getLineHeightPx?: () => number;
  omitLineHeightDep?: boolean;
}

interface WheelRecord {
  direction: 1 | -1;
  clientX: number;
  clientY: number;
  t: number;
}

function createHarness({
  mode: initialMode = 'vt200',
  getLineHeightPx = () => 15,
  omitLineHeightDep = false,
}: HarnessOptions = {}) {
  let nowT = 0;
  let mode = initialMode;
  let suppressed = false;
  let nextFrameId = 1;
  const frames = new Map<number, (t: number) => void>();
  const wheels: WheelRecord[] = [];

  const ctrl = createTouchScrollController({
    getMouseTrackingMode: () => mode,
    ...(omitLineHeightDep ? {} : { getLineHeightPx }),
    isSuppressed: () => suppressed,
    now: () => nowT,
    scheduleFrame: (cb) => {
      const id = nextFrameId++;
      frames.set(id, cb);
      return id;
    },
    cancelFrame: (id) => frames.delete(id),
    dispatchWheel: (direction, clientX, clientY) => {
      wheels.push({ direction, clientX, clientY, t: nowT });
    },
  });

  const advance = (dt: number) => {
    nowT += dt;
  };

  const pumpFrame = (dt = 16): number => {
    advance(dt);
    const ready = [...frames.entries()];
    for (const [id] of ready) frames.delete(id);
    const wheelCountBefore = wheels.length;
    for (const [, cb] of ready) cb(nowT);
    return wheels.length - wheelCountBefore;
  };

  const pumpUntilIdle = (maxFrames = 500, dt = 16): number[] => {
    const frameWheelCounts: number[] = [];
    while (frames.size > 0 && frameWheelCounts.length < maxFrames) {
      frameWheelCounts.push(pumpFrame(dt));
    }
    if (frames.size > 0) {
      throw new Error(`momentum did not stop within ${maxFrames} frames`);
    }
    return frameWheelCounts;
  };

  return {
    ctrl,
    wheels,
    advance,
    pumpFrame,
    pumpUntilIdle,
    pendingFrames: () => frames.size,
    setMode: (nextMode: string) => {
      mode = nextMode;
    },
    setSuppressed: (nextSuppressed: boolean) => {
      suppressed = nextSuppressed;
    },
  };
}

function startFlick(
  harness: ReturnType<typeof createHarness>,
  { x = 20, startY = 300 }: { x?: number; startY?: number } = {}
): void {
  harness.ctrl.onTouchStart({ touches: 1, clientX: x, clientY: startY });
  for (const y of [startY - 60, startY - 120, startY - 180]) {
    harness.advance(16);
    harness.ctrl.onTouchMove({ touches: 1, clientX: x, clientY: y });
  }
  harness.ctrl.onTouchEnd();
}

describe('decideAxis', () => {
  it('stays undecided until movement passes the lock threshold', () => {
    expect(decideAxis(2, 3)).toBe('undecided');
  });

  it('locks vertical when vertical travel dominates', () => {
    expect(decideAxis(2, AXIS_LOCK_THRESHOLD_PX + 5)).toBe('vertical');
  });

  it('locks ignore when horizontal travel dominates or travel is tied', () => {
    expect(decideAxis(AXIS_LOCK_THRESHOLD_PX + 5, 2)).toBe('ignore');
    expect(decideAxis(AXIS_LOCK_THRESHOLD_PX, AXIS_LOCK_THRESHOLD_PX)).toBe(
      'ignore'
    );
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

describe('createTouchScrollController (drag tracking)', () => {
  it('tracks 150px of finger travel as 10 rendered 15px rows', () => {
    const harness = createHarness({ getLineHeightPx: () => 15 });
    harness.ctrl.onTouchStart({ touches: 1, clientX: 100, clientY: 300 });

    for (let row = 1; row <= 10; row += 1) {
      harness.advance(16);
      harness.ctrl.onTouchMove({
        touches: 1,
        clientX: 100,
        clientY: 300 - row * 15,
      });
    }

    expect(harness.wheels.map((wheel) => wheel.direction)).toEqual(
      Array(10).fill(1)
    );
  });

  it('translates a downward swipe into scroll-up wheels', () => {
    const harness = createHarness({ getLineHeightPx: () => 15 });
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 30 });
    expect(harness.wheels.map((wheel) => wheel.direction)).toEqual([-1, -1]);
  });

  it('prevents default for a committed vertical gesture before a full row', () => {
    const harness = createHarness({ getLineHeightPx: () => 15 });
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    const result = harness.ctrl.onTouchMove({
      touches: 1,
      clientX: 0,
      clientY: -(AXIS_LOCK_THRESHOLD_PX + 1),
    });
    expect(result.prevent).toBe(true);
    expect(harness.wheels).toHaveLength(0);
  });

  it('samples line height once per gesture', () => {
    let sampleCount = 0;
    const harness = createHarness({
      getLineHeightPx: () => {
        sampleCount += 1;
        return sampleCount === 1 ? 15 : 30;
      },
    });

    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 100 });
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 85 });
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 70 });
    expect(sampleCount).toBe(1);
    expect(harness.wheels).toHaveLength(2);

    harness.ctrl.onTouchEnd();
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 100 });
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 70 });
    expect(sampleCount).toBe(2);
    expect(harness.wheels).toHaveLength(3);
  });

  it.each([
    ['zero', 0],
    ['NaN', Number.NaN],
    ['infinite', Number.POSITIVE_INFINITY],
  ])('falls back to 16px when line height is %s', (_label, lineHeight) => {
    const harness = createHarness({ getLineHeightPx: () => lineHeight });
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 100 });
    harness.ctrl.onTouchMove({
      touches: 1,
      clientX: 0,
      clientY: 100 - (FALLBACK_WHEEL_STEP_PX - 1),
    });
    expect(harness.wheels).toHaveLength(0);
    harness.ctrl.onTouchMove({
      touches: 1,
      clientX: 0,
      clientY: 100 - FALLBACK_WHEEL_STEP_PX,
    });
    expect(harness.wheels.map((wheel) => wheel.direction)).toEqual([1]);
  });

  it('falls back to 16px when the optional line-height dependency is absent', () => {
    const harness = createHarness({ omitLineHeightDep: true });
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 100 });
    harness.ctrl.onTouchMove({
      touches: 1,
      clientX: 0,
      clientY: 100 - (FALLBACK_WHEEL_STEP_PX - 1),
    });
    expect(harness.wheels).toHaveLength(0);
    harness.ctrl.onTouchMove({
      touches: 1,
      clientX: 0,
      clientY: 100 - FALLBACK_WHEEL_STEP_PX,
    });
    expect(harness.wheels.map((wheel) => wheel.direction)).toEqual([1]);
  });

  it.each([
    ['low', 4, 8],
    ['high', 96, 48],
  ])('clamps a %s line height to %ipx', (_label, lineHeight, stepPx) => {
    const harness = createHarness({ getLineHeightPx: () => lineHeight });
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 100 });
    harness.ctrl.onTouchMove({
      touches: 1,
      clientX: 0,
      clientY: 100 - (stepPx - 1),
    });
    expect(harness.wheels).toHaveLength(0);
    harness.ctrl.onTouchMove({
      touches: 1,
      clientX: 0,
      clientY: 100 - stepPx,
    });
    expect(harness.wheels.map((wheel) => wheel.direction)).toEqual([1]);
  });
});

describe('createTouchScrollController (gesture ownership)', () => {
  it('stands down when mouse tracking is inactive', () => {
    const harness = createHarness({ mode: 'none' });
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    harness.advance(16);
    const result = harness.ctrl.onTouchMove({
      touches: 1,
      clientX: 0,
      clientY: -100,
    });
    harness.ctrl.onTouchEnd();
    expect(harness.wheels).toHaveLength(0);
    expect(result.prevent).toBe(false);
    expect(harness.pendingFrames()).toBe(0);
  });

  it('ignores multi-touch gestures', () => {
    const harness = createHarness();
    harness.ctrl.onTouchStart({ touches: 2, clientX: 0, clientY: 0 });
    const result = harness.ctrl.onTouchMove({
      touches: 2,
      clientX: 0,
      clientY: -100,
    });
    expect(harness.wheels).toHaveLength(0);
    expect(result.prevent).toBe(false);
  });

  it('abandons a pinch until every finger lifts, then re-arms', () => {
    const harness = createHarness();
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: -15 });

    // A second touchstart cancels the current gesture and any accumulated state.
    harness.ctrl.onTouchStart({ touches: 2, clientX: 0, clientY: -15 });
    harness.ctrl.onTouchEnd(1);
    const stillIgnored = harness.ctrl.onTouchMove({
      touches: 1,
      clientX: 0,
      clientY: -100,
    });
    expect(stillIgnored.prevent).toBe(false);
    expect(harness.wheels).toHaveLength(1);

    harness.ctrl.onTouchEnd(0);
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 15 });
    expect(harness.wheels.map((wheel) => wheel.direction)).toEqual([1, -1]);
  });

  it('abandons when a multi-touch move arrives without a second touchstart', () => {
    const harness = createHarness();
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    harness.ctrl.onTouchMove({ touches: 2, clientX: 0, clientY: -50 });
    harness.ctrl.onTouchEnd(1);
    const result = harness.ctrl.onTouchMove({
      touches: 1,
      clientX: 0,
      clientY: -100,
    });
    expect(result.prevent).toBe(false);
    expect(harness.wheels).toHaveLength(0);
  });

  it('does not start momentum while a touch remains down', () => {
    const harness = createHarness();
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 300 });
    harness.advance(16);
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 200 });
    harness.ctrl.onTouchEnd(1);
    expect(harness.pendingFrames()).toBe(0);
  });

  it('ignores a horizontal swipe', () => {
    const harness = createHarness();
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    const result = harness.ctrl.onTouchMove({
      touches: 1,
      clientX: AXIS_LOCK_THRESHOLD_PX + 50,
      clientY: 1,
    });
    expect(harness.wheels).toHaveLength(0);
    expect(result.prevent).toBe(false);
  });

  it('resets between gestures so a new swipe starts clean', () => {
    const harness = createHarness();
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 15 });
    harness.ctrl.onTouchEnd();

    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    const result = harness.ctrl.onTouchMove({
      touches: 1,
      clientX: AXIS_LOCK_THRESHOLD_PX + 50,
      clientY: 0,
    });
    expect(result.prevent).toBe(false);
    expect(harness.wheels.map((wheel) => wheel.direction)).toEqual([-1]);
  });

  it('stands down for the full sequence once suppression takes ownership', () => {
    const harness = createHarness();
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    harness.setSuppressed(true);
    const result = harness.ctrl.onTouchMove({
      touches: 1,
      clientX: 0,
      clientY: 45,
    });
    expect(result.prevent).toBe(false);
    expect(harness.wheels).toHaveLength(0);

    harness.setSuppressed(false);
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 90 });
    expect(harness.wheels).toHaveLength(0);

    harness.ctrl.onTouchEnd();
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 0 });
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 15 });
    expect(harness.wheels.map((wheel) => wheel.direction)).toEqual([-1]);
  });
});

describe('createTouchScrollController (momentum)', () => {
  it('does not start momentum after a fast drag is held still for 200ms', () => {
    const harness = createHarness();
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 300 });
    harness.advance(16);
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 240 });
    harness.advance(16);
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 180 });
    const wheelsAtLift = harness.wheels.length;

    harness.advance(200);
    harness.ctrl.onTouchEnd();
    for (let frame = 0; frame < 10; frame += 1) harness.pumpFrame();

    expect(harness.wheels).toHaveLength(wheelsAtLift);
    expect(harness.pendingFrames()).toBe(0);
  });

  it('starts momentum when the latest fast-drag sample is 99ms old', () => {
    const harness = createHarness();
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 300 });
    harness.advance(16);
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 240 });
    harness.advance(16);
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 180 });
    const dragWheelCount = harness.wheels.length;

    harness.advance(99);
    harness.ctrl.onTouchEnd();
    harness.pumpFrame();

    expect(harness.wheels.length).toBeGreaterThan(dragWheelCount);
  });

  it('does not start momentum after a slow 0.2px/ms release', () => {
    const harness = createHarness();
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 100 });
    harness.advance(50);
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 90 });
    harness.advance(50);
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 80 });
    const wheelsAtLift = harness.wheels.length;

    harness.ctrl.onTouchEnd();
    for (let frame = 0; frame < 10; frame += 1) harness.pumpFrame();
    expect(harness.wheels).toHaveLength(wheelsAtLift);
    expect(harness.pendingFrames()).toBe(0);
  });

  it('continues a flick with a decaying tail and then terminates', () => {
    const harness = createHarness();
    startFlick(harness, { x: 37 });
    const dragWheelCount = harness.wheels.length;

    const frameWheelCounts = harness.pumpUntilIdle();
    const postLiftWheels = harness.wheels.slice(dragWheelCount);
    expect(postLiftWheels.length).toBeGreaterThanOrEqual(65);
    expect(postLiftWheels.length).toBeLessThanOrEqual(80);
    expect(postLiftWheels.every((wheel) => wheel.direction === 1)).toBe(true);
    expect(
      postLiftWheels.every(
        (wheel) => wheel.clientX === 37 && wheel.clientY === 120
      )
    ).toBe(true);
    expect(harness.pendingFrames()).toBe(0);

    // Wheel output over equal ten-frame windows follows the monotonic velocity decay.
    const fullBuckets: number[] = [];
    for (let i = 0; i + 10 <= frameWheelCounts.length; i += 10) {
      fullBuckets.push(
        frameWheelCounts.slice(i, i + 10).reduce((sum, count) => sum + count, 0)
      );
    }
    expect(fullBuckets.length).toBeGreaterThan(3);
    for (let i = 1; i < fullBuckets.length; i += 1) {
      expect(fullBuckets[i]).toBeLessThanOrEqual(fullBuckets[i - 1]);
    }
  });

  it('cancels the tail on re-grab and requires a full row of new travel', () => {
    const harness = createHarness();
    startFlick(harness);
    harness.pumpFrame();
    harness.pumpFrame();
    harness.pumpFrame();
    const wheelsBeforeGrab = harness.wheels.length;
    expect(harness.pendingFrames()).toBe(1);

    harness.ctrl.onTouchStart({ touches: 1, clientX: 50, clientY: 200 });
    expect(harness.pendingFrames()).toBe(0);
    for (let frame = 0; frame < 10; frame += 1) harness.pumpFrame();
    expect(harness.wheels).toHaveLength(wheelsBeforeGrab);

    harness.advance(16);
    harness.ctrl.onTouchMove({ touches: 1, clientX: 50, clientY: 186 });
    expect(harness.wheels).toHaveLength(wheelsBeforeGrab);
    harness.advance(16);
    harness.ctrl.onTouchMove({ touches: 1, clientX: 50, clientY: 185 });
    expect(harness.wheels).toHaveLength(wheelsBeforeGrab + 1);
  });

  it('halts mid-tail when suppression becomes active', () => {
    const harness = createHarness();
    startFlick(harness);
    harness.pumpFrame();
    harness.pumpFrame();
    const wheelsBeforeSuppression = harness.wheels.length;

    harness.setSuppressed(true);
    harness.pumpFrame();
    expect(harness.wheels).toHaveLength(wheelsBeforeSuppression);
    expect(harness.pendingFrames()).toBe(0);
    harness.pumpFrame();
    expect(harness.wheels).toHaveLength(wheelsBeforeSuppression);
  });

  it('halts mid-tail when mouse tracking turns off', () => {
    const harness = createHarness();
    startFlick(harness);
    harness.pumpFrame();
    harness.pumpFrame();
    const wheelsBeforeModeChange = harness.wheels.length;

    harness.setMode('none');
    harness.pumpFrame();
    expect(harness.wheels).toHaveLength(wheelsBeforeModeChange);
    expect(harness.pendingFrames()).toBe(0);
    harness.pumpFrame();
    expect(harness.wheels).toHaveLength(wheelsBeforeModeChange);
  });

  it('clamps absurd velocity, drains at most six wheels per tick, and respects the cap', () => {
    const tailForDistance = (distance: number) => {
      const harness = createHarness({ getLineHeightPx: () => 8 });
      harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 1000 });
      harness.advance(8);
      harness.ctrl.onTouchMove({
        touches: 1,
        clientX: 0,
        clientY: 1000 - distance,
      });
      const dragWheelCount = harness.wheels.length;
      harness.ctrl.onTouchEnd();
      const frameWheelCounts = harness.pumpUntilIdle();
      return {
        postLiftWheelCount: harness.wheels.length - dragWheelCount,
        frameWheelCounts,
      };
    };

    const atVelocityCap = tailForDistance(28); // 28px / 8ms = 3.5px/ms
    const absurdVelocity = tailForDistance(1000);
    expect(absurdVelocity.postLiftWheelCount).toBe(
      atVelocityCap.postLiftWheelCount
    );
    expect(absurdVelocity.postLiftWheelCount).toBeLessThanOrEqual(150);
    expect(Math.max(...absurdVelocity.frameWheelCounts)).toBeLessThanOrEqual(6);
  });

  it('touchcancel resets a fast drag without starting momentum', () => {
    const harness = createHarness();
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 300 });
    harness.advance(16);
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 240 });
    harness.advance(16);
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 180 });
    const wheelsAtCancel = harness.wheels.length;

    harness.ctrl.onTouchCancel();
    expect(harness.pendingFrames()).toBe(0);
    for (let frame = 0; frame < 10; frame += 1) harness.pumpFrame();
    expect(harness.wheels).toHaveLength(wheelsAtCancel);

    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 100 });
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 86 });
    expect(harness.wheels).toHaveLength(wheelsAtCancel);
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 85 });
    expect(harness.wheels).toHaveLength(wheelsAtCancel + 1);
  });

  it('derives momentum from one coalesced 100px move over 50ms', () => {
    const harness = createHarness({ getLineHeightPx: () => 16 });
    harness.ctrl.onTouchStart({ touches: 1, clientX: 0, clientY: 100 });
    harness.advance(50);
    harness.ctrl.onTouchMove({ touches: 1, clientX: 0, clientY: 0 });
    const dragWheelCount = harness.wheels.length;
    harness.ctrl.onTouchEnd();

    harness.pumpFrame();
    expect(harness.wheels.length).toBeGreaterThan(dragWheelCount);
  });
});
