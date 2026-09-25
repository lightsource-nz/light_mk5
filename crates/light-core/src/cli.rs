//! The console front-end of the event bus: one grammar, shared built-ins, and the
//! application's commands as data.
//!
//! Decision 6's last piece. The bus itself made every input path publish the same typed
//! events; what remained was five hand-written copies of the same word-matching -- every
//! application parsing `help`, `loglevel` and `quit` its own way and assembling its own help
//! line. Here the shared part lives once: a [`Cli`] owns echo, the built-ins (`help`,
//! `loglevel`, `quit`), the unknown-command reply and the usage-on-bad-arguments rule, and
//! the application hands it a table of [`Command`]s -- a name, a usage line, and a parse
//! function from the remaining words to the application's event type. A console line, a UI
//! tap and a test all end as the same record on the same bus.
//!
//! `stats` is deliberately NOT a built-in: every application's stats line is its own, so it
//! is just another table entry. What the built-ins cover is exactly what was copied verbatim.

use crate::{info, log, module, warn};

/// The words of a line after the command name, in order.
///
/// Words are separated by spaces, EXCEPT that a run inside double quotes is one word however
/// many spaces are in it. Without that, a command could not be given an argument that contains a
/// space -- and plenty of real ones do: the name of a wireless network, a file with a space in
/// it, a line of text to put on a display. Splitting on spaces alone makes those unreachable from
/// the console, and does it by silently taking the first half.
///
/// There is no escape character, so a word cannot contain a double quote. That is a real limit
/// and a deliberate one: the alternative needs somewhere to build the unescaped word, and every
/// word here is a slice of the line as it arrived.
///
/// An unclosed quote takes the rest of the line. On a console that is friendlier than refusing
/// the whole command over one missing character, and it is what the reader meant.
pub struct Words<'a> {
        rest: &'a str,
}

impl<'a> Words<'a> {
        pub fn new(line: &'a str) -> Self {
                Self { rest: line }
        }
}

impl<'a> Iterator for Words<'a> {
        type Item = &'a str;

        fn next(&mut self) -> Option<&'a str> {
                let s = self.rest.trim_start();
                if s.is_empty() {
                        self.rest = s;
                        return None;
                }
                let (word, rest) = match s.strip_prefix('"') {
                        Some(quoted) => match quoted.find('"') {
                                Some(end) => (&quoted[..end], &quoted[end + 1..]),
                                None => (quoted, ""),
                        },
                        None => match s.find(char::is_whitespace) {
                                Some(end) => (&s[..end], &s[end..]),
                                None => (s, ""),
                        },
                };
                self.rest = rest;
                Some(word)
        }
}

/// One application command: `name` selects it, `parse` takes the words after the name.
pub struct Command<E: 'static> {
        pub name: &'static str,
        /// The full usage line, shown by `help` and echoed as `usage: ...` when `parse`
        /// answers [`Parsed::Usage`]. Starts with the name: `"backlight 0..1000"`.
        pub usage: &'static str,
        pub parse: fn(&mut Words) -> Parsed<E>,
}

/// What a command's parse function decided.
pub enum Parsed<E> {
        /// A well-formed command: publish this event.
        Event(E),
        /// Handled entirely inside the parse function (printed something, flipped a debug
        /// switch); nothing to publish.
        Done,
        /// The arguments made no sense; the [`Cli`] prints the command's usage line.
        Usage,
}

/// What the line amounted to. The caller owns the bus, so an `Event` comes back to be
/// published rather than being published from here -- which is also what lets a test drive
/// the dispatcher without a bus at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome<E> {
        /// An empty line.
        Quiet,
        /// A built-in, a `Parsed::Done`, or something already answered with a warning.
        Handled,
        /// Publish this.
        Event(E),
        /// `quit`: the application should shut its runtime down.
        Shutdown,
}

pub struct Cli<E: 'static> {
        commands: &'static [Command<E>],
}

impl<E> Cli<E> {
        pub const fn new(commands: &'static [Command<E>]) -> Self {
                Self { commands }
        }

        /// Dispatch one line: echo it, try the built-ins, then the table. Every path answers
        /// on the console; the caller only ever publishes the event or shuts down.
        pub fn dispatch(&self, line: &str) -> Outcome<E> {
                let mut words = Words::new(line);
                let Some(cmd) = words.next() else { return Outcome::Quiet };
                info!("> {line}");
                match cmd {
                        "help" => {
                                info!("commands: help | loglevel error|warn|info|debug|trace | loop [detail|off] | quit");
                                for c in self.commands {
                                        info!("  {}", c.usage);
                                }
                                Outcome::Handled
                        }
                        "loglevel" => {
                                let level = match words.next() {
                                        Some("error") => Some(log::Level::Error),
                                        Some("warn") => Some(log::Level::Warn),
                                        Some("info") => Some(log::Level::Info),
                                        Some("debug") => Some(log::Level::Debug),
                                        Some("trace") => Some(log::Level::Trace),
                                        _ => None,
                                };
                                match level {
                                        Some(l) => {
                                                log::set_max_level(l);
                                                info!("log level {}", l.as_str());
                                        }
                                        None => warn!("usage: loglevel error|warn|info|debug|trace"),
                                }
                                Outcome::Handled
                        }
                        //   a built-in for the same reason `loglevel` is one: the loop belongs to
                        // the runtime, not to any application, so every application would
                        // otherwise write the same command to reach it
                        "loop" => {
                                match words.next() {
                                        Some("detail") => module::timing::set_detail(true),
                                        Some("off") => module::timing::set_detail(false),
                                        Some(_) => {
                                                warn!("usage: loop [detail|off]");
                                                return Outcome::Handled;
                                        }
                                        None => {}
                                }
                                //   the runtime answers at the end of this pass: the figures are
                                // its, and this is running inside the pass being measured
                                module::timing::request();
                                Outcome::Handled
                        }
                        "quit" => {
                                info!("shutting down");
                                Outcome::Shutdown
                        }
                        _ => {
                                for c in self.commands {
                                        if c.name == cmd {
                                                return match (c.parse)(&mut words) {
                                                        Parsed::Event(e) => Outcome::Event(e),
                                                        Parsed::Done => Outcome::Handled,
                                                        Parsed::Usage => {
                                                                warn!("usage: {}", c.usage);
                                                                Outcome::Handled
                                                        }
                                                };
                                        }
                                }
                                warn!("unknown command '{cmd}' -- try help");
                                Outcome::Handled
                        }
                }
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        use crate::EventBus;

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Ev {
                Toggle(u8),
                Stats,
        }

        fn words(line: &str) -> heapless::Vec<&str, 8> {
                Words::new(line).collect()
        }

        #[test]
        fn plain_words_are_separated_by_any_run_of_spaces() {
                assert_eq!(words("  join  one   two "), ["join", "one", "two"]);
        }

        #[test]
        fn a_quoted_run_is_one_word_however_many_spaces_are_in_it() {
                //   the case this exists for: an argument that genuinely contains a space, which
                // splitting on spaces alone takes the first half of and says nothing
                assert_eq!(words(r#"join "My Network" secret"#), ["join", "My Network", "secret"]);
        }

        #[test]
        fn quotes_work_for_every_argument_not_only_the_first() {
                assert_eq!(words(r#"join "My Network" "pass phrase""#), ["join", "My Network", "pass phrase"]);
        }

        #[test]
        fn an_empty_quoted_run_is_an_empty_word_rather_than_no_word() {
                //   which is how a network that asks for no passphrase is named explicitly
                assert_eq!(words(r#"join name """#), ["join", "name", ""]);
        }

        #[test]
        fn an_unclosed_quote_takes_the_rest_of_the_line() {
                assert_eq!(words(r#"join "My Network"#), ["join", "My Network"]);
        }

        #[test]
        fn a_quote_after_a_word_has_begun_is_part_of_that_word() {
                //   only a leading quote opens a run, so this does not surprise anyone whose
                // argument merely contains one
                assert_eq!(words(r#"set a"b c"#), ["set", "a\"b", "c"]);
        }

        #[test]
        fn a_line_of_nothing_has_no_words() {
                assert_eq!(words("   ").len(), 0);
                assert_eq!(words("").len(), 0);
        }

        static COMMANDS: &[Command<Ev>] = &[
                Command { name: "toggle", usage: "toggle 0|1", parse: |w| match w.next().and_then(|s| s.parse::<u8>().ok()) {
                        Some(n) if n < 2 => Parsed::Event(Ev::Toggle(n)),
                        _ => Parsed::Usage,
                } },
                Command { name: "stats", usage: "stats", parse: |_| Parsed::Event(Ev::Stats) },
                Command { name: "poke", usage: "poke", parse: |_| Parsed::Done },
        ];

        static CLI: Cli<Ev> = Cli::new(COMMANDS);

        #[test]
        fn lines_become_events_or_answers() {
                assert_eq!(CLI.dispatch(""), Outcome::Quiet);
                assert_eq!(CLI.dispatch("   "), Outcome::Quiet);
                assert_eq!(CLI.dispatch("toggle 1"), Outcome::Event(Ev::Toggle(1)));
                assert_eq!(CLI.dispatch("stats"), Outcome::Event(Ev::Stats));
                assert_eq!(CLI.dispatch("poke"), Outcome::Handled);
                //   bad arguments and unknown commands are answered on the console, not
                // returned as errors: the person typing is the one who needs to know
                assert_eq!(CLI.dispatch("toggle 7"), Outcome::Handled);
                assert_eq!(CLI.dispatch("toggle"), Outcome::Handled);
                assert_eq!(CLI.dispatch("frobnicate"), Outcome::Handled);
                assert_eq!(CLI.dispatch("help"), Outcome::Handled);
                assert_eq!(CLI.dispatch("quit"), Outcome::Shutdown);
        }

        #[test]
        fn loglevel_is_a_builtin_that_takes_effect() {
                assert_eq!(CLI.dispatch("loglevel warn"), Outcome::Handled);
                assert!(!log::enabled(log::Level::Info));
                assert_eq!(CLI.dispatch("loglevel info"), Outcome::Handled);
                assert!(log::enabled(log::Level::Info));
                assert_eq!(CLI.dispatch("loglevel loud"), Outcome::Handled);
        }

        /// Decision 6, end to end: a console line and a direct injection are the same record
        /// on the same bus, and a subscriber cannot tell them apart.
        #[test]
        fn a_console_line_and_a_test_injection_are_the_same_event()  {
                static BUS: EventBus<Ev, 8, 1> = EventBus::new();
                let sub = BUS.subscribe().unwrap();
                if let Outcome::Event(e) = CLI.dispatch("toggle 0") {
                        BUS.publish(e).unwrap();
                }
                BUS.publish(Ev::Toggle(0)).unwrap();
                assert_eq!(BUS.poll(&sub), Some(Ev::Toggle(0)));
                assert_eq!(BUS.poll(&sub), Some(Ev::Toggle(0)));
                assert_eq!(BUS.poll(&sub), None);
        }
}
