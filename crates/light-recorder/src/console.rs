//! The recorder's console commands, parsed once for every application: each parser turns the words
//! after its command into a [`RecorderCommand`], and [`wrap`] lifts that into the application's own
//! event, since the event type is the application's.

use light_core::cli::{Parsed, Words};
use light_fs::console::FsPath;

use crate::RecorderCommand;

pub const TONE_USAGE: &str = "tone HZ [MS] | tone off";
pub const VOLUME_USAGE: &str = "volume 0..100";
pub const REC_USAGE: &str = "rec NAME.WAV | rec stop | rec";
pub const PLAY_USAGE: &str = "play NAME.WAV | play stop | play";
pub const MICMON_USAGE: &str = "micmon on|off";
pub const MICGAIN_USAGE: &str = "micgain R14HEX R17HEX (e.g. micgain 17 DF)";

/// Lift a parsed recorder command into the application's event.
pub fn wrap<E>(p: Parsed<RecorderCommand>, into: impl FnOnce(RecorderCommand) -> E) -> Parsed<E> {
        match p {
                Parsed::Event(c) => Parsed::Event(into(c)),
                Parsed::Done => Parsed::Done,
                Parsed::Usage => Parsed::Usage,
        }
}

/// `tone HZ [MS]` (20..=10000 Hz, 500 ms by default) or `tone off`.
pub fn tone(w: &mut Words) -> Parsed<RecorderCommand> {
        match w.next() {
                Some("off") => Parsed::Event(RecorderCommand::ToneOff),
                Some(hz) => {
                        let Ok(hz) = hz.parse::<u16>() else { return Parsed::Usage };
                        if !(20..=10_000).contains(&hz) {
                                return Parsed::Usage;
                        }
                        let ms = match w.next() {
                                None => 500,
                                Some(ms) => match ms.parse::<u16>() {
                                        Ok(ms) if ms > 0 => ms,
                                        _ => return Parsed::Usage,
                                },
                        };
                        Parsed::Event(RecorderCommand::Tone { hz, ms })
                }
                None => Parsed::Usage,
        }
}

/// `volume 0..100`.
pub fn volume(w: &mut Words) -> Parsed<RecorderCommand> {
        match w.next().and_then(|s| s.parse::<u8>().ok()) {
                Some(v) if v <= 100 => Parsed::Event(RecorderCommand::Volume(v)),
                _ => Parsed::Usage,
        }
}

/// `rec NAME.WAV` starts a take, `rec stop` ends it, a bare `rec` reports.
pub fn rec(w: &mut Words) -> Parsed<RecorderCommand> {
        match w.next() {
                None => Parsed::Event(RecorderCommand::RecStatus),
                Some("stop") => Parsed::Event(RecorderCommand::RecStop),
                Some(name) => match FsPath::new(name) {
                        Some(p) => Parsed::Event(RecorderCommand::RecStart(p)),
                        None => Parsed::Usage,
                },
        }
}

/// `play NAME.WAV` starts playback, `play stop` ends it, a bare `play` reports.
pub fn play(w: &mut Words) -> Parsed<RecorderCommand> {
        match w.next() {
                None => Parsed::Event(RecorderCommand::PlayStatus),
                Some("stop") => Parsed::Event(RecorderCommand::PlayStop),
                Some(name) => match FsPath::new(name) {
                        Some(p) => Parsed::Event(RecorderCommand::PlayStart(p)),
                        None => Parsed::Usage,
                },
        }
}

/// `micmon on|off`.
pub fn micmon(w: &mut Words) -> Parsed<RecorderCommand> {
        match w.next() {
                Some("on") => Parsed::Event(RecorderCommand::MicMon(true)),
                Some("off") => Parsed::Event(RecorderCommand::MicMon(false)),
                _ => Parsed::Usage,
        }
}

/// `micgain R14 R17`, both hex.
pub fn micgain(w: &mut Words) -> Parsed<RecorderCommand> {
        match (w.next().and_then(|s| u8::from_str_radix(s, 16).ok()), w.next().and_then(|s| u8::from_str_radix(s, 16).ok())) {
                (Some(r14), Some(r17)) => Parsed::Event(RecorderCommand::MicGain { r14, r17 }),
                _ => Parsed::Usage,
        }
}

/// `micdbg`.
pub fn micdbg(_w: &mut Words) -> Parsed<RecorderCommand> {
        Parsed::Event(RecorderCommand::MicDbg)
}

/// `synth`.
pub fn synth(_w: &mut Words) -> Parsed<RecorderCommand> {
        Parsed::Event(RecorderCommand::Synth)
}

#[cfg(test)]
mod tests {
        use super::*;

        fn run(f: fn(&mut Words) -> Parsed<RecorderCommand>, line: &str) -> Option<RecorderCommand> {
                match f(&mut Words::new(line)) {
                        Parsed::Event(c) => Some(c),
                        _ => None,
                }
        }

        #[test]
        fn tone_defaults_its_length_and_bounds_its_pitch() {
                assert_eq!(run(tone, "440"), Some(RecorderCommand::Tone { hz: 440, ms: 500 }));
                assert_eq!(run(tone, "440 3000"), Some(RecorderCommand::Tone { hz: 440, ms: 3000 }));
                assert_eq!(run(tone, "off"), Some(RecorderCommand::ToneOff));
                assert_eq!(run(tone, "10"), None);
                assert_eq!(run(tone, "440 0"), None);
                assert_eq!(run(tone, ""), None);
        }

        #[test]
        fn rec_and_play_start_stop_and_report() {
                let Some(RecorderCommand::RecStart(p)) = run(rec, "TAKE.WAV") else { panic!("expected a start") };
                assert_eq!(p.as_str(), "TAKE.WAV");
                assert_eq!(run(rec, "stop"), Some(RecorderCommand::RecStop));
                assert_eq!(run(rec, ""), Some(RecorderCommand::RecStatus));
                assert_eq!(run(play, "stop"), Some(RecorderCommand::PlayStop));
                assert_eq!(run(play, ""), Some(RecorderCommand::PlayStatus));
        }

        #[test]
        fn volume_micmon_and_micgain_validate() {
                assert_eq!(run(volume, "78"), Some(RecorderCommand::Volume(78)));
                assert_eq!(run(volume, "101"), None);
                assert_eq!(run(micmon, "on"), Some(RecorderCommand::MicMon(true)));
                assert_eq!(run(micmon, "maybe"), None);
                assert_eq!(run(micgain, "17 EF"), Some(RecorderCommand::MicGain { r14: 0x17, r17: 0xEF }));
                assert_eq!(run(micgain, "17"), None);
        }

        #[test]
        fn wrap_lifts_into_the_applications_event() {
                #[derive(Debug, PartialEq)]
                struct App(RecorderCommand);
                assert!(matches!(wrap(volume(&mut Words::new("50")), App), Parsed::Event(App(RecorderCommand::Volume(50)))));
                assert!(matches!(wrap(volume(&mut Words::new("x")), App), Parsed::Usage));
        }
}
