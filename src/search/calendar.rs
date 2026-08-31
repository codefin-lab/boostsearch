//! Calendar units: what a month is, and where one begins.

#[derive(Clone, Copy)]
pub(crate) enum CalendarUnit {
    Second,
    Minute,
    Hour,
    Day,
    Week,
    /// auto_date_histogram's seven-day rounding starts its weeks on Sunday,
    /// unlike `calendar_interval: week`
    WeekSunday,
    Month,
    Quarter,
    Year,
}

/// Why the calendar arithmetic below cannot fail.
pub(crate) const ZERO_IS_IN_RANGE: &str = "zero is in range for every field of a time";

pub(crate) const FIRST_EXISTS: &str = "the first day exists in every month";

impl CalendarUnit {
    pub(crate) fn parse(s: &str) -> Option<CalendarUnit> {
        Some(match s {
            "second" | "1s" => CalendarUnit::Second,
            "minute" | "1m" => CalendarUnit::Minute,
            "hour" | "1h" => CalendarUnit::Hour,
            "day" | "1d" => CalendarUnit::Day,
            "week" | "1w" => CalendarUnit::Week,
            "week_sunday" => CalendarUnit::WeekSunday,
            "month" | "1M" => CalendarUnit::Month,
            "quarter" | "1q" => CalendarUnit::Quarter,
            "year" | "1y" => CalendarUnit::Year,
            _ => return None,
        })
    }

    pub(crate) fn floor(
        self,
        dt: boostcore::time::OffsetDateTime,
    ) -> boostcore::time::OffsetDateTime {
        use boostcore::time::{Date, Month, Time};
        let midnight = |d: Date| d.with_time(Time::MIDNIGHT).assume_utc();
        match self {
            // zero is in range for every one of these, whatever the instant
            CalendarUnit::Second => dt.replace_nanosecond(0).expect(ZERO_IS_IN_RANGE),
            CalendarUnit::Minute => dt
                .replace_second(0)
                .expect(ZERO_IS_IN_RANGE)
                .replace_nanosecond(0)
                .expect(ZERO_IS_IN_RANGE),
            CalendarUnit::Hour => dt
                .replace_minute(0)
                .expect(ZERO_IS_IN_RANGE)
                .replace_second(0)
                .expect(ZERO_IS_IN_RANGE)
                .replace_nanosecond(0)
                .expect(ZERO_IS_IN_RANGE),
            CalendarUnit::Day => midnight(dt.date()),
            // calendar weeks start on Monday
            CalendarUnit::Week => {
                let back = dt.weekday().number_days_from_monday() as i64;
                midnight(dt.date() - boostcore::time::Duration::days(back))
            }
            CalendarUnit::WeekSunday => {
                let back = dt.weekday().number_days_from_sunday() as i64;
                midnight(dt.date() - boostcore::time::Duration::days(back))
            }
            // the first of a month exists in every month of every year the
            // instant itself could be in
            CalendarUnit::Month => {
                midnight(Date::from_calendar_date(dt.year(), dt.month(), 1).expect(FIRST_EXISTS))
            }
            CalendarUnit::Quarter => {
                let m = ((dt.month() as u8 - 1) / 3) * 3 + 1;
                let month = Month::try_from(m).expect("a quarter starts in month 1, 4, 7 or 10");
                midnight(Date::from_calendar_date(dt.year(), month, 1).expect(FIRST_EXISTS))
            }
            CalendarUnit::Year => midnight(
                Date::from_calendar_date(dt.year(), Month::January, 1).expect(FIRST_EXISTS),
            ),
        }
    }

    pub(crate) fn advance(
        self,
        dt: boostcore::time::OffsetDateTime,
    ) -> boostcore::time::OffsetDateTime {
        use boostcore::time::{Date, Duration, Month, Time};
        let add_months = |dt: boostcore::time::OffsetDateTime, n: u32| {
            let total = dt.year() * 12 + (dt.month() as i32 - 1) + n as i32;
            let (y, m) = (total.div_euclid(12), total.rem_euclid(12) as u8 + 1);
            // a remainder of twelve plus one is a month, and the first of it
            // exists -- but a year past what a date can hold does not, and
            // that is the one case where the instant stands still
            let month = Month::try_from(m).expect("a remainder mod twelve, plus one, is a month");
            match Date::from_calendar_date(y, month, 1) {
                Ok(d) => d.with_time(Time::MIDNIGHT).assume_utc(),
                Err(_) => dt,
            }
        };
        match self {
            CalendarUnit::Second => dt + Duration::seconds(1),
            CalendarUnit::Minute => dt + Duration::minutes(1),
            CalendarUnit::Hour => dt + Duration::hours(1),
            CalendarUnit::Day => dt + Duration::days(1),
            CalendarUnit::Week | CalendarUnit::WeekSunday => dt + Duration::days(7),
            CalendarUnit::Month => add_months(dt, 1),
            CalendarUnit::Quarter => add_months(dt, 3),
            CalendarUnit::Year => add_months(dt, 12),
        }
    }
}
