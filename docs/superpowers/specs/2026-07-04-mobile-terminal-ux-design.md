# Mobile Terminal UX — Design

**Date:** 2026-07-04
**Status:** Approved (decisions settled interactively with Miguel; he is away during implementation)
**Builds on:** PR #22 (touch→wheel scroll bridge, mobile Copy/Paste/Keyboard controls, OSC 52 clipboard)

## Problem

The terminal console (xterm.js hosting the tmux-backed `claude` CLI session, plus
plain shell side terminals) is still close to unusable on phones:

1. When the on-screen keyboard opens, nothing resizes or scrolls — the keyboard
   covers the bottom half of the terminal, including claude's input line.
2. There is no way to select text by touch (xterm.js has no touch selection).
3. There is no way to send Esc, Tab, Shift+Tab, Ctrl combos, or arrow keys —
   mobile soft keyboards simply don't have them.

**Platforms:** iOS Safari (primary, plain web + home-screen PWA), Android Chrome
(secondary). Desktop behavior must remain byte-for-byte unchanged: everything
below is gated on touch capability (`useIsTouchDevice` / `isTouchDevice`, same
gates PR #22 used).

**Scope:** `XTermInstance` and its satellites in `packages/web-core` — so it
applies to both the CLI main pane and shell side terminals automatically.

## Decisions (settled with Miguel)

| Topic | Decision |
|---|---|
| Keyboard handling | Shrink + refit: container tracks `visualViewport`, xterm refits, PTY/tmux resizes; input line always visible |
| Key bar | `esc ⇥ ⇧⇥ ^C ctrl ← ↓ ↑ → ⏎` with sticky/latching ctrl; fixed set, no settings UI |
| Gestures | Long-press + drag = arrow D-pad (3 speed tiers); double-tap = Tab; three-finger tap = paste. **No pinch zoom.** |
| Text selection | Explicit select-mode toggle button in the mobile controls (gestures/scroll bridge pause while active) |
| Font size | A+ / A− stepper buttons in mobile controls, persisted in localStorage |
| Delivery | PR + merge via ship-it light; **no /opt deploy** |

## Design

### 1. Keyboard-aware viewport (`useVisualViewportHeight` + layout wiring)

New hook in `packages/web-core/src/shared/hooks/`: subscribes to
`window.visualViewport` `resize` + `scroll` events (SSR-safe, `null` /
no-op when `visualViewport` is unavailable or the device is not
touch-capable). It exposes the current visible height.

Wiring: the terminal wrapper (in `XTermInstance`'s outer container's ancestor —
the workspace mobile layout) caps its height to the visual viewport when the
keyboard is open, instead of `h-screen`'s layout-viewport height. The existing
`ResizeObserver` + debounced `fitTerminal` in `XTermInstance` then does the
refit + WS resize automatically — no new resize plumbing.

Details:

- `interactive-widget=resizes-content` added to the viewport meta in both
  `packages/local-web/index.html` and `packages/remote-web/index.html` —
  Android Chrome then resizes the layout viewport natively; the hook remains
  the iOS path (and a no-op safety net on Android).
- On keyboard open (viewport shrink) and on terminal focus:
  `term.scrollToBottom()` so the prompt is visible immediately.
- Compensate iOS Safari's scroll-the-page-on-focus behavior: when
  `visualViewport.offsetTop > 0`, scroll the window back so the app chrome
  stays pinned (best-effort; keep it simple and defensive).
- The key bar (below) renders inside this sized container, bottom-anchored, so
  it naturally sits directly above the system keyboard.

### 2. Key bar (`TerminalKeyBar.tsx`)

Touch-only component (returns `null` off-touch), rendered by `XTermInstance`
under the terminal viewport, always visible on touch devices (no fragile
keyboard-open detection; it doubles as the tap-target to focus the terminal).
Horizontally scrollable single row, 44px-high touch targets:

`esc` `⇥` `⇧⇥` `^C` `ctrl` `←` `↓` `↑` `→` `⏎`

Sequences sent through the existing single data pipe (`terminal.input(...)`,
which flows through `onData` → `conn.send`):

- `esc` → `\x1b`; `⇥` → `\t`; `⇧⇥` → `\x1b[Z` (CSI Z); `^C` → `\x03`;
  `⏎` → `\r`.
- Arrows respect application cursor keys mode
  (`terminal.modes.applicationCursorKeysMode`): `\x1bOA…` vs `\x1b[A…`.
- Buttons must not steal focus from the terminal (`preventDefault` on
  pointer/mouse down) so the system keyboard stays open.

**Sticky ctrl:** tapping `ctrl` latches it (visually highlighted). The next
single printable character arriving on the data pipe is transformed to its
control code (`char & 0x1f`), then ctrl unlatches. Tapping `ctrl` again while
latched cancels. Implemented as a pure transform function
(`applyStickyCtrl(data, latched) → {out, stillLatched}`) inserted in the
`onData` → `send` path in `XTermInstance`, driven by shared state
(ref + subscription) owned by the key bar. Multi-char bursts (paste, IME
commits) pass through untouched and unlatch — only a single-char chunk is
transformed. Unit-tested.

### 3. Gesture layer (`terminalTouchGestures.ts`)

Same architecture as `terminalTouchScroll.ts`: a pure, unit-testable gesture
controller (state machine consuming `{type, touches, timestamp}` events and
emitting actions) plus a thin `install` function that binds real touch events
and executes actions. Coordinated with the existing scroll bridge:

- **Long-press + drag = D-pad.** Finger down and held ≥ ~350 ms within a small
  slop radius enters D-pad mode (a subtle overlay indicator appears). Dragging
  past a threshold sends repeated arrow keys in the dominant axis direction;
  repeat rate has 3 tiers by drag distance (Termius-style "gears"; roughly
  ~110 ms / ~55 ms / ~25 ms per keystroke). Lifting the finger exits. While in
  D-pad mode the scroll bridge is suppressed.
- **Movement before the long-press threshold** = scroll, handled by the
  existing touch-scroll bridge exactly as today.
- **Double-tap = Tab.** Two taps within ~250 ms and tap-slop distance send
  `\t`. Single taps still focus the terminal as before (tap behavior itself is
  unchanged — we only add the second-tap interpretation).
- **Three-finger tap = paste.** Same clipboard read path + feedback as the
  Paste button (including the availability guards from PR #22).
- Arrow sequences honor application cursor keys mode (shared helper with the
  key bar).
- Any multi-touch that isn't a clean three-finger tap keeps today's "ignore
  until all fingers lift" rule.

### 4. Select mode (toggle in `TerminalMobileControls`)

New toggle button (e.g. text-cursor icon) in the existing mobile controls:

- **On:** gesture layer + touch-scroll bridge are disabled. Touch drag maps to
  xterm buffer selection: translate touch coords → cell coords using the
  `.xterm-screen` bounding box and `cols`/`rows`, drive
  `terminal.select(col, row, length)` across the drag (line-spanning
  selections included). The existing `onSelectionChange` handler already
  copies selections to the clipboard; the Copy button also picks them up.
  A status flash ("Select mode — drag to select") explains the state.
- **Off (default):** everything behaves as designed above. Toggling off clears
  the selection.
- No synthetic mouse events are forwarded to tmux — selection is purely
  client-side over the visible buffer, which sidesteps mouse-tracking entirely.
- This is the riskiest item (Miguel: skippable if it breaks things). It is
  last in the build order and isolated behind its toggle so it can be dropped
  without touching the rest.

### 5. Font size steppers

`A−` / `A+` buttons in `TerminalMobileControls`: adjust
`terminal.options.fontSize` within 8–20 (default 12), call `fitTerminal`-style
refit (fit + WS resize), persist to `localStorage`
(`vk-terminal-mobile-font-size`). On touch devices, terminal creation reads the
persisted value; desktop keeps the hardcoded 12.

## Error handling

- All new listeners/timers install once per created terminal (PR #22 pattern:
  live with the element, die on dispose) and must not leak across reattach.
- Clipboard paths reuse PR #22's guarded helpers (distinct
  unavailable/blocked/empty feedback).
- `visualViewport` absent → hook returns null → layout falls back to current
  behavior (no crash, desktop unchanged).
- Repeat timers (D-pad) must be cancelled on touchend/touchcancel/dispose.

## Testing

- Vitest (run via `npx vitest`, per repo convention) for the pure parts:
  gesture controller state machine (long-press vs scroll vs double-tap vs
  three-finger disambiguation, speed tiers, cancellation), sticky-ctrl
  transform, arrow-sequence selection by cursor mode, touch→cell coordinate
  mapping, font-size clamp/persistence logic.
- `pnpm run check` + `pnpm run lint` green.
- Manual smoke on device after merge (Miguel; /opt deploy handled separately
  via ship-it, not in this change).

## Build order

1. Keyboard-aware viewport + meta tag (issue #1 — biggest pain, standalone).
2. Key bar + sticky ctrl (issue #3).
3. Gesture layer (D-pad, double-tap Tab, three-finger paste).
4. Font steppers.
5. Select mode (riskiest, optional, isolated).
