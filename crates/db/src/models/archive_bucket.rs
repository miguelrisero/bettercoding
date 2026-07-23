use chrono::Duration;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Non-overlapping age ranges for archived workspaces.
///
/// Keep this half-open boundary table in sync with
/// `packages/ui/src/lib/archiveBuckets.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ArchiveBucket {
    Today,
    OneToThreeDays,
    ThreeToSevenDays,
    SevenToFifteenDays,
    FifteenToThirtyDays,
    OlderThanThirtyDays,
}

impl ArchiveBucket {
    pub fn from_age(age: Duration) -> Self {
        // `num_days` floors positive durations to complete days. Clamp clock
        // skew/future timestamps to today instead of producing a negative age.
        match age.num_days().max(0) {
            0 => Self::Today,
            1..=2 => Self::OneToThreeDays,
            3..=6 => Self::ThreeToSevenDays,
            7..=14 => Self::SevenToFifteenDays,
            15..=29 => Self::FifteenToThirtyDays,
            _ => Self::OlderThanThirtyDays,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::ArchiveBucket;

    #[test]
    fn from_age_pins_every_bucket_boundary() {
        let millisecond = Duration::milliseconds(1);
        let cases = [
            (Duration::zero(), ArchiveBucket::Today),
            (millisecond, ArchiveBucket::Today),
            (Duration::days(1) - millisecond, ArchiveBucket::Today),
            (Duration::days(1), ArchiveBucket::OneToThreeDays),
            (
                Duration::days(1) + millisecond,
                ArchiveBucket::OneToThreeDays,
            ),
            (
                Duration::days(3) - millisecond,
                ArchiveBucket::OneToThreeDays,
            ),
            (Duration::days(3), ArchiveBucket::ThreeToSevenDays),
            (
                Duration::days(3) + millisecond,
                ArchiveBucket::ThreeToSevenDays,
            ),
            (
                Duration::days(7) - millisecond,
                ArchiveBucket::ThreeToSevenDays,
            ),
            (Duration::days(7), ArchiveBucket::SevenToFifteenDays),
            (
                Duration::days(7) + millisecond,
                ArchiveBucket::SevenToFifteenDays,
            ),
            (
                Duration::days(15) - millisecond,
                ArchiveBucket::SevenToFifteenDays,
            ),
            (Duration::days(15), ArchiveBucket::FifteenToThirtyDays),
            (
                Duration::days(15) + millisecond,
                ArchiveBucket::FifteenToThirtyDays,
            ),
            (
                Duration::days(30) - millisecond,
                ArchiveBucket::FifteenToThirtyDays,
            ),
            (Duration::days(30), ArchiveBucket::OlderThanThirtyDays),
            (
                Duration::days(30) + millisecond,
                ArchiveBucket::OlderThanThirtyDays,
            ),
        ];

        for (age, expected) in cases {
            assert_eq!(ArchiveBucket::from_age(age), expected, "age: {age:?}");
        }
    }

    #[test]
    fn from_age_clamps_future_timestamps_to_today() {
        assert_eq!(
            ArchiveBucket::from_age(Duration::days(-10)),
            ArchiveBucket::Today
        );
    }
}
