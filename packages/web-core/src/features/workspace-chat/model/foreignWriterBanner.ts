/**
 * A dismissal suppresses only the writer event the user saw. A later native
 * feed timestamp must raise the banner again.
 */
export function shouldShowForeignWriterBanner(
  foreignWriterSeenAt: string | null,
  dismissedAt: string | null
): boolean {
  if (!foreignWriterSeenAt) return false;
  if (!dismissedAt) return true;
  if (foreignWriterSeenAt === dismissedAt) return false;

  const seenTime = Date.parse(foreignWriterSeenAt);
  const dismissedTime = Date.parse(dismissedAt);
  if (Number.isFinite(seenTime) && Number.isFinite(dismissedTime)) {
    return seenTime > dismissedTime;
  }

  // Backend timestamps are ISO-8601. Keep deterministic ordering if a future
  // transport supplies a value Date.parse does not recognise.
  return foreignWriterSeenAt > dismissedAt;
}
