import type { InputDetail } from 'shared/types';
import type { InputSort } from '@/shared/stores/useUiPreferencesStore';

const RECOMMENDED = /\s*\(recommended\)\s*$/i;

export function splitRecommended(label: string): {
  label: string;
  recommended: boolean;
} {
  return RECOMMENDED.test(label)
    ? { label: label.replace(RECOMMENDED, ''), recommended: true }
    : { label, recommended: false };
}

export function compareInputs(
  a: InputDetail,
  b: InputDetail,
  sort: InputSort
): number {
  const age = Date.parse(a.input.created_at) - Date.parse(b.input.created_at);
  return (
    (sort === 'priority'
      ? a.input.priority.localeCompare(b.input.priority) || age
      : -age) || a.input.id.localeCompare(b.input.id)
  );
}

// Place the current detail in the selected order even after it leaves Pending.
export function nextOpenInput(
  inputs: InputDetail[],
  current: InputDetail,
  sort: InputSort
): string | null {
  current = inputs.find((d) => d.input.id === current.input.id) ?? current;
  const open = inputs
    .filter((d) => d.input.status === 'open' && d.input.id !== current.input.id)
    .sort((a, b) => compareInputs(a, b, sort));
  return (
    open.find((d) => compareInputs(d, current, sort) > 0)?.input.id ??
    open[0]?.input.id ??
    null
  );
}

export const inputStatusLabel: Record<string, string> = {
  open: 'Needs answer',
  answered: 'Answered',
  closed: 'Closed',
  cancelled: 'Cancelled',
};
