//! Instants as the window says them, in the local time zone: today,
//! yesterday or a date, with the hour and minute. The numbers come from
//! here, the words from Slint (`DashboardWords`).

use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;

/// Which day an instant fell on, from the day it is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Day {
    Today,
    Yesterday,
    /// Another day: its day of the month, its month (1 to 12), and its
    /// year when it is not this one.
    On {
        day: i8,
        month: i8,
        year: Option<i16>,
    },
}

/// An instant as the window says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Moment {
    pub day: Day,
    pub hour: i8,
    pub minute: i8,
}

/// The instant `seconds` (since the clock's origin) as said at `now` in
/// `zone`; `None` for seconds no date can hold.
pub(crate) fn moment(seconds: i64, now: Timestamp, zone: &TimeZone) -> Option<Moment> {
    let at = Timestamp::from_second(seconds).ok()?.to_zoned(zone.clone());
    let today = now.to_zoned(zone.clone()).date();
    let date = at.date();
    Some(Moment {
        day: day_of(date, today),
        hour: at.hour(),
        minute: at.minute(),
    })
}

fn day_of(date: Date, today: Date) -> Day {
    if date == today {
        Day::Today
    } else if today.yesterday().is_ok_and(|yesterday| yesterday == date) {
        Day::Yesterday
    } else {
        Day::On {
            day: date.day(),
            month: date.month(),
            year: (date.year() != today.year()).then_some(date.year()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paris's rule written out: the test never depends on the zones the
    /// machine has.
    fn paris() -> TimeZone {
        TimeZone::posix("CET-1CEST,M3.5.0,M10.5.0/3").unwrap()
    }

    /// 2026-11-13 at 14:30 in Paris (13:30 UTC).
    fn now() -> Timestamp {
        "2026-11-13T13:30:00Z".parse().unwrap()
    }

    fn at(utc: &str) -> i64 {
        utc.parse::<Timestamp>().unwrap().as_second()
    }

    #[test]
    fn an_instant_is_said_in_the_local_zone_today_yesterday_or_on_its_date() {
        assert_eq!(
            moment(at("2026-11-13T08:12:00Z"), now(), &paris()),
            Some(Moment {
                day: Day::Today,
                hour: 9,
                minute: 12
            })
        );
        assert_eq!(
            moment(at("2026-11-12T17:00:00Z"), now(), &paris()).map(|moment| moment.day),
            Some(Day::Yesterday)
        );
        assert_eq!(
            moment(at("2026-11-08T09:05:00Z"), now(), &paris()),
            Some(Moment {
                day: Day::On {
                    day: 8,
                    month: 11,
                    year: None
                },
                hour: 10,
                minute: 5
            })
        );
    }

    #[test]
    fn the_day_turns_at_local_midnight_not_at_utc_s() {
        // 23:30 UTC on the 12th is 00:30 on the 13th in Paris: today.
        assert_eq!(
            moment(at("2026-11-12T23:30:00Z"), now(), &paris()).map(|moment| moment.day),
            Some(Day::Today)
        );
        // 22:30 UTC on the 12th is 23:30 on the 12th in Paris: yesterday.
        assert_eq!(
            moment(at("2026-11-12T22:30:00Z"), now(), &paris()).map(|moment| moment.day),
            Some(Day::Yesterday)
        );
    }

    #[test]
    fn summer_time_is_followed() {
        // 07:12 UTC in July is 9:12 in Paris (CEST).
        assert_eq!(
            moment(
                at("2026-07-13T07:12:00Z"),
                "2026-07-13T10:00:00Z".parse().unwrap(),
                &paris()
            ),
            Some(Moment {
                day: Day::Today,
                hour: 9,
                minute: 12
            })
        );
    }

    #[test]
    fn yesterday_crosses_months_and_years() {
        let new_month: Timestamp = "2026-12-01T10:00:00Z".parse().unwrap();
        assert_eq!(
            moment(at("2026-11-30T10:00:00Z"), new_month, &paris()).map(|moment| moment.day),
            Some(Day::Yesterday)
        );
        let new_year: Timestamp = "2027-01-01T10:00:00Z".parse().unwrap();
        assert_eq!(
            moment(at("2026-12-31T10:00:00Z"), new_year, &paris()).map(|moment| moment.day),
            Some(Day::Yesterday)
        );
        assert_eq!(
            moment(at("2026-12-30T10:00:00Z"), new_year, &paris()).map(|moment| moment.day),
            Some(Day::On {
                day: 30,
                month: 12,
                year: Some(2026)
            }),
            "a day of another year carries its year"
        );
        // 23:30 UTC on 31 December is already 1 January in Paris: this
        // year, as the day.
        let new_year_week: Timestamp = "2027-01-05T10:00:00Z".parse().unwrap();
        assert_eq!(
            moment(at("2026-12-31T23:30:00Z"), new_year_week, &paris()).map(|moment| moment.day),
            Some(Day::On {
                day: 1,
                month: 1,
                year: None
            })
        );
    }

    #[test]
    fn seconds_no_date_holds_are_none() {
        assert_eq!(moment(i64::MAX, now(), &paris()), None);
    }
}
