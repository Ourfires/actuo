//! Timestamps: RFC 3339 in UTC with milliseconds, e.g. `2026-09-30T12:00:00.123Z`.

use ::time::OffsetDateTime;
use ::time::macros::format_description;

pub fn now() -> String {
    format(OffsetDateTime::now_utc())
}

pub fn format(t: OffsetDateTime) -> String {
    let fmt =
        format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
    t.to_offset(::time::UtcOffset::UTC)
        .format(&fmt)
        .expect("timestamp formats")
}

#[cfg(test)]
mod tests {
    #[test]
    fn shape() {
        let t = super::format(::time::macros::datetime!(2026-09-30 12:00:00.1234 UTC));
        assert_eq!(t, "2026-09-30T12:00:00.123Z");
    }
}
