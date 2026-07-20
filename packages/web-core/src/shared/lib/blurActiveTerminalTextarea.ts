export function blurActiveTerminalTextarea(): boolean {
  const activeElement = document.activeElement;
  if (
    !(activeElement instanceof HTMLElement) ||
    !activeElement.classList.contains('xterm-helper-textarea')
  ) {
    return false;
  }

  activeElement.blur();
  return true;
}
