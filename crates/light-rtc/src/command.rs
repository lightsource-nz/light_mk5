//! The clock's console command, parsed once for every board: `rtc` shows the time, and
//! `rtc set YYYY-MM-DD HH:MM:SS` sets it. The events it produces live in each application's own
//! vocabulary, so the caller passes the two it wants: the parse is shared, the event type is not.

use light_core::cli::{Parsed, Words};

use crate::Datetime;

/// The usage line for a command table built on [`parse_command`].
pub const USAGE: &str = "rtc | rtc set YYYY-MM-DD HH:MM:SS";

/// Parse the words after `rtc`: nothing is `show`, `set DATE TIME` is `set(datetime)`, anything
/// else is the usage line.
pub fn parse_command<E>(w: &mut Words, show: E, set: impl FnOnce(Datetime) -> E) -> Parsed<E> {
        match w.next() {
                None => Parsed::Event(show),
                Some("set") => {
                        let (Some(date), Some(time)) = (w.next(), w.next()) else { return Parsed::Usage };
                        match parse_datetime(date, time) {
                                Some(t) => Parsed::Event(set(t)),
                                None => Parsed::Usage,
                        }
                }
                _ => Parsed::Usage,
        }
}

/// `YYYY-MM-DD` and `HH:MM:SS` as a [`Datetime`], its weekday computed; `None` when either is
/// malformed or out of range (years 1970..=2069, the span a two-digit year register holds).
pub fn parse_datetime(date: &str, time: &str) -> Option<Datetime> {
        let (year, month, day) = split3(date, '-')?;
        let (hour, minute, second) = split3(time, ':')?;
        let valid = (1..=12).contains(&month) && (1..=31).contains(&day) && hour < 24 && minute < 60 && second < 60 && (1970..=2069).contains(&year);
        if !valid {
                return None;
        }
        Some(Datetime { year, month, day, weekday: weekday(year, month, day), hour: hour as u8, minute, second })
}

fn split3(s: &str, sep: char) -> Option<(u16, u8, u8)> {
        let mut it = s.split(sep);
        let a = it.next()?.parse().ok()?;
        let b = it.next()?.parse().ok()?;
        let c = it.next()?.parse().ok()?;
        if it.next().is_some() {
                return None;
        }
        Some((a, b, c))
}

/// The day of the week by Zeller's congruence, mapped to 0 = Sunday, the register's convention.
pub fn weekday(y: u16, m: u8, d: u8) -> u8 {
        let (mut y, mut m) = (i32::from(y), i32::from(m));
        if m < 3 {
                m += 12;
                y -= 1;
        }
        let (k, j) = (y % 100, y / 100);
        let h = (i32::from(d) + 13 * (m + 1) / 5 + k + k / 4 + j / 4 + 5 * j) % 7;
        ((h + 6) % 7) as u8
}

#[cfg(test)]
mod tests {
        use super::*;

        #[derive(Debug, PartialEq)]
        enum Ev {
                Show,
                Set(Datetime),
        }

        fn parse(line: &str) -> Parsed<Ev> {
                parse_command(&mut Words::new(line), Ev::Show, Ev::Set)
        }

        #[test]
        fn the_weekday_matches_known_dates() {
                assert_eq!(weekday(2026, 10, 1), 4); // a Thursday
                assert_eq!(weekday(2000, 1, 1), 6); // a Saturday
                assert_eq!(weekday(2024, 2, 29), 4); // a leap day, Thursday
                assert_eq!(weekday(1970, 1, 1), 4); // the epoch, Thursday
        }

        #[test]
        fn no_arguments_is_show_and_set_parses_a_full_datetime() {
                assert!(matches!(parse(""), Parsed::Event(Ev::Show)));
                let Parsed::Event(Ev::Set(t)) = parse("set 2026-10-01 14:05:09") else { panic!("expected a set") };
                assert_eq!((t.year, t.month, t.day, t.weekday, t.hour, t.minute, t.second), (2026, 10, 1, 4, 14, 5, 9));
        }

        #[test]
        fn malformed_or_out_of_range_input_is_the_usage_line() {
                for line in ["set", "set 2026-10-01", "set 2026-13-01 00:00:00", "set 2026-10-01 24:00:00", "set 2070-01-01 00:00:00", "set 2026-10-01-02 00:00:00", "set 2026/10/01 00:00:00", "show"] {
                        assert!(matches!(parse(line), Parsed::Usage), "{line}");
                }
        }
}
