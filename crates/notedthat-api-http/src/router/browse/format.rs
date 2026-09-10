//! Turning object metadata into the two columns a directory listing shows.

/// The placeholder for a value a row does not have.
///
/// Folders have no size and no timestamp — they are synthesised from keys, not
/// stored — and an object's `last_modified` is optional. Printing a real-looking
/// value in either case would be a lie; `WebDAV` has to invent one because the
/// protocol demands it, and an HTML page does not.
pub(super) const ABSENT: &str = "—";

/// Human-readable binary size: `812 B`, `4.1 KiB`, `41 KiB`, `1.0 MiB`.
///
/// Integer arithmetic throughout. Going through `f64` would need a
/// `clippy::cast_precision_loss` allow to express what a division and a
/// remainder say exactly.
pub(super) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];

    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut divisor = 1024_u64;
    let mut unit = 0;
    // Step up while the next unit would still leave at least 1 whole of it.
    while unit + 1 < UNITS.len() && bytes / divisor >= 1024 {
        divisor *= 1024;
        unit += 1;
    }

    let whole = bytes / divisor;
    if whole >= 10 {
        return format!("{whole} {}", UNITS[unit]);
    }
    let tenths = (bytes % divisor) * 10 / divisor;
    format!("{whole}.{tenths} {}", UNITS[unit])
}

/// `YYYY-MM-DD` in UTC, or `None` when there is no usable timestamp.
///
/// Derived from `httpdate::fmt_http_date` — the same formatter the API's
/// `Last-Modified` header uses — so the page and the API can never disagree
/// about the instant. That is worth more than the four lines a date library
/// would save, and it avoids a second date implementation in a tree that
/// already has one.
///
/// IMF-fixdate is fixed-width by RFC 7231 §7.1.1.1
/// (`Tue, 08 Sep 2026 12:34:56 GMT`), so the day, month and year sit at known
/// byte offsets. Read with `get`, never by indexing: a formatting change
/// upstream must degrade to `None`, not panic in a page render.
pub(super) fn civil_date(unix_seconds: i64) -> Option<String> {
    let formatted = http_date(unix_seconds)?;
    let day = formatted.get(5..7)?;
    let month_name = formatted.get(8..11)?;
    let month = MONTHS.iter().position(|name| *name == month_name)?;
    let year = formatted.get(12..16)?;
    Some(format!("{year}-{:02}-{day}", month + 1))
}

/// The full RFC 7231 timestamp, for the row's tooltip.
///
/// `httpdate::fmt_http_date` **panics** past year 9999, and `last_modified` is
/// whatever the backend put there. A page render must not panic on absurd
/// metadata, so the value is bounded here rather than trusted.
pub(super) fn http_date(unix_seconds: i64) -> Option<String> {
    /// 9999-12-31T23:59:59Z — the last instant `httpdate` will format.
    const LATEST_FORMATTABLE: u64 = 253_402_300_799;

    let seconds = u64::try_from(unix_seconds).ok()?;
    if seconds > LATEST_FORMATTABLE {
        return None;
    }
    let instant = std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(seconds))?;
    Some(httpdate::fmt_http_date(instant))
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_below_a_kibibyte_are_plain_byte_counts() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1), "1 B");
        assert_eq!(human_size(812), "812 B");
        assert_eq!(human_size(1023), "1023 B");
    }

    #[test]
    fn sizes_gain_one_decimal_below_ten_and_lose_it_above() {
        assert_eq!(human_size(1024), "1.0 KiB");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(4250), "4.1 KiB");
        assert_eq!(human_size(10 * 1024), "10 KiB");
        assert_eq!(human_size(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn the_largest_representable_size_still_formats() {
        // Given / When / Then — no unit overflow, no panic.
        assert_eq!(human_size(u64::MAX), "15 EiB");
    }

    #[test]
    fn a_timestamp_becomes_an_iso_date_in_utc() {
        assert_eq!(civil_date(0).as_deref(), Some("1970-01-01"));
        // 2026-09-08T12:34:56Z
        assert_eq!(civil_date(1_788_870_896).as_deref(), Some("2026-09-08"));
    }

    #[test]
    fn an_unrepresentable_timestamp_yields_no_date_rather_than_panicking() {
        // Given / When / Then — a page render must survive absurd metadata.
        assert_eq!(civil_date(-1), None);
        assert_eq!(
            civil_date(i64::MAX),
            None,
            "must not panic in a page render"
        );
        assert_eq!(
            civil_date(253_402_300_800),
            None,
            "one second past year 9999"
        );
        assert!(
            civil_date(253_402_300_799).is_some(),
            "the last formattable instant"
        );
    }

    #[test]
    fn every_month_round_trips_through_the_name_table() {
        // Given — the first instant of each month of 2026, so an off-by-one in
        // the month table cannot hide behind a single sampled date.
        for (index, expected) in [
            (0_i64, "1970-01-01"),
            (2_678_400, "1970-02-01"),
            (5_097_600, "1970-03-01"),
            (7_776_000, "1970-04-01"),
            (10_368_000, "1970-05-01"),
            (13_046_400, "1970-06-01"),
            (15_638_400, "1970-07-01"),
            (18_316_800, "1970-08-01"),
            (20_995_200, "1970-09-01"),
            (23_587_200, "1970-10-01"),
            (26_265_600, "1970-11-01"),
            (28_857_600, "1970-12-01"),
        ] {
            assert_eq!(civil_date(index).as_deref(), Some(expected));
        }
    }
}
