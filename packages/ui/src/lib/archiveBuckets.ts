import type { ArchiveBucket } from 'shared/types';

export const ARCHIVE_BUCKET_ORDER: readonly ArchiveBucket[] = [
  'today',
  'one_to_three_days',
  'three_to_seven_days',
  'seven_to_fifteen_days',
  'fifteen_to_thirty_days',
  'older_than_thirty_days',
];

export const DAY_IN_MILLISECONDS = 24 * 60 * 60 * 1000;

/**
 * Maps a duration to the same half-open archive-age buckets as
 * `crates/db/src/models/archive_bucket.rs`. Keep both boundary tables in sync.
 */
export function archiveBucketFromAgeMilliseconds(
  ageMilliseconds: number
): ArchiveBucket {
  if (!Number.isFinite(ageMilliseconds)) {
    return 'older_than_thirty_days';
  }

  const ageDays = Math.floor(
    Math.max(0, ageMilliseconds) / DAY_IN_MILLISECONDS
  );

  if (ageDays === 0) return 'today';
  if (ageDays < 3) return 'one_to_three_days';
  if (ageDays < 7) return 'three_to_seven_days';
  if (ageDays < 15) return 'seven_to_fifteen_days';
  if (ageDays < 30) return 'fifteen_to_thirty_days';
  return 'older_than_thirty_days';
}

export function archiveBucketForTimestamp(
  archivedAt: string | null,
  nowMilliseconds = Date.now()
): ArchiveBucket {
  if (!archivedAt) {
    // Unknown archive ages belong in the oldest bucket. Treating them as fresh
    // would make a destructive action look safer than it is.
    return 'older_than_thirty_days';
  }

  const archivedAtMilliseconds = Date.parse(archivedAt);
  if (!Number.isFinite(archivedAtMilliseconds)) {
    return 'older_than_thirty_days';
  }

  return archiveBucketFromAgeMilliseconds(
    nowMilliseconds - archivedAtMilliseconds
  );
}

export function isArchivedRecently(
  archivedAt: string | null,
  nowMilliseconds = Date.now()
): boolean {
  const bucket = archiveBucketForTimestamp(archivedAt, nowMilliseconds);
  return bucket === 'today' || bucket === 'one_to_three_days';
}
