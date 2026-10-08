//! Timestamp helpers.
//!
//! Timestamps are stored as Unix seconds. Rendering is UTC-based so that ids,
//! logs and the local database never disagree about what "today" means.

/// Current Unix time in seconds.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `2026-10-08 12:35:04 UTC`
pub fn format_timestamp(timestamp: i64) -> String {
    let (days, seconds) = div_mod(timestamp, 86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60
    )
}

/// `Oct 6`, or `Oct 6, 2025` when the year differs from `now`.
pub fn format_date(timestamp: i64, now: i64) -> String {
    let (days, _) = div_mod(timestamp, 86_400);
    let (year, month, day) = civil_from_days(days);
    let (now_year, _, _) = civil_from_days(div_mod(now, 86_400).0);
    let month_name = MONTHS
        .get((month as usize).saturating_sub(1))
        .copied()
        .unwrap_or("???");
    if year == now_year {
        format!("{month_name} {day}")
    } else {
        format!("{month_name} {day}, {year}")
    }
}

/// Human-friendly age such as `just now`, `2 min ago`, `yesterday`, `Oct 6`.
pub fn format_relative(now: i64, timestamp: i64) -> String {
    let delta = now - timestamp;
    if delta < 0 {
        return format_date(timestamp, now);
    }
    match delta {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", delta / 60),
        3600..=86_399 => {
            let hours = delta / 3600;
            if hours == 1 {
                "1 hour ago".to_string()
            } else {
                format!("{hours} hours ago")
            }
        }
        86_400..=172_799 => "yesterday".to_string(),
        172_800..=604_799 => format!("{} days ago", delta / 86_400),
        _ => format_date(timestamp, now),
    }
}

fn div_mod(value: i64, divisor: i64) -> (i64, i64) {
    let quotient = value.div_euclid(divisor);
    let remainder = value.rem_euclid(divisor);
    (quotient, remainder)
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_known_timestamps_render_in_utc() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_timestamp(946_684_800), "2000-01-01 00:00:00 UTC");
        assert_eq!(format_timestamp(1_700_000_000), "2023-11-14 22:13:20 UTC");
    }

    #[test]
    fn relative_labels_are_human_readable() {
        let now = 1_700_000_000;
        assert_eq!(format_relative(now, now), "just now");
        assert_eq!(format_relative(now, now - 120), "2 min ago");
        assert_eq!(format_relative(now, now - 3600), "1 hour ago");
        assert_eq!(format_relative(now, now - 7200), "2 hours ago");
        assert_eq!(format_relative(now, now - 86_400), "yesterday");
        assert_eq!(format_relative(now, now - 3 * 86_400), "3 days ago");
        assert_eq!(format_relative(now, now - 60 * 86_400), "Sep 15");
    }

    #[test]
    fn dates_in_other_years_include_the_year() {
        let now = 1_700_000_000; // 2023-11-14
        assert_eq!(format_date(0, now), "Jan 1, 1970");
        assert_eq!(format_date(now, now), "Nov 14");
    }

    #[test]
    fn timestamps_in_the_future_do_not_panic() {
        let now = 1_700_000_000;
        assert_eq!(format_relative(now, now + 500), format_date(now + 500, now));
    }

    #[test]
    fn unix_now_is_plausible() {
        assert!(unix_now() > 1_600_000_000);
    }
}
