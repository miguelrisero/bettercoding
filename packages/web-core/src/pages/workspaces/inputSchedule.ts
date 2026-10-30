// datetime-local supplies calendar fields; this control explicitly represents UTC.
export function fromUtcControl(value: string): string {
  const date = new Date(`${value}:00.000Z`);
  if (
    !/^(?!0000)\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(value) ||
    !Number.isFinite(date.getTime()) ||
    date.toISOString().slice(0, 16) !== value
  )
    throw new Error('Enter a valid UTC date and time.');
  return date.toISOString();
}

export function toUtcControl(showAt: string | null): string {
  return showAt ? new Date(showAt).toISOString().slice(0, 16) : '';
}
