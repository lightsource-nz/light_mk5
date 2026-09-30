//! A card recorder and player: WAV capture from a codec's microphone to a [`Store`], playback of
//! 16-bit PCM WAV back through the codec, a test tone, and the audio diagnostics (mic monitor, mic
//! probe, a known-good synthesised file). Recordings are ordinary mono 16-bit WAV any desktop
//! player opens.
//!
//! [`Recorder`] is a runtime module generic over the application's event type, the way the RTC and
//! power modules are: the application hands it three functions -- which of its events are recorder
//! commands, which asks for stats, and how to carry the recorder's [`AudioStatus`] -- so any app
//! wires it to its own vocabulary without the recorder naming it. What a recording is CALLED, how a
//! list of them is browsed, what a button does: all the application's, driven through
//! [`RecorderCommand`]s.
//!
//! The seams: the codec is an [`Es8311`] over any [`light_core::I2cBus`]; the sample transport is
//! any [`AudioStream`] (the port owns buffers and DMA); the card is any [`Store`].

#![no_std]

pub mod console;

use light_audio::Es8311;
use light_core::hal::{BlockDevice, Store};
use light_core::{info, log, warn, AudioStream, Bus, Clock, I2cBus, Module, OutputPin, Poll, Subscription};
use light_fs::{Fat, File as FsFile, FsError};

pub use light_fs::console::FsPath;

/// The cap on a single take: recording stops itself at this many seconds (five minutes), so a
/// forgotten take cannot fill the card or overflow the WAV's 32-bit sizes.
pub const MAX_REC_SECS: u32 = 5 * 60;

/// Everything the recorder can be asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecorderCommand {
        /// Play a sine at `hz` for `ms`.
        Tone { hz: u16, ms: u16 },
        ToneOff,
        /// The codec's output volume, `0..=100`.
        Volume(u8),
        /// Start a take into this file (created; it must not exist). The name `null` captures and
        /// discards, with the card out of the path -- a bring-up bisect.
        RecStart(FsPath),
        RecStop,
        RecStatus,
        PlayStart(FsPath),
        PlayStop,
        PlayStatus,
        /// Route the microphone straight to the speaker inside the codec.
        MicMon(bool),
        /// Probe the transport's capture and playback lines.
        MicDbg,
        /// Write a 2 s 440 Hz sine to `SINE.WAV`: a known-good file to play.
        Synth,
        /// The mic path's two gain registers, raw -- live tuning without a reflash.
        MicGain { r14: u8, r17: u8 },
}

/// What the recorder is doing, in the terms an interface shows. Published whenever it changes,
/// which includes every elapsed second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioStatus {
        Idle,
        /// A take in flight: its file and elapsed seconds.
        Recording { secs: u16, take: FsPath },
        /// A playback in flight: elapsed and total seconds.
        Playing { secs: u16, total: u16 },
}

impl AudioStatus {
        /// Whether audio is in flight -- what defers an idle power-off.
        pub fn is_active(&self) -> bool {
                !matches!(self, AudioStatus::Idle)
        }
}

/// How the recorder reaches the application's events.
#[derive(Clone, Copy)]
pub struct RecorderEvents<E> {
        /// The recorder command an event carries, if it is one.
        pub command_of: fn(&E) -> Option<RecorderCommand>,
        /// Whether an event asks every module for its stats.
        pub is_stats: fn(&E) -> bool,
        /// The application's event carrying the recorder's status.
        pub status_event: fn(AudioStatus) -> E,
}

/// The tuning a board supplies.
#[derive(Clone, Copy)]
pub struct RecorderConfig {
        pub sample_hz: u32,
        /// The mic path's (SYSTEM14, ADC17) register pair, tuned on the board.
        pub mic_gain: (u8, u8),
        /// The output volume set at start, `0..=100`.
        pub volume: u8,
}

/// The `.bss` slots for the recorder's big state, allocated where the board's memory map wants
/// them -- never inline in the module, which lives on a small core stack.
pub struct RecorderSlots<Dev: BlockDevice + 'static> {
        pub rec: &'static mut Option<Recording<Dev>>,
        pub play: &'static mut Option<Playback<Dev>>,
        /// A small scratch for frame-aligned card reads on the playback path.
        pub play_stage: &'static mut [u8],
}

/// One open recording: the volume stays mounted and every filled capture buffer appends to the
/// file, write-through, until stop patches the WAV header.
pub struct Recording<Dev: BlockDevice> {
        fs: Fat<Dev>,
        file: FsFile,
}

/// One open playback: the file positioned at its PCM data, streamed into the DAC's refill until
/// the data chunk ends.
pub struct Playback<Dev: BlockDevice> {
        fs: Fat<Dev>,
        file: FsFile,
        channels: u8,
        /// Where the data chunk stops -- the file may carry trailing chunks.
        data_end: u32,
        /// Where the PCM begins -- with `data_end`, what elapsed/total time is read from.
        data_start: u32,
}

/// One period of sine at 20000 amplitude, 32 steps -- plenty for a test tone.
static SINE: [i16; 32] = [
        0, 3902, 7654, 11111, 14142, 16629, 18478, 19616, 20000, 19616, 18478, 16629, 14142, 11111, 7654, 3902, 0, -3902, -7654, -11111, -14142, -16629, -18478, -19616, -20000, -19616, -18478, -16629,
        -14142, -11111, -7654, -3902,
];

/// A canonical 44-byte PCM WAV header: mono, 16-bit, `sample_hz` -- what makes a recording a file
/// any desktop player opens.
pub fn wav_header(sample_hz: u32, data_len: u32) -> [u8; 44] {
        let mut h = [0u8; 44];
        h[..4].copy_from_slice(b"RIFF");
        h[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
        h[8..12].copy_from_slice(b"WAVE");
        h[12..16].copy_from_slice(b"fmt ");
        h[16..20].copy_from_slice(&16u32.to_le_bytes());
        h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
        h[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
        h[24..28].copy_from_slice(&sample_hz.to_le_bytes());
        h[28..32].copy_from_slice(&(sample_hz * 2).to_le_bytes());
        h[32..34].copy_from_slice(&2u16.to_le_bytes()); // block align
        h[34..36].copy_from_slice(&16u16.to_le_bytes());
        h[36..40].copy_from_slice(b"data");
        h[40..44].copy_from_slice(&data_len.to_le_bytes());
        h
}

/// The codec plus the sample transport plus the card: recording, playback, the test tone, and
/// every audio diagnostic. The transport streams continuously (silence when nothing plays), so a
/// long frame draw cannot starve the codec into audible chop.
pub struct Recorder<S: Store, B: I2cBus, A: AudioStream, P: OutputPin, C: Clock, E: Copy + 'static> {
        codec: Es8311<B>,
        stream: A,
        /// The speaker amplifier's enable.
        pa: P,
        clock: C,
        config: RecorderConfig,
        bus: &'static dyn Bus<E>,
        sub: Subscription,
        events: RecorderEvents<E>,
        /// Phase accumulator into [`SINE`]; the top 5 bits index the table.
        phase: u32,
        phase_inc: u32,
        /// Sample frames of tone left to play; 0 is silence.
        remaining: u32,
        /// The card, shared with whatever else also serves it.
        store: S,
        rec: &'static mut Option<Recording<S::Dev>>,
        /// The file the take in flight is going to: what an empty take removes.
        take: Option<FsPath>,
        play: &'static mut Option<Playback<S::Dev>>,
        play_stage: &'static mut [u8],
        /// The `rec null` bisect: capture runs, everything drains to nowhere.
        rec_null: bool,
        null_bytes: u32,
        /// Publish-on-change: the last [`AudioStatus`] the application was told about.
        last_status: AudioStatus,
        /// The mic path's gain pair, applied at every record start -- `micgain` retunes it live.
        mic14: u8,
        mic17: u8,
        /// The worst gap between this module's polls while audio was in flight -- the number the
        /// stream buffer's duration has to beat. `stats` prints and resets it.
        poll_us_last: u64,
        poll_gap_max_us: u32,
}

impl<S: Store, B: I2cBus, A: AudioStream, P: OutputPin, C: Clock, E: Copy + 'static> Recorder<S, B, A, P, C, E> {
        #[allow(clippy::too_many_arguments)]
        pub fn new(codec: Es8311<B>, stream: A, pa: P, clock: C, store: S, config: RecorderConfig, slots: RecorderSlots<S::Dev>, bus: &'static dyn Bus<E>, events: RecorderEvents<E>) -> Self {
                let sub = bus.subscribe().expect("subscriber slot");
                Self {
                        codec,
                        stream,
                        pa,
                        clock,
                        config,
                        bus,
                        sub,
                        events,
                        phase: 0,
                        phase_inc: 0,
                        remaining: 0,
                        store,
                        rec: slots.rec,
                        take: None,
                        play: slots.play,
                        play_stage: slots.play_stage,
                        rec_null: false,
                        null_bytes: 0,
                        last_status: AudioStatus::Idle,
                        mic14: config.mic_gain.0,
                        mic17: config.mic_gain.1,
                        poll_us_last: 0,
                        poll_gap_max_us: 0,
                }
        }

        /// What an interface should say right now, read straight from the state.
        fn status(&self) -> AudioStatus {
                let rate = self.config.sample_hz * 2;
                if let Some(r) = self.rec.as_ref() {
                        AudioStatus::Recording { secs: (r.file.size().saturating_sub(44) / rate) as u16, take: self.take.unwrap_or(FsPath::EMPTY) }
                } else if let Some(p) = self.play.as_ref() {
                        let rate = rate * u32::from(p.channels);
                        AudioStatus::Playing {
                                secs: (p.file.pos().saturating_sub(p.data_start) / rate) as u16,
                                total: (p.data_end.saturating_sub(p.data_start) / rate) as u16,
                        }
                } else {
                        AudioStatus::Idle
                }
        }

        /// Put the speaker amplifier back in its resting state: on (`load` enables it, and playback
        /// and tones rely on it), except while a take is capturing, when it is held off so the
        /// speaker cannot feed the microphone. Every path that switches it off for its own purposes
        /// hands it back through here -- a diagnostic that left it off once silenced every later
        /// tone and playback with nothing to say why.
        fn amp_rest(&mut self) {
                self.pa.set(!(self.rec.is_some() || self.rec_null));
        }

        fn rec_start(&mut self, path: &str) {
                if self.rec.is_some() || self.rec_null {
                        info!("rec: already recording");
                        return;
                }
                if path == "null" {
                        //   the freeze bisect: the WHOLE capture pipeline -- mic, transport, buffer
                        // hand-off -- with the card and filesystem entirely out of the path. Stable
                        // here + frozen with a file convicts the card leg
                        for _ in 0..3 {
                                if self.codec.mic_enable().is_ok() {
                                        break;
                                }
                        }
                        self.pa.set(false);
                        self.stream.capture_start();
                        self.rec_null = true;
                        self.null_bytes = 0;
                        info!("rec: null sink -- capturing and discarding");
                        return;
                }
                //   speaker amp OFF *before* the card setup, not after: opening, mounting and
                // writing the header block the core for the best part of a second, and the idle DAC
                // ring underruns and restarts through those stalls -- a click each time, audible
                // through a live speaker in the gap before capture begins. Any failure path below
                // turns it back on, since no take will follow to do so.
                self.pa.set(false);
                let Some(dev) = self.store.open() else {
                        info!("rec: no card");
                        self.pa.set(true);
                        return;
                };
                info!("rec: card up");
                let mut fs = match Fat::mount(dev) {
                        Ok(fs) => fs,
                        Err(e) => {
                                info!("rec: mount failed: {e:?}");
                                self.pa.set(true);
                                return;
                        }
                };
                info!("rec: mounted");
                let mut file = match fs.create(path) {
                        Ok(f) => f,
                        Err(e) => {
                                info!("rec: create failed: {e:?}");
                                self.pa.set(true);
                                return;
                        }
                };
                info!("rec: created");
                if let Err(e) = file.write(&mut fs, &wav_header(self.config.sample_hz, 0)) {
                        info!("rec: header write failed: {e:?}");
                        self.pa.set(true);
                        return;
                }
                info!("rec: header written");
                //   retried: the first attempt right after the SD burst has been seen to time out
                // where a later one succeeds
                let mut mic = Err(light_core::hal::I2cError::Timeout);
                for attempt in 1..=3 {
                        mic = self.codec.mic_config(self.mic14, self.mic17);
                        if mic.is_ok() {
                                info!("rec: mic enabled (attempt {attempt})");
                                break;
                        }
                }
                if let Err(e) = mic {
                        warn!("rec: mic enable failed after retries: {e:?}");
                }
                //   the amp is already off (muted before the card setup above); it stays off for
                // the take, since live it clicks with every SD write burst and the microphone
                // records its own speaker
                self.stream.capture_start();
                info!("rec: capture started, speaker muted");
                *self.rec = Some(Recording { fs, file });
                self.take = FsPath::new(path);
                info!("rec: recording {} -- mono 16-bit at {} Hz", path, self.config.sample_hz);
        }

        fn rec_stop(&mut self) {
                self.stream.capture_stop();
                self.pa.set(true);
                if self.rec_null {
                        self.rec_null = false;
                        info!("rec: null sink stopped -- {} B captured and discarded", self.null_bytes);
                        return;
                }
                let Some(mut rec) = self.rec.take() else {
                        info!("rec: not recording");
                        return;
                };
                let take = self.take.take();
                let data = rec.file.size().saturating_sub(44);
                if data == 0 {
                        //   nothing captured -- a record/stop faster than one buffer fills. Discard
                        // the take rather than leave a header-only WAV that would list as an
                        // unplayable row (the file handle is a plain index, nothing to close)
                        match take {
                                Some(p) => match rec.fs.remove(p.as_str()) {
                                        Ok(()) => info!("rec: discarded empty take {}", p.as_str()),
                                        Err(e) => warn!("rec: could not remove empty take {}: {e:?}", p.as_str()),
                                },
                                None => warn!("rec: empty take with no name to remove"),
                        }
                        return;
                }
                //   the header written blind at start now learns the real length
                let patch = rec.file.seek(&mut rec.fs, 0).and_then(|()| rec.file.write(&mut rec.fs, &wav_header(self.config.sample_hz, data)).map(|_| ()));
                match patch {
                        Ok(()) => info!("rec: stopped -- {} B of audio, ~{} ms", data, (u64::from(data) / 2 * 1000 / u64::from(self.config.sample_hz)) as u32),
                        Err(e) => warn!("rec: header patch failed: {e:?}"),
                }
        }

        fn play_start(&mut self, path: &str) {
                if self.play.is_some() {
                        info!("play: already playing");
                        return;
                }
                if self.rec.is_some() || self.rec_null {
                        info!("play: stop the recording first");
                        return;
                }
                //   step logs like rec's: twice a wedge struck between the command echo and the
                // first outcome line, and nothing said which step held the core
                info!("play: starting {path}");
                let Some(dev) = self.store.open() else {
                        info!("play: no card");
                        return;
                };
                let mut fs = match Fat::mount(dev) {
                        Ok(fs) => fs,
                        Err(e) => {
                                info!("play: mount failed: {e:?}");
                                return;
                        }
                };
                info!("play: mounted");
                let mut file = match fs.open(path) {
                        Ok(f) => f,
                        Err(e) => {
                                info!("play: open failed: {e:?}");
                                return;
                        }
                };
                info!("play: open, {} B", file.size());
                //   the RIFF walk: WAV headers are chunked, and while OUR files are the canonical 44
                // bytes, a desktop-authored one may carry LIST chunks first
                let mut hdr = [0u8; 12];
                if file.read(&mut fs, &mut hdr).unwrap_or(0) != 12 || &hdr[..4] != b"RIFF" || &hdr[8..12] != b"WAVE" {
                        info!("play: not a WAV");
                        return;
                }
                let (mut channels, mut rate, mut pcm16) = (0u16, 0u32, false);
                for _ in 0..16 {
                        let mut ch = [0u8; 8];
                        if file.read(&mut fs, &mut ch).unwrap_or(0) != 8 {
                                info!("play: no data chunk");
                                return;
                        }
                        let len = u32::from_le_bytes([ch[4], ch[5], ch[6], ch[7]]);
                        if &ch[..4] == b"fmt " && len >= 16 {
                                let mut f = [0u8; 16];
                                if file.read(&mut fs, &mut f).unwrap_or(0) != 16 {
                                        info!("play: short fmt chunk");
                                        return;
                                }
                                let format = u16::from_le_bytes([f[0], f[1]]);
                                channels = u16::from_le_bytes([f[2], f[3]]);
                                rate = u32::from_le_bytes([f[4], f[5], f[6], f[7]]);
                                let bits = u16::from_le_bytes([f[14], f[15]]);
                                pcm16 = format == 1 && bits == 16;
                                let extra = file.pos() + (len - 16) + (len & 1);
                                if file.seek(&mut fs, extra).is_err() {
                                        return;
                                }
                        } else if &ch[..4] == b"data" {
                                if !pcm16 || rate != self.config.sample_hz || !(1..=2).contains(&channels) {
                                        info!("play: unsupported format ({channels} ch, {rate} Hz) -- this codec run takes 16-bit PCM at {} Hz", self.config.sample_hz);
                                        return;
                                }
                                let data_start = file.pos();
                                let data_end = data_start.saturating_add(len).min(file.size());
                                //   u64: the frame count times 1000 overflows u32 past ~4.5 minutes of
                                // audio, and a long take (or a stray big WAV on the card) is a
                                // duration to print, not a panic
                                let ms = ((u64::from(data_end - data_start) / (u64::from(channels) * 2)) * 1000 / u64::from(self.config.sample_hz)) as u32;
                                info!("play: {} -- {} ch, ~{} ms", path, channels, ms);
                                //   drop any tail a previous play or stop left queued, so this file
                                // starts clean rather than after stale audio
                                self.stream.stream_clear();
                                *self.play = Some(Playback { fs, file, channels: channels as u8, data_end, data_start });
                                return;
                        } else {
                                let next = file.pos().saturating_add(len + (len & 1));
                                if file.seek(&mut fs, next).is_err() {
                                        return;
                                }
                        }
                }
                info!("play: gave up looking for the data chunk");
        }

        fn play_stop(&mut self) {
                if self.play.take().is_some() {
                        //   drop the buffered lead so the codec falls silent now, not after the ring
                        // plays out what was queued ahead
                        self.stream.stream_clear();
                        self.stream.set_active(false);
                        info!("play: stopped");
                } else {
                        info!("play: idle");
                }
        }

        /// Write a clean 2 s 440 Hz sine to `SINE.WAV` via the ordinary filesystem path, so playing
        /// it exercises the file-playback chain with a KNOWN-good signal -- splitting "playback
        /// broken" from "recording bad".
        fn synth(&mut self) {
                let Some(dev) = self.store.open() else {
                        info!("synth: no card");
                        return;
                };
                let mut fs = match Fat::mount(dev) {
                        Ok(fs) => fs,
                        Err(e) => {
                                info!("synth: mount failed: {e:?}");
                                return;
                        }
                };
                let _ = fs.remove("SINE.WAV");
                match fs.create("SINE.WAV") {
                        Ok(mut f) => {
                                let sample_hz = self.config.sample_hz;
                                let samples = sample_hz * 2; // 2 s
                                let _ = f.write(&mut fs, &wav_header(sample_hz, samples * 2));
                                let inc = ((440u64 << 32) / u64::from(sample_hz)) as u32;
                                let mut phase = 0u32;
                                //   960-byte chunks (480 samples) off the stack -- small
                                let mut chunk = [0u8; 960];
                                let mut written = 0u32;
                                let mut ok = true;
                                while written < samples && ok {
                                        let n = ((samples - written) as usize).min(480);
                                        for s in 0..n {
                                                let v = SINE[(phase >> 27) as usize];
                                                chunk[s * 2..s * 2 + 2].copy_from_slice(&v.to_le_bytes());
                                                phase = phase.wrapping_add(inc);
                                        }
                                        ok = f.write(&mut fs, &chunk[..n * 2]).is_ok();
                                        written += n as u32;
                                }
                                info!("synth: SINE.WAV written ({} samples) -- play it", written);
                        }
                        Err(e) => info!("synth: create failed: {e:?}"),
                }
        }

        fn command(&mut self, c: RecorderCommand) {
                match c {
                        RecorderCommand::Tone { hz, ms } => {
                                self.phase_inc = ((u64::from(hz) << 32) / u64::from(self.config.sample_hz)) as u32;
                                self.remaining = u32::from(ms) * self.config.sample_hz / 1000;
                                info!("tone {hz} Hz for {ms} ms");
                        }
                        RecorderCommand::ToneOff => {
                                self.remaining = 0;
                                info!("tone off");
                        }
                        RecorderCommand::Volume(v) => match self.codec.set_volume(v) {
                                Ok(()) => info!("volume {v}"),
                                Err(e) => warn!("volume set failed: {e:?}"),
                        },
                        RecorderCommand::RecStart(path) => self.rec_start(path.as_str()),
                        RecorderCommand::RecStop => self.rec_stop(),
                        RecorderCommand::RecStatus => {
                                match self.rec.as_ref() {
                                        Some(r) => info!("rec: recording, {} B so far, {} overruns", r.file.size(), self.stream.cap_overruns()),
                                        None => info!("rec: idle"),
                                };
                        }
                        RecorderCommand::PlayStart(path) => self.play_start(path.as_str()),
                        RecorderCommand::PlayStop => self.play_stop(),
                        RecorderCommand::PlayStatus => {
                                match self.play.as_ref() {
                                        Some(p) => info!("play: at {} of {} B", p.file.pos(), p.data_end),
                                        None => info!("play: idle"),
                                };
                        }
                        RecorderCommand::MicGain { r14, r17 } => {
                                self.mic14 = r14;
                                self.mic17 = r17;
                                //   applied immediately too, so a live take retunes mid-recording and
                                // the next one starts here
                                match self.codec.mic_config(r14, r17) {
                                        Ok(()) => info!("micgain: REG14 0x{r14:02x}, REG17 0x{r17:02x}"),
                                        Err(e) => warn!("micgain: applied at next rec ({e:?})"),
                                }
                        }
                        RecorderCommand::Synth => self.synth(),
                        RecorderCommand::MicMon(on) => {
                                //   enable the mic, route ADC->DAC, unmute and drive the speaker: the
                                // analog front end, alone
                                let r = if on {
                                        self.codec.mic_enable().and_then(|()| self.codec.set_adc_to_dac(true)).and_then(|()| self.codec.mute(false))
                                } else {
                                        self.codec.set_adc_to_dac(false)
                                };
                                //   on drives the speaker regardless; off ends only the loopback, and
                                // the amplifier returns to rest
                                if on {
                                        self.pa.set(true);
                                } else {
                                        self.amp_rest();
                                }
                                match r {
                                        Ok(()) => info!("micmon {}: talk near the board", if on { "on -- speaker carries the mic" } else { "off" }),
                                        Err(e) => warn!("micmon failed: {e:?}"),
                                }
                        }
                        RecorderCommand::MicDbg => {
                                //   enable the mic, then let the port probe its own capture path
                                // (speaker muted, NO loopback -- cannot feed back). Splits "mic line
                                // dead" from "transport misreads a toggling line"
                                self.pa.set(false);
                                let _ = self.codec.set_adc_to_dac(false);
                                for _ in 0..3 {
                                        if self.codec.mic_enable().is_ok() {
                                                break;
                                        }
                                }
                                self.stream.debug_probe();
                                self.amp_rest();
                        }
                }
        }
}

impl<S: Store, B: I2cBus, A: AudioStream, P: OutputPin, C: Clock, E: Copy + 'static> Module for Recorder<S, B, A, P, C, E> {
        fn name(&self) -> &'static str {
                "recorder"
        }

        fn load(&mut self) -> Result<(), ()> {
                match self.codec.probe() {
                        Ok(Some(id)) => info!("es8311 chip id confirmed: 0x{id:04x}"),
                        Ok(None) => warn!("es8311 answered with an unexpected chip id"),
                        Err(e) => warn!("es8311 did not answer the chip id read: {e:?}"),
                }
                if let Err(e) = self.codec.init(self.config.sample_hz, &mut self.clock) {
                        warn!("es8311 init failed: {e:?}");
                        return Ok(());
                }
                let _ = self.codec.set_volume(self.config.volume);
                //   belt and suspenders against the ADC->DAC monitor's feedback loop: the codec reset
                // in init() already clears it, but assert it off explicitly so no prior micmon state
                // can ever survive into a running speaker
                let _ = self.codec.set_adc_to_dac(false);
                self.stream.start();
                self.pa.set(true);
                info!("audio up: es8311 master at {} Hz", self.config.sample_hz);
                Ok(())
        }

        fn poll(&mut self) -> Poll {
                //   the worst poll-to-poll gap while audio is in flight: measured, not guessed,
                // because the stream buffer's duration is a budget this gap spends
                let now = log::now_us();
                if self.rec.is_some() || self.play.is_some() || self.rec_null || self.remaining > 0 {
                        if self.poll_us_last != 0 {
                                self.poll_gap_max_us = self.poll_gap_max_us.max((now - self.poll_us_last) as u32);
                        }
                        self.poll_us_last = now;
                } else {
                        self.poll_us_last = 0;
                }
                while let Some(ev) = self.bus.poll(&self.sub) {
                        if let Some(c) = (self.events.command_of)(&ev) {
                                self.command(c);
                        } else if (self.events.is_stats)(&ev) {
                                //   reset on read, so each reading covers the interval since the last
                                // -- a cumulative count here spent a debugging session being misread
                                // as per-playback
                                info!("audio: {} stream underruns, {} capture overruns; max poll gap {} us", self.stream.underruns(), self.stream.cap_overruns(), self.poll_gap_max_us);
                                self.stream.reset_stats();
                                self.poll_gap_max_us = 0;
                        }
                }
                //   drain the capture ring into the file; an I/O failure ends the take
                let mut failed = false;
                if let Some(rec) = self.rec.as_mut() {
                        let file = &mut rec.file;
                        let fs = &mut rec.fs;
                        let mut err: Option<FsError> = None;
                        self.stream.capture_take(&mut |buf| {
                                if err.is_some() {
                                        return;
                                }
                                // SAFETY: a u16 slice viewed as its little-endian bytes -- exactly
                                // WAV's PCM order
                                let bytes = unsafe { core::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 2) };
                                if let Err(e) = file.write(fs, bytes) {
                                        err = Some(e);
                                }
                        });
                        if let Some(e) = err {
                                warn!("rec: write failed ({e:?}); stopping");
                                failed = true;
                        }
                } else if self.rec_null {
                        //   the bisect sink: drain and discard, no card in the path
                        let n = &mut self.null_bytes;
                        self.stream.capture_take(&mut |buf| *n += (buf.len() * 2) as u32);
                }
                if failed {
                        self.rec_stop();
                }
                //   the length cap: a take stops itself at MAX_REC_SECS. Checked right after the
                // drain, so the file size the elapsed seconds are read from already includes this
                // poll's captured bytes
                if let Some(rec) = self.rec.as_ref() {
                        let secs = rec.file.size().saturating_sub(44) / (self.config.sample_hz * 2);
                        if secs >= MAX_REC_SECS {
                                info!("rec: reached the {}s cap -- stopping", MAX_REC_SECS);
                                self.rec_stop();
                        }
                }
                let playing = self.remaining > 0 || self.rec.is_some() || self.rec_null || self.play.is_some();
                if self.play.is_some() {
                        //   a file plays: top the IRQ-drained prefetch ring up toward full off the
                        // card. The IRQ copies the ring into the DMA buffers on its own, so a blocked
                        // poll (a slow card read right here) spends the ring's lead, not the codec's
                        // deadline. Card bytes convert to one output WORD per frame, the 16-bit sample
                        // in both slots; `channels * 2` bytes advance a frame (stereo's left channel
                        // is what plays).
                        self.stream.set_active(true);
                        let stream = &mut self.stream;
                        let play = self.play.as_mut().expect("play present");
                        let scratch = &mut *self.play_stage;
                        let fs = &mut play.fs;
                        let file = &mut play.file;
                        let data_end = play.data_end;
                        let stride = usize::from(play.channels) * 2;
                        let mut failed = false;
                        while stream.stream_free() > 0 {
                                let mut produced = 0usize;
                                stream.stream_push(&mut |dst| {
                                        //   frame-aligned card read into the scratch, then convert; the
                                        // scratch bounds one push, the outer loop tops up the rest
                                        let want_words = dst.len().min(scratch.len() / stride);
                                        let remaining = data_end.saturating_sub(file.pos()) as usize;
                                        let want_bytes = (want_words * stride).min(remaining - remaining % stride);
                                        if want_bytes == 0 {
                                                return 0;
                                        }
                                        let mut got = 0;
                                        while got < want_bytes {
                                                match file.read(fs, &mut scratch[got..want_bytes]) {
                                                        Ok(0) => break,
                                                        Ok(n) => got += n,
                                                        Err(_) => {
                                                                failed = true;
                                                                break;
                                                        }
                                                }
                                        }
                                        let frames = got / stride;
                                        for f in 0..frames {
                                                let b = f * stride;
                                                let sample = i16::from_le_bytes([scratch[b], scratch[b + 1]]);
                                                let s = u32::from(sample as u16);
                                                dst[f] = s << 16 | s;
                                        }
                                        produced = frames;
                                        frames
                                });
                                if failed || produced == 0 {
                                        break;
                                }
                        }
                        //   done producing when the data chunk is spent; finish only once the ring has
                        // also drained, so the tail is not cut off. A card error ends it at once.
                        let exhausted = self.play.as_ref().map(|p| p.file.pos() >= p.data_end).unwrap_or(true);
                        if failed {
                                warn!("play: read failed; stopping");
                                self.stream.stream_clear();
                                self.stream.set_active(false);
                                *self.play = None;
                        } else if exhausted && self.stream.stream_pending() == 0 {
                                info!("play: finished");
                                self.stream.set_active(false);
                                *self.play = None;
                        }
                } else if self.remaining > 0 {
                        //   the test tone: synthesise sine straight into the ring, one word per frame,
                        // capped at the frames left so the tone runs its exact length
                        self.stream.set_active(true);
                        let stream = &mut self.stream;
                        let phase = &mut self.phase;
                        let inc = self.phase_inc;
                        let remaining = &mut self.remaining;
                        while stream.stream_free() > 0 && *remaining > 0 {
                                let mut produced = 0usize;
                                stream.stream_push(&mut |dst| {
                                        let n = dst.len().min(*remaining as usize);
                                        for slot in dst[..n].iter_mut() {
                                                let s = u32::from(SINE[(*phase >> 27) as usize] as u16);
                                                *slot = s << 16 | s;
                                                *phase = phase.wrapping_add(inc);
                                        }
                                        *remaining -= n as u32;
                                        produced = n;
                                        n
                                });
                                if produced == 0 {
                                        break;
                                }
                        }
                } else {
                        //   idle (or recording): nothing to play. The ring drains to silence and
                        // underrun accounting is gated off, so that is not counted as starvation.
                        self.stream.set_active(false);
                }
                //   publish-on-change: the elapsed second is part of the status value, so a live
                // take announces itself once a second and transitions announce at once
                let s = self.status();
                if s != self.last_status {
                        self.last_status = s;
                        let _ = self.bus.publish((self.events.status_event)(s));
                }
                if playing { Poll::Busy } else { Poll::Idle }
        }

        fn unload(&mut self) {
                self.pa.set(false);
        }
}

#[cfg(test)]
mod tests {
        use super::*;

        #[test]
        fn the_wav_header_describes_mono_16_bit_pcm() {
                let h = wav_header(24_000, 480);
                assert_eq!(&h[..4], b"RIFF");
                assert_eq!(u32::from_le_bytes([h[4], h[5], h[6], h[7]]), 36 + 480);
                assert_eq!(&h[8..16], b"WAVEfmt ");
                assert_eq!(u16::from_le_bytes([h[22], h[23]]), 1);
                assert_eq!(u32::from_le_bytes([h[24], h[25], h[26], h[27]]), 24_000);
                assert_eq!(u32::from_le_bytes([h[28], h[29], h[30], h[31]]), 48_000);
                assert_eq!(u16::from_le_bytes([h[34], h[35]]), 16);
                assert_eq!(&h[36..40], b"data");
                assert_eq!(u32::from_le_bytes([h[40], h[41], h[42], h[43]]), 480);
        }

        #[test]
        fn only_idle_is_inactive() {
                assert!(!AudioStatus::Idle.is_active());
                assert!(AudioStatus::Recording { secs: 0, take: FsPath::EMPTY }.is_active());
                assert!(AudioStatus::Playing { secs: 0, total: 3 }.is_active());
        }
}
