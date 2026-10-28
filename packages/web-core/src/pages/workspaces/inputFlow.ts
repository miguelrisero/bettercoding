import type { InputDetail } from 'shared/types';

const RECOMMENDED = /\s*\(recommended\)\s*$/i;

export function splitRecommended(label: string): {
  label: string;
  recommended: boolean;
} {
  return RECOMMENDED.test(label)
    ? { label: label.replace(RECOMMENDED, ''), recommended: true }
    : { label, recommended: false };
}

// The next open Input after the current one in list order, wrapping around.
export function nextOpenInput(
  inputs: InputDetail[],
  currentId: string
): string | null {
  const index = inputs.findIndex((d) => d.input.id === currentId);
  const order =
    index < 0
      ? inputs
      : [...inputs.slice(index + 1), ...inputs.slice(0, index)];
  return order.find((d) => d.input.status === 'open')?.input.id ?? null;
}

export const inputStatusLabel: Record<string, string> = {
  open: 'Needs answer',
  answered: 'Answered',
  closed: 'Closed',
  cancelled: 'Cancelled',
};
