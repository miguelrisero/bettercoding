export function blurActiveTerminalTextarea(): boolean {
  const activeElement = document.activeElement;
  // `xterm-helper-textarea` is xterm.js INTERNAL DOM, not public API, and is
  // pinned to the xterm version in package.json. If an upgrade renames it,
  // this deliberately narrow focus workaround becomes a silent no-op.
  if (
    !(activeElement instanceof HTMLElement) ||
    !activeElement.classList.contains('xterm-helper-textarea')
  ) {
    return false;
  }

  activeElement.blur();
  return true;
}
