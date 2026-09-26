//! crush: font-crusher in Rust.
//!
//! The command surface is kept compatible with the C implementation this replaces -- `font add`,
//! `display add`, `render new`, `console` -- because that is what `crush_add_font_target()`
//! generates, what the 93 acceptance tests exercise, and what people have in their shell
//! history. What changes is the OUTPUT: a render now produces an LGF blob (see `light-font`)
//! beside the C pair existing consumers still link, so the two stacks can share one crush
//! during the migration.
//!
//! The context is a directory of JSON files, `.crush/` in the working directory (or
//! `$CRUSH_CONTEXT`), with the same file names and top-level shape as the C implementation's so
//! an existing context template still seeds it.

mod console;
mod context;
mod log;
mod render;
mod theme;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

use context::Context;

#[derive(Parser, Debug)]
#[command(name = "crush", version, about = "renders TrueType fonts into bitmap fonts for light firmware", disable_help_subcommand = true)]
pub struct Cli {
        #[command(subcommand)]
        pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
        /// Fonts registered with this context
        Font {
                #[command(subcommand)]
                cmd: FontCmd,
        },
        /// Displays a font can be rendered against
        Display {
                #[command(subcommand)]
                cmd: DisplayCmd,
        },
        /// Renders: a font at a size against a display
        Render {
                #[command(subcommand)]
                cmd: RenderCmd,
        },
        /// The context itself
        Context {
                #[command(subcommand)]
                cmd: ContextCmd,
        },
        /// Look-and-feel definitions: compile a JSON theme to an LTH blob
        Theme {
                #[command(subcommand)]
                cmd: ThemeCmd,
        },
        /// UI designs: compile a JSON design to an LUI blob
        Ui {
                #[command(subcommand)]
                cmd: UiCmd,
        },
        /// Asset packs: gather compiled blobs into one LAP a device reads from storage
        Pack {
                #[command(subcommand)]
                cmd: PackCmd,
        },
        /// Wireless parts: lift the firmware a radio is uploaded at power-up out of its
        /// vendor header, so it can be delivered as an asset rather than linked into an image
        Radio {
                #[command(subcommand)]
                cmd: RadioCmd,
        },
        /// Run commands from a script, a single --command, or an interactive prompt
        Console(ConsoleArgs),
}

#[derive(Subcommand, Debug)]
pub enum PackCmd {
        /// Gather named blobs into one pack, and write the digest that identifies it
        Build {
                /// Where the LAP pack goes
                output: PathBuf,
                /// An asset, as `<name>=<file>`. Repeat for each. Names are short ASCII
                /// identifiers of at most sixteen characters -- `font`, `theme`, `ui` -- and are
                /// what the firmware asks for
                #[arg(long = "entry", value_name = "NAME=FILE", required = true)]
                entry: Vec<String>,
                /// Where the pack's SHA-256 goes, as the thirty-two raw bytes the firmware is
                /// built with. Without it the digest is only reported
                #[arg(long = "digest", value_name = "FILE")]
                digest: Option<PathBuf>,
        },
        /// Report what a pack holds, without an image to check it against
        Info {
                /// The LAP pack
                input: PathBuf,
        },
}

#[derive(Subcommand, Debug)]
pub enum RadioCmd {
        /// Cut a vendor's combined firmware header into the two blobs a radio is given: the
        /// image uploaded into its RAM, and the regulatory limits loaded after it
        Firmware {
                /// The vendor's combined header, carrying the array and the two lengths
                input: PathBuf,
                /// Where the uploaded image goes
                #[arg(long = "firmware", value_name = "FILE")]
                firmware: PathBuf,
                /// Where the regulatory blob goes
                #[arg(long = "clm", value_name = "FILE")]
                clm: PathBuf,
        },
        /// Lift a radio's Bluetooth image out of the vendor header that carries it. A part that
        /// does both ships two images in two headers; this one has nothing to cut it into
        Bluetooth {
                /// The vendor's Bluetooth firmware header
                input: PathBuf,
                /// Where the uploaded image goes
                #[arg(long = "firmware", value_name = "FILE")]
                firmware: PathBuf,
        },
        /// Lift a module's NVRAM settings out of the vendor header that carries them: its
        /// calibration and identity, shipped as C strings rather than as an array
        Nvram {
                /// The vendor's NVRAM header
                input: PathBuf,
                /// Where the settings go
                #[arg(long = "nvram", value_name = "FILE")]
                nvram: PathBuf,
        },
}

#[derive(Subcommand, Debug)]
pub enum UiCmd {
        /// Compile a JSON design into the binary UI blob the firmware displays
        Compile {
                /// The design source (JSON)
                input: PathBuf,
                /// Where the LUI blob goes
                output: PathBuf,
                /// The crates directory, where a design's `extends: "<crate>"` finds its parent
                /// `<crate>/design.json`. A design that extends by path, or not at all, needs none.
                #[arg(long = "crates")]
                crates: Option<PathBuf>,
        },
}

#[derive(Subcommand, Debug)]
pub enum ThemeCmd {
        /// Compile a JSON theme into the binary blob the firmware embeds
        Compile {
                /// The theme source (JSON), possibly extending a base
                input: PathBuf,
                /// Where the LTH blob goes
                output: PathBuf,
                /// Where `extends: "<name>"` finds framework themes
                #[arg(long = "themes")]
                themes: Option<PathBuf>,
                /// The theme `extends: "default"` aliases -- the board's default
                #[arg(long = "default")]
                default: Option<String>,
        },
}

#[derive(Subcommand, Debug)]
pub enum FontCmd {
        /// Register a font file with the context
        Add {
                /// A font file on this machine
                #[arg(long = "local-file")]
                local_file: PathBuf,
                /// Which face of a collection to use
                #[arg(long = "face-index", default_value_t = 0)]
                face_index: u32,
        },
        /// List registered fonts
        List,
        /// Describe one font
        Info { name: String },
        /// Forget a font
        Remove { name: String },
}

#[derive(Subcommand, Debug)]
pub enum DisplayCmd {
        /// Describe a display by resolution and, optionally, physical size
        Add {
                name: String,
                width: u16,
                height: u16,
                /// Physical size in millimetres, WxH, e.g. 27.9x32.6 -- sets the pixel density
                #[arg(long)]
                dimension: Option<String>,
                /// Bits per pixel
                #[arg(long = "pixel-depth", short = 'p', default_value_t = 1)]
                pixel_depth: u8,
        },
        List,
        Info { name: String },
}

#[derive(Subcommand, Debug)]
pub enum RenderCmd {
        /// Render a font at a size against a display
        New {
                name: String,
                /// Point size, resolved through the display's pixel density
                point_size: f64,
                /// Explicit vertical pixel size; 0 derives it from the point size
                #[arg(default_value_t = 0)]
                pixel_size: u16,
                #[arg(long)]
                font: String,
                #[arg(long)]
                display: String,
        },
        List,
        Info { name: String },
}

#[derive(Subcommand, Debug)]
pub enum ContextCmd {
        /// Create an empty context in the working directory
        New,
        /// Show where the context is and what it holds
        Info,
}

#[derive(Args, Debug)]
pub struct ConsoleArgs {
        /// A script of commands, one per line; stdin when omitted and not a terminal
        pub script: Option<PathBuf>,
        /// Run one command line and exit with its status
        #[arg(short = 'c', long = "command")]
        pub command: Option<String>,
        /// Read one line at a time from stdin, and keep going after failures
        #[arg(short = 'i', long = "interactive")]
        pub interactive: bool,
        /// Run every line of a script even after one fails; the exit status still reports it
        #[arg(long = "keep-going")]
        pub keep_going: bool,
}

/// What a command run inside the console reports: the console decides what to do with it.
pub type CmdResult = Result<(), String>;

fn main() -> ExitCode {
        let cli = Cli::parse();
        let mut ctx = match Context::open() {
                Ok(c) => c,
                Err(e) => {
                        log::error(&format!("{e}"));
                        return ExitCode::from(1);
                }
        };
        let result = match cli.command {
                Command::Console(args) => console::run(&mut ctx, &args),
                other => run_command(&mut ctx, other),
        };
        let saved = ctx.save();
        if let Err(e) = &saved {
                log::error(&format!("could not save the context: {e}"));
        }
        match (result, saved) {
                (Ok(()), Ok(())) => ExitCode::SUCCESS,
                (Err(e), _) => {
                        if !e.is_empty() {
                                log::error(&e);
                        }
                        ExitCode::from(1)
                }
                (_, Err(_)) => ExitCode::from(1),
        }
}

/// Runs one parsed command against the context. Shared by `main` and the console.
pub fn run_command(ctx: &mut Context, command: Command) -> CmdResult {
        match command {
                Command::Font { cmd } => match cmd {
                        FontCmd::Add { local_file, face_index } => ctx.font_add(&local_file, face_index),
                        FontCmd::List => {
                                log::debug("finished parsing command line, target command: 'crush font list'");
                                for f in ctx.fonts() {
                                        log::info(&format!("font '{}' ({} file(s), face {})", f.name, f.files.len(), f.face_index));
                                }
                                Ok(())
                        }
                        FontCmd::Info { name } => ctx.font_info(&name),
                        FontCmd::Remove { name } => ctx.font_remove(&name),
                },
                Command::Display { cmd } => match cmd {
                        DisplayCmd::Add { name, width, height, dimension, pixel_depth } => {
                                ctx.display_add(&name, width, height, dimension.as_deref(), pixel_depth)
                        }
                        DisplayCmd::List => {
                                log::debug("finished parsing command line, target command: 'crush display list'");
                                for d in ctx.displays() {
                                        log::info(&format!("display '{}' {}x{} @ {:.1}x{:.1} ppi, {} bpp", d.name, d.res_h, d.res_v, d.ppi_h, d.ppi_v, d.pixel_depth));
                                }
                                Ok(())
                        }
                        DisplayCmd::Info { name } => ctx.display_info(&name),
                },
                Command::Render { cmd } => match cmd {
                        RenderCmd::New { name, point_size, pixel_size, font, display } => {
                                ctx.render_new(&name, point_size, pixel_size, &font, &display)
                        }
                        RenderCmd::List => {
                                for r in ctx.renders() {
                                        log::info(&format!("render '{}': {} @ {}pt/{}px on {}", r.name, r.font, r.point_size, r.pixel_size, r.display));
                                }
                                Ok(())
                        }
                        RenderCmd::Info { name } => ctx.render_info(&name),
                },
                Command::Context { cmd } => match cmd {
                        ContextCmd::New => ctx.create_new(),
                        ContextCmd::Info => {
                                log::info(&format!("context at '{}': {} fonts, {} displays, {} renders", ctx.dir().display(), ctx.fonts().len(), ctx.displays().len(), ctx.renders().len()));
                                Ok(())
                        }
                },
                Command::Theme { cmd } => match cmd {
                        ThemeCmd::Compile { input, output, themes, default } => theme::compile(&input, &output, themes.as_deref(), default.as_deref()),
                },
                Command::Ui { cmd } => match cmd {
                        UiCmd::Compile { input, output, crates } => ui_compile(&input, &output, crates.as_deref()),
                },
                Command::Pack { cmd } => match cmd {
                        PackCmd::Build { output, entry, digest } => pack_build(&output, &entry, digest.as_deref()),
                        PackCmd::Info { input } => pack_info(&input),
                },
                Command::Radio { cmd } => match cmd {
                        RadioCmd::Firmware { input, firmware, clm } => radio_firmware(&input, &firmware, &clm),
                        RadioCmd::Bluetooth { input, firmware } => radio_bluetooth(&input, &firmware),
                        RadioCmd::Nvram { input, nvram } => radio_nvram(&input, &nvram),
                },
                Command::Console(_) => Err("console cannot be nested".into()),
        }
}

/// Compile a JSON design to an LUI blob, resolving its `extends` chain first.
fn ui_compile(input: &std::path::Path, output: &std::path::Path, crates: Option<&std::path::Path>) -> CmdResult {
        let design = crush_core::design::resolve_file(input, crates)?;
        let blob = crush_core::lui::compile(&design)?;
        if let Some(dir) = output.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("could not create '{}': {e}", dir.display()))?;
        }
        std::fs::write(output, &blob).map_err(|e| format!("could not write '{}': {e}", output.display()))?;
        log::info(&format!("design '{}': {} pages, {} bytes -> {}", input.display(), design.pages.len(), blob.len(), output.display()));
        Ok(())
}

/// Gather the named blobs into a pack, and write out the digest that identifies it.
///
/// The digest goes to a file of its own because two artefacts have to agree on it: the pack, which
/// carries it in its header, and the firmware image, which is built with it and checks the pack
/// against it at startup. One build step produces both, so they cannot drift.
fn pack_build(output: &std::path::Path, entries: &[String], digest: Option<&std::path::Path>) -> CmdResult {
        let mut builder = light_assets::build::Builder::new();
        for spec in entries {
                let (name, file) = spec
                        .split_once('=')
                        .ok_or_else(|| format!("entry '{spec}' is not <name>=<file>"))?;
                let bytes = std::fs::read(file).map_err(|e| format!("could not read '{file}': {e}"))?;
                builder
                        .add(name, &bytes)
                        .map_err(|e| format!("entry '{name}' ({file}) cannot go in a pack: {e:?}"))?;
                log::debug(&format!("pack entry '{name}': {} bytes from {file}", bytes.len()));
        }
        let blob = builder.build(light_assets::soft::SoftSha256::new());
        let pack = light_assets::Pack::open_unchecked(&blob).map_err(|e| format!("the pack just built does not read back: {e:?}"))?;

        write_out(output, &blob)?;
        if let Some(path) = digest {
                write_out(path, pack.digest())?;
        }
        log::info(&format!(
                "pack '{}': {} entries, {} bytes, sha256 {}",
                output.display(),
                pack.len(),
                blob.len(),
                hex(pack.digest())
        ));
        Ok(())
}

/// Cut a radio's two blobs out of the vendor header that carries both.
fn radio_firmware(input: &std::path::Path, firmware: &std::path::Path, clm: &std::path::Path) -> CmdResult {
        let text = std::fs::read_to_string(input).map_err(|e| format!("could not read '{}': {e}", input.display()))?;
        let split = crush_core::radio::split(&text).map_err(|e| format!("'{}': {e}", input.display()))?;
        log::info(&format!(
                "radio firmware from '{}': {} bytes of image, {} bytes of regulatory data",
                input.display(),
                split.firmware.len(),
                split.clm.len()
        ));
        write_out(firmware, &split.firmware)?;
        write_out(clm, &split.clm)?;
        Ok(())
}

/// Lift a radio's Bluetooth image out of its own vendor header.
fn radio_bluetooth(input: &std::path::Path, firmware: &std::path::Path) -> CmdResult {
        let text = std::fs::read_to_string(input).map_err(|e| format!("could not read '{}': {e}", input.display()))?;
        let image = crush_core::radio::bluetooth(&text).map_err(|e| format!("'{}': {e}", input.display()))?;
        log::info(&format!("radio bluetooth from '{}': {} bytes of image", input.display(), image.len()));
        write_out(firmware, &image)?;
        Ok(())
}

/// Lift a module's NVRAM settings out of its vendor header.
fn radio_nvram(input: &std::path::Path, nvram: &std::path::Path) -> CmdResult {
        let text = std::fs::read_to_string(input).map_err(|e| format!("could not read '{}': {e}", input.display()))?;
        let settings = crush_core::radio::nvram(&text).map_err(|e| format!("'{}': {e}", input.display()))?;
        //   the count of settings, not just the byte count: it is the number a person can sanity-check
        // against the header they pointed at
        let count = settings.split(|b| *b == 0).filter(|s| !s.is_empty()).count();
        log::info(&format!("radio nvram from '{}': {} settings, {} bytes", input.display(), count, settings.len()));
        write_out(nvram, &settings)?;
        Ok(())
}

/// Report a pack's contents: what a person asks when a device says the pack is not the one.
fn pack_info(input: &std::path::Path) -> CmdResult {
        let blob = std::fs::read(input).map_err(|e| format!("could not read '{}': {e}", input.display()))?;
        let pack = light_assets::Pack::open_unchecked(&blob).map_err(|e| format!("'{}' is not a readable pack: {e:?}", input.display()))?;
        log::info(&format!("pack '{}': {} entries, {} bytes, sha256 {}", input.display(), pack.len(), pack.as_bytes().len(), hex(pack.digest())));
        for i in 0..pack.len() {
                let (name, bytes) = (pack.name(i).unwrap_or("?"), pack.blob(i).map_or(0, <[u8]>::len));
                log::info(&format!("  {name}: {bytes} bytes"));
        }
        Ok(())
}

fn write_out(path: &std::path::Path, bytes: &[u8]) -> CmdResult {
        if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("could not create '{}': {e}", dir.display()))?;
        }
        std::fs::write(path, bytes).map_err(|e| format!("could not write '{}': {e}", path.display()))
}

fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write;
        bytes.iter().fold(String::new(), |mut s, b| {
                let _ = write!(s, "{b:02x}");
                s
        })
}
