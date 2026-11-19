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
  sort: InputSort,
  closeOrder?: readonly string[]
): string | null {
  if (!closeOrder)
    current = inputs.find((d) => d.input.id === current.input.id) ?? current;
  const open = inputs.filter(
    (d) => d.input.status === 'open' && d.input.id !== current.input.id
  );
  const index = closeOrder?.indexOf(current.input.id) ?? -1;
  if (closeOrder && index >= 0) {
    const pending = new Set(open.map((d) => d.input.id));
    const neighbor =
      closeOrder.slice(index + 1).find((id) => pending.has(id)) ??
      closeOrder.slice(0, index).reverse().find((id) => pending.has(id));
    if (neighbor) return neighbor;
  }
  open.sort((a, b) => compareInputs(a, b, sort));
  return (
    open.find((d) => compareInputs(d, current, sort) > 0)?.input.id ??
    (closeOrder ? open.at(-1) : open[0])?.input.id ??
    null
  );
}

export const inputStatusLabel: Record<string, string> = {
  open: 'Needs answer',
  answered: 'Answered',
  closed: 'Closed',
  cancelled: 'Cancelled',
};

// The selection epoch also detects closing and reopening the same Input.
export function inputNavigationIsCurrent(
  started: number,
  current: number,
  fromId: string,
  selectedId: string | null
): boolean {
  return started === current && selectedId === fromId;
}
