export function handleTerminalCommandError(
  message: { type: 'error' | 'input_rejected'; message?: string },
  release: () => void,
  display: (text: string) => void
): void {
  if (message.type === 'error') release();
  display(`\r\n\x1b[31m${message.message ?? 'terminal error'}\x1b[0m\r\n`);
}
