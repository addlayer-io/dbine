//! Wall-clock arithmetic for job schedules, without a date library: civil
//! dates as the engines print them (`2026-11-12 03:00:00`), whole seconds,
//! month steps clamped to the month's last day (as MySQL's `DATE_ADD`), and
//! RFC 3339 in UTC for the app.

/// A date and time with no zone attached, to the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Civil {
    y: i64,
    mo: u32,
    d: u32,
    h: u32,
    mi: u32,
    s: u32,
}

/// Days from 1970-01-01 to a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if leap(y) => 29,
        _ => 28,
    }
}

impl Civil {
    /// `2026-11-12 03:00:00`, `2026-11-12T03:00:00`, with or without
    /// fractions of a second (dropped) or a trailing `Z`.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        let s = s.trim().trim_end_matches('Z');
        let (date, time) = s.split_once([' ', 'T']).unwrap_or((s, "00:00:00"));
        let mut dp = date.split('-');
        let y: i64 = dp.next()?.parse().ok()?;
        let mo: u32 = dp.next()?.parse().ok()?;
        let d: u32 = dp.next()?.parse().ok()?;
        if dp.next().is_some() {
            return None;
        }
        let time = time.split('.').next()?;
        let mut tp = time.split(':');
        let h: u32 = tp.next()?.parse().ok()?;
        let mi: u32 = tp.next().unwrap_or("0").parse().ok()?;
        let sec: u32 = tp.next().unwrap_or("0").parse().ok()?;
        if tp.next().is_some()
            || !(1..=12).contains(&mo)
            || d == 0
            || d > days_in_month(y, mo)
            || h > 23
            || mi > 59
            || sec > 59
        {
            return None;
        }
        Some(Self {
            y,
            mo,
            d,
            h,
            mi,
            s: sec,
        })
    }

    /// Seconds since 1970-01-01 00:00:00 of the same wall clock.
    pub(crate) fn seconds(self) -> i64 {
        days_from_civil(self.y, self.mo, self.d) * 86_400
            + i64::from(self.h) * 3600
            + i64::from(self.mi) * 60
            + i64::from(self.s)
    }

    pub(crate) fn from_seconds(n: i64) -> Self {
        let (y, mo, d) = civil_from_days(n.div_euclid(86_400));
        let r = n.rem_euclid(86_400);
        Self {
            y,
            mo,
            d,
            h: (r / 3600) as u32,
            mi: (r % 3600 / 60) as u32,
            s: (r % 60) as u32,
        }
    }

    pub(crate) fn add_seconds(self, n: i64) -> Self {
        Self::from_seconds(self.seconds() + n)
    }

    /// `n` months later, the day clamped to the target month's last
    /// (January 31st plus one month is February 28th or 29th).
    pub(crate) fn add_months(self, n: i64) -> Self {
        let period = self.y * 12 + i64::from(self.mo) - 1 + n;
        let y = period.div_euclid(12);
        let mo = (period.rem_euclid(12) + 1) as u32;
        Self {
            y,
            mo,
            d: self.d.min(days_in_month(y, mo)),
            ..self
        }
    }

    /// Months from `self`'s month to `later`'s, ignoring the days.
    pub(crate) fn months_until(self, later: Civil) -> i64 {
        (later.y - self.y) * 12 + i64::from(later.mo) - i64::from(self.mo)
    }

    /// As SQL takes it: `2026-11-12 03:00:00`.
    pub(crate) fn sql(self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.y, self.mo, self.d, self.h, self.mi, self.s
        )
    }

    /// RFC 3339, for a time already in UTC: `2026-11-12T03:00:00Z`.
    pub(crate) fn utc(self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            self.y, self.mo, self.d, self.h, self.mi, self.s
        )
    }

    /// Within the range where MySQL's `CONVERT_TZ` converts (TIMESTAMP's,
    /// with a day of margin); outside it the function returns its input.
    pub(crate) fn in_timestamp_range(self) -> bool {
        let t = self.seconds();
        (86_400..2_147_483_647 - 86_400).contains(&t)
    }
}

/// The offset of a zone that has no daylight saving: `+03:00`, `-05:30`,
/// `UTC`… in seconds east of UTC. `None` for named zones with rules
/// (`Europe/Madrid`) and for `SYSTEM`.
pub(crate) fn fixed_offset(tz: &str) -> Option<i64> {
    let tz = tz.trim();
    if [
        "UTC",
        "Etc/UTC",
        "GMT",
        "Etc/GMT",
        "Z",
        "Universal",
        "Etc/Universal",
        "Zulu",
        "Etc/Zulu",
    ]
    .iter()
    .any(|z| tz.eq_ignore_ascii_case(z))
    {
        return Some(0);
    }
    let (sign, rest) = match tz.as_bytes().first()? {
        b'+' => (1, &tz[1..]),
        b'-' => (-1, &tz[1..]),
        _ => return None,
    };
    let (h, m) = rest.split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    let h: i64 = h.parse().ok()?;
    let m: i64 = m.parse().ok()?;
    (h <= 14 && m < 60).then_some(sign * (h * 3600 + m * 60))
}

/// A wall-clock time in a zone `offset` seconds east of UTC, in UTC.
pub(crate) fn to_utc(local: Civil, offset: i64) -> Civil {
    local.add_seconds(-offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Civil {
        Civil::parse(s).unwrap()
    }

    #[test]
    fn civil_dates_round_trip() {
        for s in [
            "1970-01-01 00:00:00",
            "2000-02-29 23:59:59",
            "2026-11-12 03:00:00",
            "1969-12-31 23:59:59",
            "2400-03-01 12:00:00",
        ] {
            let t = c(s);
            assert_eq!(Civil::from_seconds(t.seconds()), t, "{s}");
            assert_eq!(t.sql(), s);
        }
        assert_eq!(c("1970-01-02 00:00:00").seconds(), 86_400);
        assert_eq!(c("2026-11-12T03:00:00Z").utc(), "2026-11-12T03:00:00Z");
        assert_eq!(c("2026-11-12 03:00:00.123456").sql(), "2026-11-12 03:00:00");
        assert_eq!(c("2026-11-12").sql(), "2026-11-12 00:00:00");
        for bad in [
            "",
            "2026-02-30 00:00:00",
            "2026-13-01 00:00:00",
            "2026-01-01 24:00:00",
            "x",
            "2026-01-01-01",
        ] {
            assert!(Civil::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn month_steps_clamp_to_the_last_day() {
        let jan31 = c("2026-01-31 03:00:00");
        assert_eq!(jan31.add_months(1).sql(), "2026-02-28 03:00:00");
        assert_eq!(jan31.add_months(2).sql(), "2026-03-31 03:00:00");
        assert_eq!(
            c("2024-01-31 00:00:00").add_months(1).sql(),
            "2024-02-29 00:00:00"
        );
        assert_eq!(jan31.add_months(12).sql(), "2027-01-31 03:00:00");
        assert_eq!(jan31.add_months(-1).sql(), "2025-12-31 03:00:00");
        assert_eq!(jan31.months_until(c("2027-03-01 00:00:00")), 14);
        assert_eq!(
            c("2026-12-31 23:59:30").add_seconds(45).sql(),
            "2027-01-01 00:00:15"
        );
    }

    #[test]
    fn fixed_offsets() {
        assert_eq!(fixed_offset("+00:00"), Some(0));
        assert_eq!(fixed_offset("UTC"), Some(0));
        assert_eq!(fixed_offset("Etc/UTC"), Some(0));
        assert_eq!(fixed_offset("-03:00"), Some(-10_800));
        assert_eq!(fixed_offset("+5:30"), Some(19_800));
        assert_eq!(fixed_offset("SYSTEM"), None);
        assert_eq!(fixed_offset("Europe/Madrid"), None);
        assert_eq!(fixed_offset("+25:00"), None);
        assert_eq!(
            to_utc(c("2026-11-12 00:00:00"), -10_800).utc(),
            "2026-11-12T03:00:00Z"
        );
        assert!(c("2026-11-12 00:00:00").in_timestamp_range());
        assert!(!c("2040-01-01 00:00:00").in_timestamp_range());
        assert!(!c("1970-01-01 00:00:00").in_timestamp_range());
    }
}
