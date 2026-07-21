import { type ClassValue, clsx } from 'clsx';

// Intentionally bare clsx (no tailwind-merge) until the Tailwind v4 migration,
// matching web-core's cn. Never combine conflicting utilities in one cn() call;
// pass mutually exclusive values (for example, with a ternary), because cn()
// will not deduplicate or resolve which one wins.
export function cn(...inputs: ClassValue[]) {
  return clsx(inputs);
}
