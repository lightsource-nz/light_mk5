# 04 — The interface as data

A device's font, its look-and-feel and its whole user interface are **files you author**, not
code. Each is compiled at build time by `crush`, the framework's asset compiler, into a binary
blob the firmware embeds with `include_bytes!`. Restyling an interface is a change to a theme
file; relaying it is a change to a design file; neither touches a line of Rust. This chapter
covers the three files, the CMake calls that compile them, and the Rust that drives the result
onto a panel.

Specification: [08-assets-and-tooling.md](../documents/08-assets-and-tooling.md) (the formats
and the tool) and [02-graphics-and-ui.md](../documents/02-graphics-and-ui.md) (the rendering
stack and the widget toolkit).

## The three assets and their CMake calls

Each asset is one call in the firmware target's `CMakeLists.txt`, next to the executable it
belongs to. The call compiles the source to a blob under the build tree and hands the blob's path
to the named crate as an environment variable; the crate reads it with
`include_bytes!(env!("…"))`. Editing the source recompiles the blob and rebuilds the crate, and
nothing else.

```cmake
light_add_font(blinky_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY po13 WIDTH 64 HEIGHT 128 DIMENSION 17.2x32.3
        POINT_SIZE 6 PIXEL_SIZE 8
        CRATE light_app_blinky ENV LIGHT_FONT_LGF)

light_add_theme(blinky_theme MONO
        THEME ${CMAKE_CURRENT_LIST_DIR}/theme/po13.json
        CRATE light_app_blinky ENV LIGHT_THEME_LTH)

light_add_ui(blinky_ui
        UI ${CMAKE_CURRENT_LIST_DIR}/design.json
        CRATE light_app_blinky ENV LIGHT_UI_LUI)
```

```rust
static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));
static THEME_BLOB: &[u8] = include_bytes!(env!("LIGHT_THEME_LTH"));
static UI_BLOB: &[u8] = include_bytes!(env!("LIGHT_UI_LUI"));
```

| Call | Arguments |
| --- | --- |
| `light_add_font(<name> …)` | `FONT <file>` a TrueType face; `DISPLAY <name> WIDTH <px> HEIGHT <px>` the panel; `DIMENSION <WxH mm>` its physical size, from which the pixel density is derived (without it 96 ppi is assumed and point sizes will not match the glass); `POINT_SIZE <pt>` and `PIXEL_SIZE <px>` the size (a non-zero pixel size wins and is the *vertical* size); `CRATE`/`ENV` where to embed it. |
| `light_add_theme(<name> …)` | `[THEME <file.json>]` a theme to compile, else the framework default; `[MONO]` makes the default the monochrome one, for a 1-bpp panel; `CRATE`/`ENV`. |
| `light_add_ui(<name> …)` | `UI <design.json>` the design; `CRATE`/`ENV`. |

Without `CRATE`/`ENV` the blob is compiled but not embedded, for `light_add_asset_pack` to
gather into a pack the device reads from a storage partition instead — the choice a product makes
when it wants to update its assets without re-shipping its firmware (see
[08-assets-and-tooling.md](../documents/08-assets-and-tooling.md), *Where the blobs live*).

The three CMake helpers are included from `cmake/LightFont.cmake`, `cmake/LightTheme.cmake` and
`cmake/LightUi.cmake`, after `crush` has been imported as a host build (the root `CMakeLists.txt`
does both; [Chapter 05](05-your-own-project.md) shows a project doing the same).

## Fonts

A font is a TrueType face rendered once, at build time, to the **LGF** bitmap format: a fixed
cell per glyph, the 95 printable ASCII characters, 1 bit per pixel. The firmware parses the blob
in place and never rasterises anything:

```rust
use light_font::Font;

let font = match Font::parse(FONT_BLOB) {
        Ok(f) => f,
        Err(e) => panic!("the embedded font does not parse: {e:?}"),
};
info!("font {} px, cell {}x{}", font.pixel_size(), font.cell_width(), font.cell_height());
```

A `Font` is `Copy` and borrows the blob, so passing it around costs nothing. A bad blob is a build
defect, which is why the examples panic on it rather than styling past it.

Choosing a size: the cell is derived from the face's nominal metrics, so a 16 px font on a
240-pixel-wide panel gives roughly 240 / cell_width characters per row. The widget demo uses
14 pt / 16 px on a 240×280 panel and 6 pt / 8 px on a 64×128 OLED.

## Themes

A theme is a small JSON file of colours, surfaces and metrics. The framework ships two bases in
`themes/`: `steel` (colour) and `mono` (for 1-bpp panels). A board's theme normally **extends** one
and overrides what it must:

```json
{
        "name": "round169",
        "extends": "default",
        "metrics": { "screen_radius": 42 }
}
```

`"extends": "default"` means whichever base the CMake call chose (`steel`, or `mono` under the
`MONO` flag); `"extends": "steel"` names a base in `themes/` directly; a value containing a slash
or ending in `.json` is a path relative to the theme file. Each level overrides the one beneath
it.

The complete vocabulary:

```json
{
        "name": "steel",
        "colors": {
                "bg": "#B0B8C0",           "frame": "0000",           "title": "#D8E0E8",
                "text": "FFFF",            "button_outline": "#7890A8",
                "button_text": "FFFF",     "focus_text": "FFFF",
                "indicator": "#FF0000",    "bar": "#22405F"
        },
        "surfaces": {
                "focus":  { "from": "4C5D", "to": "090E" },
                "button": { "from": "090E", "to": "090E" }
        },
        "metrics": { "radius": 4, "screen_radius": 0 },
        "descent": "left"
}
```

- A **colour** is either `"#RRGGBB"` (truncated to RGB565) or four hex digits of RGB565 directly.
- A **surface** is a vertical gradient `from`/`to`; `"focus": null` in a child suppresses the
  base's gradient rather than inheriting it, which is how `mono` gets a flat inverted focus.
- **Metrics** are the button corner `radius` and the glass's `screen_radius` — the curvature of
  the panel's own corners, so a frame drawn flush to the edge follows the glass. Measure it;
  guesses are routinely wrong by a factor of two.
- **`descent`** (`top`/`bottom`/`left`/`right`) is the default edge a page slides in from.

Unknown keys are errors: a typo stops the build rather than silently styling nothing. At runtime
the parser skips keys it does not know, so a newer compiler still styles older firmware.

```rust
use light_ui::{Fonts, Style, Theme};

let theme = Theme::parse(THEME_BLOB).expect("the embedded theme parses");
layer.bg = theme.bg;                                          // the frame layer clears to the theme's ground
ui.set_style(&Style::new(theme, Fonts::uniform(&font)));      // one face for titles and body
```

## UI designs

A design is a JSON description of the device, its pages, and the **actions** those pages can
perform. `crush` compiles it to an **LUI** blob; `light-ui` builds its widget tree from the blob
at runtime. The widget demo's design, cut down:

```json
{
  "device": { "width": 172, "height": 640, "corner_radius": 0 },
  "root": 0,
  "actions": [
    { "name": "Toggle0",  "event": 1 },
    { "name": "ListOpen", "goto": 2, "transition": "bottom" },
    { "name": "Back",     "back": true },
    { "name": "Item1",    "event": 16 }
  ],
  "pages": [
    {
      "title": "mk5 demo",
      "layout": "stack",
      "gap": 6,
      "children": [
        { "button": "Alpha",  "action": "Toggle0", "tag": 1 },
        { "button": "List >", "action": "ListOpen" }
      ]
    },
    {
      "title": "List",
      "layout": "stack",
      "gap": 6,
      "scroll": true,
      "children": [
        { "button": "Item 1", "action": "Item1", "min_h": 56 },
        { "button": "< Back", "action": "Back",  "min_h": 56 }
      ]
    }
  ]
}
```

### The vocabulary

**Top level.** `device` (`width`, `height`, `corner_radius`); `orientation` (`"portrait"`, the
default, or `"landscape"`), which is the design's own copy of the layout axis so a preview matches
the device; `root`, the index of the first page; `actions`; `pages`; and optionally `extends`.

**An action** mirrors one thing the firmware can do, named once and used by any number of
buttons: `name`; `event`, a number the firmware maps to its own event; `goto`, the index of a page
to open; `back: true` to return to the parent; `transition`, the edge the opened page slides in
from (`top`/`bottom`/`left`/`right`). Two actions opening the same page with different
transitions is a compile error.

**A page** is a titled window: `title`; `layout` — `stack` (equal-height rows), `row` (equal-width
columns), `linear` (along the device's axis, so one design works in either orientation), or
`grid` (with `cols`, row-major); `gap` between children; `scroll: true` to let an overflowing stack
scroll; `subtitle: true` to reserve a second header row; `children`.

**A child** is a `button` or a `label` (the key's value is its text), or a **frame** — a container
with its own `layout`, `gap`, `cols` and `scroll` (`"vertical"` or `"horizontal"`) around a flat
list of leaves. Frames nest one level deep only. A button carries `action`, or raw
`event`/`goto`/`back` if it names no action. Any child may carry `tag` (a number the firmware uses
to find the widget later), `min_w`/`min_h`/`max_w`/`max_h` (a `min_h` is what makes a stack
overflow and scroll instead of shrinking), and `grow: true` (take the surplus in a row).

### Sharing a design across boards

A design may **extend** a parent — by the name of a crate whose `design.json` is the parent (found
under `crates/`), or by a relative path — and override only what differs. The five touch boards
share one `light_app_ui_demo/design.json`; the OLED board's design is:

```json
{
  "extends": "light_app_ui_demo_keys",
  "device": { "width": 128, "height": 64 },
  "pages": [
    {},
    { "children": [ { "min_h": 14 }, { "min_h": 14 }, { "min_h": 14 }, { "min_h": 14 }, { "min_h": 14 }, { "min_h": 14 } ] }
  ]
}
```

The merge is deep: objects by key, arrays element-wise by index (an empty `{}` leaves that element
alone, extra elements append), scalars replace. One thing it cannot express is *removing* a page
or a widget the parent has; a child that needs fewer is authored on the parent.

### The editor

```
scripts/edit-ui.ps1                          # lists every design in the tree
scripts/edit-ui.ps1 -Crate light_app_ui_demo # opens one
```

The desktop editor renders the design through the same `light-ui`/`light-draw` code the firmware
runs, so the preview is pixel-faithful. It has an edit mode (select, retitle, resize, add and
remove widgets and pages), a run mode (taps navigate as on the device), an actions mode (the
registry), and a theme mode (live colour and metric editing, saved to a `theme.json` beside the
design). It saves a design that extends a parent as the minimal override, never a flattened copy,
and never saves an edit that does not compile. [Chapter 06](06-running-without-hardware.md) has
more on the host tools.

## Driving it from Rust

The runtime side is three objects — the **display** (a panel driver over a bus, and the frame
buffer), the **frame layer** (pacing, double buffering, region flushing), and the **UI** (the
widget arena) — plus the parsed blobs. The OLED demo's wiring, in `light_app_main`:

```rust
use light_display::sh1107::Sh1107;
use light_display::{Display, FrameLayer};
use light_draw::{Flip, PixelFormat, Rotation};
use light_ui::{Lui, Ui};

const OLED_WIDTH: u16 = 64;
const OLED_HEIGHT: u16 = 128;
const UI_WIDGETS: usize = 8;                 // the most widgets any one page needs

static FRAME: ConstStaticCell<[u8; PixelFormat::Mono1.buffer_len(OLED_WIDTH, OLED_HEIGHT)]> =
        ConstStaticCell::new([0; PixelFormat::Mono1.buffer_len(OLED_WIDTH, OLED_HEIGHT)]);
static LAYER: ConstStaticCell<FrameLayer> = ConstStaticCell::new(FrameLayer::new(OLED_WIDTH, OLED_HEIGHT, PixelFormat::Mono1));
static UI: ConstStaticCell<Ui<AppEvent, UI_WIDGETS>> = ConstStaticCell::new(Ui::new());

let display = Display::new(Sh1107::new(p.oled_bus), FRAME.take(), OLED_WIDTH, OLED_HEIGHT, PixelFormat::Mono1, now_us);
let layer: &'static mut FrameLayer = LAYER.take();
let ui: &'static mut Ui<AppEvent, UI_WIDGETS> = UI.take();
let lui = Lui::parse(UI_BLOB).expect("the embedded UI parses");
```

Everything big is a `ConstStaticCell` in `.bss`: the frame buffer, the layer (it carries three
region lists) and the widget arena. `Ui<A, N>` is generic over your event type `A` and the arena
size `N`; there is no default for `N` because it is a fact about your pages.

### Building a page from the blob

A design's event numbers are the contract between the JSON and the firmware. A small function
maps each blob child to your event, and the navigation a button carries becomes an event the
display module acts on:

```rust
use light_ui::lui::code;
use light_ui::LuiChild;

fn map_child(_index: usize, child: &LuiChild) -> Option<AppEvent> {
        if child.event != 0 {
                return Some(match child.event {
                        1 => AppEvent::Toggle(0),
                        2 => AppEvent::Toggle(1),
                        16..=20 => AppEvent::Item((child.event - 15) as u8),
                        _ => return None,
                });
        }
        match child.nav {
                code::NAV_GOTO => Some(AppEvent::UiOpen(child.nav_page as u8)),
                code::NAV_BACK => Some(AppEvent::UiBack),
                _ => None,
        }
}
```

In the display module's `load`:

```rust
self.display.driver().set_display_offset(OLED_DISPLAY_OFFSET);   // a fact about this glass
self.display.init(&mut clock);
self.layer.set_orientation(Rotation::R90, Flip::None);           // a portrait glass mounted sideways
self.layer.set_frame_rate(FPS);
self.ui.fit(self.layer);                                         // size the tree to the logical canvas
let root = self.lui.root();
if let Some(page) = self.lui.page(root) {
        self.ui.build_lui_with(&page, map_child).expect("the root page builds");
}
self.ui.invalidate_all();
```

and in its `poll`, every pass:

```rust
match self.layer.poll(&mut self.display) {                       // advance any push in flight
        Ok(_) => {}
        Err(UpdateError::Timeout) => warn!("chunk timed out; update abandoned"),
        Err(UpdateError::Busy) => unreachable!(),
}
while let Some(ev) = EVENTS.poll(&self.events) {
        self.handle(ev);                                         // focus, activate, open, back, relabel
}
let style = Style::new(*self.ui.theme(), Fonts::uniform(&self.font));
self.ui.render(self.layer, &mut self.display, &style, now_us());   // a no-op unless something is dirty
if self.ui.is_dirty() || self.layer.busy(&self.display) { Poll::Busy } else { Poll::Idle }
```

Opening another page uses `navigate_lui`, which builds it with the slide the design asked for:

```rust
if let Some(page) = self.lui.page(target) {
        self.ui.navigate_lui(&page, back, None, map_child)?;    // `back` reverses the motion; None = the page's own descent
}
```

Activating a button (`self.ui.activate()` from a key, or the toolkit's own tap handling from a
touch module) returns the event the button emits; you publish it on the bus like any other, and
the module that cares reacts on its own poll. To change a widget later, find it by the `tag` the
design gave it: `self.ui.find(1).map(|id| self.ui.set_label(id, "Alpha *"))`.

The widget demo's `DisplayMod` in `crates/light_app_ui_demo` is the full version of this module,
including touch, swipe-back, rotation from an IMU and page transitions; the OLED demo's `OledMod`
in `module/light_pico2/rust` is the short one above, driven by two keys. A board that wants the
whole thing wires `DisplayMod` with a `DisplayConfig` and a `BoardHook` for the two
driver-specific edges (what to do after `init`, what to do at unload), as the touch boards do.

### Orientation

The toolkit draws in a **logical** frame that stays upright however the panel is mounted; one
transform maps it to the buffer. `layer.set_orientation(rotation, flip)` is the only place the
mounting is stated, and touch input is untransformed inside the toolkit, so a rotated device needs
no special-casing above it. A board with an IMU turns the settled orientation into a rotation with
a small `rotation_map` and the display module animates the turn.

### Panels

A panel is a `DisplayDriver` over one of the display-bus traits. The drivers that ship —
ST7789, ST7796, ST7735 and SH1107 over `SpiDisplayBus`, the AXS15231B over `QspiDisplayBus`, and
`Scanout` for an RGB panel a hardware engine streams to — are reference implementations; each
carries the facts about its part (a GDDRAM offset, an init table, a quirk) and nothing about the
chip behind the bus. [Chapter 08](08-adding-hardware.md) says what writing a new one involves.
