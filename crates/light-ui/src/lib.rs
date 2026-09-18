//! The widget toolkit, ported from the predecessor C framework.
//!
//! A retained tree of windows, buttons and labels over the frame layer. The tree is built from
//! `const` descriptors that live in flash; navigating tears the current page down and builds
//! the next, so only one page's widgets exist at a time. Widgets live in a fixed arena owned by
//! the [`Ui`] -- no allocator, and a tree too big for it is a build error at the call that adds
//! the widget, never a silent drop.
//!
//! **What comes out is an event, not a callback.** The predecessor's buttons carried a C
//! function pointer, a `void *` and a command string; here a button carries what it *emits*
//! -- a value of the application's own event type -- and, optionally, where it navigates.
//! Activation returns the emitted value for the caller to publish on its bus, which is the
//! "command tree is the event bus" decision applied to the UI: a tap, a console line and a
//! boot script all end up as the same event, and a handler runs on its own module's poll
//! rather than inside the input path.
//!
//! **Hardware-free**, as its predecessor was: nothing here knows what a touch controller, a
//! push button or an IMU is. The application maps its devices onto the input calls (a few
//! lines per app), and the toolkit can be exercised entirely on the host.
//!
//! Coordinates: every widget rect is in ABSOLUTE logical canvas coordinates, never
//! parent-relative, so a hit test, a clip and an invalidation are the same arithmetic wherever a
//! widget sits. Input arrives in PANEL coordinates -- what a touch controller reports -- and is
//! untransformed here, because the toolkit is the one thing that knows the rotation.

#![no_std]

use heapless::Vec;

use light_draw::{Rotation, Transform};
use light_display::frames::{FrameLayer, LogicalRegion, MAX_REGIONS};
use light_core::warn;
#[cfg(test)]
use light_display::Display;
#[cfg(test)]
use light_font::Font;

pub mod theme;
pub use theme::Theme;

pub mod lui;
pub use lui::{Lui, LuiChild, LuiError, LuiPage, LuiRuntime};

mod style;
pub use style::*;
mod model;
pub use model::*;
mod desc;
pub use desc::*;
mod input;
pub use input::*;
mod anim;
pub use anim::*;
mod layout;
mod scrolling;
mod nav;
mod render;



/// A widget rectangle: inclusive, logical, signed -- a widget positioned partly off the canvas
/// is clipped here before anything reaches the rasteriser.
pub type Rect = LogicalRegion;






















// --- declarative definitions ----------------------------------------------------------------




// --- errors and outcomes ----------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
        /// The arena is full: the tree needs more widgets than the `Ui` was sized for.
        Full,
        /// A page whose descriptor could not be built is not shown; the previous page is gone.
        NoContent,
}



// --- corner geometry ---------------------------------------------------------------------------
//
// There are two ways to keep something inside a corner arc, and which is right depends on the
// shape of the thing. A short string can simply start further RIGHT; a full-width row cannot,
// so it has to start further DOWN. The two helpers are the same circle solved for each axis,
// and using the wrong one makes a rounded frame either clip its content or waste a band the
// width of the radius.

fn isqrt(n: u32) -> u32 {
        if n == 0 {
                return 0;
        }
        let mut x = n;
        let mut y = (x + 1) / 2;
        while y < x {
                x = y;
                y = (x + n / x) / 2;
        }
        x
}

/// How far the arc still intrudes horizontally `dy` rows above its centre: `r - sqrt(r² - dy²)`.
/// What lets a title sit at the very TOP of a rounded frame, pushed sideways by as much as the
/// curve reaches in at its own row. Truncation errs on the safe side.
fn corner_indent(radius: u8, dy: i32) -> i32 {
        let r = i32::from(radius);
        if r <= 0 || dy <= 0 {
                return 0;
        }
        if dy >= r {
                return r;
        }
        r - isqrt((r * r - dy * dy) as u32) as i32
}

/// The mirror, for content that must span the full width: given the horizontal inset such
/// content sits at, how far down before the arc has come in that far: `sqrt(ix (2r - ix))`.
fn corner_drop(radius: u8, inset_x: i32) -> i32 {
        let r = i32::from(radius);
        let ix = inset_x;
        if r <= 0 || ix <= 0 || ix >= r {
                return 0;
        }
        r - isqrt((ix * (2 * r - ix)) as u32) as i32
}




fn rect_empty(r: &Rect) -> bool {
        r.x1 < r.x0 || r.y1 < r.y0
}

fn rect_contains(r: &Rect, x: i32, y: i32) -> bool {
        x >= r.x0 && x <= r.x1 && y >= r.y0 && y <= r.y1
}

/// Shrinks `r` to its intersection with `clip`; false when nothing survives.
fn rect_intersect(r: &mut Rect, clip: &Rect) -> bool {
        r.x0 = r.x0.max(clip.x0);
        r.y0 = r.y0.max(clip.y0);
        r.x1 = r.x1.min(clip.x1);
        r.y1 = r.y1.min(clip.y1);
        !rect_empty(r)
}

// --- the context ---------------------------------------------------------------------------------

/// A widget tree, its focus and touch state, and which parts of it changed. Owns nothing about
/// the display: it draws through whatever frame layer it is handed, contributing the tree and
/// its dirty regions. `N` is the arena size -- the most widgets one page can have.
pub struct Ui<A: 'static, const N: usize> {
        widgets: Vec<Option<Widget<A>>, N>,
        root: Option<WidgetId>,
        focused: Option<WidgetId>,
        /// Set by any invalidation, cleared once a repaint has been pushed: "is there anything to
        /// draw", which decides whether to open a frame at all.
        dirty: bool,
        /// Regions to hand the frame layer at the next repaint, collapsing to the whole canvas
        /// past the list's capacity -- it can only push more than needed, never less.
        pending: Vec<Rect, MAX_REGIONS>,
        pending_all: bool,
        /// The canvas as the toolkit last saw it: logical size and the panel transform.
        width: i32,
        height: i32,
        transform: Transform,
        /// Each role's font cell, for layout and truncation; fonts are fixed-pitch. Indexed by
        /// [`FontRole`], set at [`set_style`](Self::set_style).
        cell_w: [i32; FontRole::COUNT],
        cell_h: [i32; FontRole::COUNT],
        /// Pixels kept clear on every edge, for glass that does not show the whole grid. Uniform
        /// rather than per-edge because the interface rotates while the corners are fixed in the
        /// panel's frame: a uniform inset is the only value invariant under rotation.
        safe_inset: i32,
        /// The look-and-feel every element paints with -- see [`theme`]. Starts as the
        /// pre-theme monochrome look; an application installs its own with
        /// [`set_theme`](Self::set_theme), typically parsed from an embedded LTH blob.
        theme: Theme,
        // --- touch tracking, owned by `touch()`; everything logical ---
        touch_down: bool,
        touch_dragging: bool,
        /// The scrollable window captured when the drag engaged: the drag stays with it even if
        /// the finger wanders off, as every scrolling surface behaves.
        drag_window: Option<WidgetId>,
        touch_start: (i32, i32),
        touch_last: (i32, i32),
        /// When the contact landed, and whether it has EVER strayed beyond the slop since --
        /// a tap must both rest long enough (see [`TAP_MIN_HOLD_US`]) and never have wandered.
        touch_start_us: u64,
        touch_moved: bool,
        pub drag_slop: i32,
        /// The button wearing the just-activated flash and when it lapses (see
        /// [`ACTIVATE_FLASH_US`]): instant feedback on a tap, independent of focus, cleared by
        /// the render loop when the deadline passes or the widget goes away.
        flash: Option<(WidgetId, u64)>,
        // --- navigation ---
        page: Option<&'static Page<A>>,
        /// Where back goes when it is not the current page's parent; set only by
        /// `navigate_returning`, cleared by every ordinary navigation.
        return_page: Option<&'static Page<A>>,
        /// The direction child pages open across this whole tree, unless a page pins its own
        /// with [`Page::descend`]. `None` -- the default -- keeps the historical
        /// layout-derived flow: a `Row` page enters from the bottom ([`Descent::FromBottom`]),
        /// everything else from the right ([`Descent::FromRight`]). An app or board sets this to point the whole interface
        /// one way, and may change it live, e.g. from an orientation sensor. See [`Descent`].
        default_descent: Option<Descent>,
        /// Whether [`set_default_descent`](Self::set_default_descent) was called: an explicit
        /// application choice a theme's descent seed must not overwrite.
        default_descent_explicit: bool,
        /// The axis every [`Layout::Linear`] window runs along. `Vertical` by default; a
        /// landscape tree sets `Horizontal`. `Stack`/`Row` windows ignore it. See [`Axis`].
        layout_axis: Axis,
        // --- the rotation animation, driven from `render` ---
        //   while active, frames show the pre-rotation image turning rather than the widget
        // tree; the real rotation is applied once, on the final step. The image is captured
        // into the display's back buffer at the first step (`Display::freeze`), which is why the
        // animation needs the display and `set_rotation` does not
        rotating: bool,
        rotate_started: bool,
        rotate_target: Rotation,
        /// Total turn in degrees, signed: the shortest route between the two quadrants.
        rotate_degrees: i32,
        rotate_start_us: u64,
        pub rotate_ms: u32,
        /// A rotation asked for while a transition was running, applied once it finishes. Both
        /// animations want the back buffer and the frame, so they cannot overlap; deferring
        /// rather than dropping either means the transition is seen through and the device
        /// still ends up the right way up.
        rotate_deferred: Option<Rotation>,
        // --- the page transition, likewise ---
        //   each frame draws the incoming page and then slides the image captured before the
        // transfer off it, so the outgoing page appears to move away and reveal the new one.
        // Only the outgoing image is stored: the incoming page is the live tree, redrawn each
        // step, which is why this costs one buffer and not two
        page_moving: bool,
        page_move_started: bool,
        /// Which way the outgoing image leaves: forward pushes it toward logical -x so the new
        /// page arrives from the right; a return sends it the other way.
        page_move_back: bool,
        /// A page laid out as a `Row` runs sideways, so its transition runs the OTHER axis:
        /// entering one drops it in from the top, leaving one reveals the parent upward.
        /// The child page of the pair decides -- the incoming page going forward, the
        /// outgoing one coming back -- so one page's arrival and departure mirror.
        page_move_descent: Descent,
        /// No back buffer to capture the outgoing page into: the incoming tree slides in
        /// OVER the old image, which survives in the live buffer wherever a step has not yet
        /// overdrawn it. Chosen at the first step, from what the display can hold.
        page_move_over: bool,
        /// Region (partial) buffering: a single-buffered RGB565 display that opted in scrolls the
        /// outgoing image off the live buffer in place (a reveal for every direction, since one
        /// buffer cannot slide the incoming IN), rather than the over-mode full repaint. Chosen at
        /// the first step. Mutually exclusive with `page_move_over`.
        page_move_region: bool,
        /// Physical unit direction, derived from the logical one at the first step -- or the
        /// LOGICAL sign of the incoming page's offset in the slide-over mode, which draws
        /// through the canvas transform and never leaves logical space.
        page_move_dx: i32,
        page_move_dy: i32,
        /// The travel of the last DRAWN capture-mode step: the incoming page is static in
        /// that mode, so each step paints only the band uncovered since this mark.
        page_move_travel: i32,
        /// How far it travels to leave: the buffer's extent along that axis.
        page_move_span: i32,
        page_move_start_us: u64,
        pub page_move_ms: u32,
}

impl<A: Copy, const N: usize> Ui<A, N> {
        /// An empty toolkit. `const`, so it can be a `static` initialised in place -- the arena is
        /// the biggest object in an application, and a firmware's core 0 stack is 4 KB. Call
        /// [`set_font`](Self::set_font) and [`fit`](Self::fit) before the first build.
        pub const fn new() -> Self {
                Self {
                        widgets: Vec::new(),
                        root: None,
                        focused: None,
                        dirty: false,
                        pending: Vec::new(),
                        pending_all: false,
                        width: 0,
                        height: 0,
                        transform: Transform::IDENTITY,
                        cell_w: [0; FontRole::COUNT],
                        cell_h: [0; FontRole::COUNT],
                        safe_inset: 0,
                        theme: Theme::DEFAULT,
                        touch_down: false,
                        touch_dragging: false,
                        drag_window: None,
                        touch_start: (0, 0),
                        touch_last: (0, 0),
                        touch_start_us: 0,
                        touch_moved: false,
                        drag_slop: DRAG_SLOP,
                        flash: None,
                        page: None,
                        return_page: None,
                        default_descent: None,
                        default_descent_explicit: false,
                        layout_axis: Axis::Vertical,
                        rotating: false,
                        rotate_started: false,
                        rotate_target: Rotation::R0,
                        rotate_degrees: 0,
                        rotate_start_us: 0,
                        rotate_ms: ROTATE_MS,
                        rotate_deferred: None,
                        page_moving: false,
                        page_move_started: false,
                        page_move_back: false,
                        page_move_descent: Descent::FromRight,
                        page_move_over: false,
                        page_move_region: false,
                        page_move_dx: 0,
                        page_move_dy: 0,
                        page_move_travel: 0,
                        page_move_span: 0,
                        page_move_start_us: 0,
                        page_move_ms: PAGE_MOVE_MS,
                }
        }






        /// Take the canvas geometry from the layer: its logical size and transform. Re-lays-out and
        /// repaints everything when the size changed, which is what a rotation does.
        pub fn fit(&mut self, layer: &FrameLayer) {
                let (w, h) = layer.logical_size();
                let (w, h) = (i32::from(w), i32::from(h));
                let changed = w != self.width || h != self.height;
                self.width = w;
                self.height = h;
                self.transform = layer.transform();
                if changed {
                        self.relayout();
                        self.invalidate_all();
                }
        }

        pub fn get(&self, id: WidgetId) -> Option<&Widget<A>> {
                self.widgets.get(usize::from(id.0)).and_then(|s| s.as_ref())
        }

        fn w(&self, id: WidgetId) -> &Widget<A> {
                self.get(id).expect("a live widget")
        }

        fn w_mut(&mut self, id: WidgetId) -> &mut Widget<A> {
                self.widgets[usize::from(id.0)].as_mut().expect("a live widget")
        }

        pub fn root(&self) -> Option<WidgetId> {
                self.root
        }

        pub fn focused(&self) -> Option<WidgetId> {
                self.focused
        }


        /// The first live widget carrying `tag`.
        pub fn find(&self, tag: u8) -> Option<WidgetId> {
                self.widgets.iter().enumerate().find_map(|(i, s)| match s {
                        Some(w) if w.tag == tag && tag != 0 => Some(WidgetId(i as u8)),
                        _ => None,
                })
        }


        pub fn logical_size(&self) -> (i32, i32) {
                (self.width, self.height)
        }

        // --- tree ---

        fn alloc(&mut self, w: Widget<A>) -> Result<WidgetId, Error> {
                if let Some(i) = self.widgets.iter().position(|s| s.is_none()) {
                        self.widgets[i] = Some(w);
                        return Ok(WidgetId(i as u8));
                }
                if self.widgets.len() >= 255 {
                        return Err(Error::Full);
                }
                self.widgets.push(Some(w)).map_err(|_| Error::Full)?;
                Ok(WidgetId((self.widgets.len() - 1) as u8))
        }

        fn add(&mut self, parent: Option<WidgetId>, kind: Kind<A>, rect: Rect, focusable: bool) -> Result<WidgetId, Error> {
                let id = self.alloc(Widget { kind, text: TextSlot::EMPTY, rect, visible: true, focusable, enabled: true, hit_slop_y1: 0, min_w: 0, min_h: 0, max_w: 0, max_h: 0, grow: false, tag: 0, parent, next_sibling: None, first_child: None })?;
                match parent {
                        None => {
                                if let Some(old) = self.root {
                                        warn!("ui already has a root widget; replacing it");
                                        self.destroy(old);
                                }
                                self.root = Some(id);
                        }
                        Some(p) => {
                                // appended: sibling order is paint order and focus order, and
                                // neither reads correctly reversed
                                match self.w(p).first_child {
                                        None => self.w_mut(p).first_child = Some(id),
                                        Some(mut c) => {
                                                while let Some(n) = self.w(c).next_sibling {
                                                        c = n;
                                                }
                                                self.w_mut(c).next_sibling = Some(id);
                                        }
                                }
                        }
                }
                Ok(id)
        }






        /// Free a widget and everything under it, unlinking it first. Focus and the drag target
        /// are cleared if they pointed inside. The only correct way to release a widget.
        pub fn destroy(&mut self, id: WidgetId) {
                if self.get(id).is_none() {
                        return;
                }
                // unlinked BEFORE anything is freed, so the tree never holds a stale handle
                match self.w(id).parent {
                        None => {
                                if self.root == Some(id) {
                                        self.root = None;
                                }
                        }
                        Some(p) => {
                                let next = self.w(id).next_sibling;
                                if self.w(p).first_child == Some(id) {
                                        self.w_mut(p).first_child = next;
                                } else {
                                        let mut c = self.w(p).first_child;
                                        while let Some(cid) = c {
                                                if self.w(cid).next_sibling == Some(id) {
                                                        self.w_mut(cid).next_sibling = next;
                                                        break;
                                                }
                                                c = self.w(cid).next_sibling;
                                        }
                                }
                        }
                }
                self.destroy_subtree(id);
                // whatever the subtree occupied has to be repainted; the widget that owned that
                // area no longer exists to invalidate it
                self.invalidate_all();
        }

        fn destroy_subtree(&mut self, id: WidgetId) {
                let mut c = self.w(id).first_child;
                while let Some(cid) = c {
                        c = self.w(cid).next_sibling;
                        self.destroy_subtree(cid);
                }
                if self.focused == Some(id) {
                        self.focused = None;
                }
                if self.drag_window == Some(id) {
                        self.drag_window = None;
                }
                if matches!(self.flash, Some((f, _)) if f == id) {
                        self.flash = None;
                }
                self.widgets[usize::from(id.0)] = None;
        }

        /// Next widget in depth-first pre-order -- paint order and focus order -- or `None` once
        /// the walk has left the subtree at `root`.
        fn next(&self, mut id: WidgetId, root: WidgetId) -> Option<WidgetId> {
                if let Some(c) = self.w(id).first_child {
                        return Some(c);
                }
                loop {
                        if id == root {
                                return None;
                        }
                        if let Some(n) = self.w(id).next_sibling {
                                return Some(n);
                        }
                        id = self.w(id).parent?;
                }
        }

        /// Every widget in pre-order from the root.
        fn walk(&self) -> impl Iterator<Item = WidgetId> + '_ {
                let root = self.root;
                let mut cur = root;
                core::iter::from_fn(move || {
                        let id = cur?;
                        cur = self.next(id, root?);
                        Some(id)
                })
        }

        fn children(&self, id: WidgetId) -> impl Iterator<Item = WidgetId> + '_ {
                let mut cur = self.w(id).first_child;
                core::iter::from_fn(move || {
                        let c = cur?;
                        cur = self.w(c).next_sibling;
                        Some(c)
                })
        }

        /// The direct children of `id`, in build order. For walking a built tree from outside -- an
        /// editor correlating each widget with the design node that produced it (the two are 1:1 and
        /// in the same order), to hit-test a point or outline a selection, frames included.
        pub fn child_ids(&self, id: WidgetId) -> impl Iterator<Item = WidgetId> + '_ {
                self.children(id)
        }

        // --- navigation ---











        // --- geometry ---




















        /// Keep `inset` pixels clear on every edge of the canvas. On rounded glass this is
        /// a small BREATHING MARGIN, not the corner radius: the curve is carried by the
        /// root window's own corner radius (the glass's measured radius minus this inset --
        /// insetting a rounded rectangle by d leaves a rounded rectangle of radius r - d).
        /// Setting the full glass radius here is the superseded approach that gave up a
        /// whole band on every edge to keep a SQUARE frame inside round glass.
        /// Install a look-and-feel; every element repaints with it. Install it BEFORE
        /// building pages: container corner radii resolve from the theme when a window is
        /// created, so a theme swapped under a live tree recolors it but keeps its
        /// geometry until the next page build. Typically parsed from
        /// an embedded LTH blob ([`Theme::parse`]) -- theming is a data change, never an
        /// edit to this crate.
        pub fn set_theme(&mut self, theme: Theme) {
                if self.theme == theme {
                        return;
                }
                self.theme = theme;
                //   a theme may seed the tree's descent -- but only as a starting value: an
                // application that called set_default_descent has spoken, and keeps its choice
                if !self.default_descent_explicit {
                        if let Some(d) = theme.descent {
                                self.default_descent = Some(d);
                        }
                }
                self.invalidate_all();
        }

        pub fn theme(&self) -> &Theme {
                &self.theme
        }

        /// Shade the focused widget's fill with a vertical gradient, or `None` for the
        /// solid inversion -- a live handle on the theme's [`theme::key::FOCUS_SURFACE`],
        /// which is where the value now lives.
        pub fn set_focus_shade(&mut self, shade: Option<Shade>) {
                if self.theme.focus_surface == shade {
                        return;
                }
                self.theme.focus_surface = shade;
                self.invalidate_all();
        }

        pub fn set_safe_inset(&mut self, inset: u8) {
                if self.safe_inset == i32::from(inset) {
                        return;
                }
                self.safe_inset = i32::from(inset);
                self.relayout();
                self.invalidate_all();
        }






        // --- invalidation ---



        // --- focus and activation ---








        // --- input ---










        // --- mutators ---

        pub fn set_visible(&mut self, id: WidgetId, visible: bool) {
                if self.w(id).visible == visible {
                        return;
                }
                self.invalidate_widget(id);
                self.w_mut(id).visible = visible;
                self.invalidate_widget(id);
                if !visible && self.focused == Some(id) {
                        self.focus_next();
                }
        }

        pub fn set_enabled(&mut self, id: WidgetId, enabled: bool) {
                if self.w(id).enabled == enabled {
                        return;
                }
                self.w_mut(id).enabled = enabled;
                self.invalidate_widget(id);
                if !enabled && self.focused == Some(id) {
                        self.focus_next();
                }
        }

        /// Constraints take effect on the next layout pass -- set them in a batch, then relayout.
        pub fn set_min_size(&mut self, id: WidgetId, w: i32, h: i32) {
                let x = self.w_mut(id);
                x.min_w = w;
                x.min_h = h;
        }

        pub fn set_max_size(&mut self, id: WidgetId, w: i32, h: i32) {
                let x = self.w_mut(id);
                x.max_w = w;
                x.max_h = h;
        }

        pub fn set_label(&mut self, id: WidgetId, label: &'static str) {
                match &mut self.w_mut(id).kind {
                        Kind::Button(b) => b.label = label,
                        Kind::Label(l) => l.text = label,
                        Kind::Window(w) => w.title = Some(label),
                }
                //   a static label supersedes any runtime text
                self.w_mut(id).text = TextSlot::EMPTY;
                self.invalidate_widget(id);
        }

        /// Runtime text for a widget: copied into the widget (up to [`TextSlot::CAP`] bytes,
        /// truncated at a char boundary), shown in place of the static label until
        /// [`set_label`](Self::set_label) or an empty string clears it. This is how a label
        /// carries a formatted value -- an elapsed time, a file name -- which a
        /// `&'static str` cannot. On a window it replaces the TITLE, and only on a window
        /// built with one: an untitled frame reserves no header band to draw into.
        pub fn set_text(&mut self, id: WidgetId, text: &str) {
                self.w_mut(id).text.set(text);
                self.invalidate_widget(id);
        }

        /// The text a widget currently SHOWS: its runtime text if [`set_text`](Self::set_text)
        /// gave it one, otherwise its static label/title (a window with no title reads empty).
        /// `None` for an id that no longer exists. Lets a caller read a list row or label back --
        /// what the viewer sees -- without reaching into the widget.
        pub fn widget_text(&self, id: WidgetId) -> Option<&str> {
                let w = self.get(id)?;
                if w.text.len > 0 {
                        return Some(w.text.as_str());
                }
                Some(match &w.kind {
                        Kind::Button(b) => b.label,
                        Kind::Label(l) => l.text,
                        Kind::Window(win) => win.title.unwrap_or(""),
                })
        }

        /// Set one row of a [`file_list!`](crate::file_list) by its tag (`tag_base + row`),
        /// showing `placeholder` for an empty string. This is the by-tag address and the
        /// empty-slot convention a selectable list needs in one place; a no-op if that row is
        /// not built (the list's page is not the one showing).
        pub fn set_list_text(&mut self, tag_base: u8, row: u8, text: &str, placeholder: &'static str) {
                if let Some(id) = self.find(tag_base.wrapping_add(row)) {
                        if text.is_empty() {
                                self.set_label(id, placeholder);
                        } else {
                                self.set_text(id, text);
                        }
                }
        }

        /// Set one row of a [`file_list!`](crate::file_list) by its tag, HIDING the row when `text`
        /// is empty instead of showing a placeholder -- so a partly-filled list collapses to just
        /// its entries. [`Relayout`](Self::relayout) after a batch of these for the freed space to
        /// close up. A no-op if that row is not built.
        pub fn set_list_row(&mut self, tag_base: u8, row: u8, text: &str) {
                if let Some(id) = self.find(tag_base.wrapping_add(row)) {
                        let filled = !text.is_empty();
                        self.set_visible(id, filled);
                        if filled {
                                self.set_text(id, text);
                        }
                }
        }

        /// Fill a whole [`file_list!`](crate::file_list) from `entries`, one per row from row 0,
        /// `placeholder` for an empty entry -- for a caller that holds the entire list rather
        /// than feeding rows one event at a time. Rows past `entries` are left as they are.
        pub fn fill_list(&mut self, tag_base: u8, entries: &[&str], placeholder: &'static str) {
                for (row, text) in entries.iter().enumerate() {
                        self.set_list_text(tag_base, row as u8, text, placeholder);
                }
        }

        /// The title-bar status symbol on a window: `Some((shape, lit, col))` places
        /// [`shape`](IndicatorShape) on title character cell `col`, `lit` choosing drawn or
        /// dark; `None` is off. A flashing light toggles `lit` at a fixed `col`, and since the
        /// symbol sits on a blank cell the title already holds the text never reflows. A no-op
        /// on a non-window.
        pub fn set_indicator(&mut self, id: WidgetId, state: Option<(IndicatorShape, bool, u16)>) {
                if let Kind::Window(w) = &mut self.w_mut(id).kind {
                        if w.indicator == state {
                                return;
                        }
                        w.indicator = state;
                        self.invalidate_widget(id);
                }
        }

        /// Set the second title row's text on a window built with [`Desc::subtitle`]. A no-op on
        /// a one-row window (or a non-window); an empty string draws a blank lower row without
        /// changing the band height, so a status that comes and goes never shifts the content.
        pub fn set_subtitle(&mut self, id: WidgetId, text: &str) {
                if let Kind::Window(w) = &mut self.w_mut(id).kind {
                        if let Some(slot) = w.subtitle.as_mut() {
                                slot.set(text);
                                self.invalidate_widget(id);
                        }
                }
        }

        // --- painting ---












}

/// Building a page from an LUI blob into a widget tree of ANY event type. A button's emitted value
/// comes from the caller's `emit` closure, mapping the blob child (and its index) to the app's own
/// event -- so a typed app (`Ui<AppEvent>`) drives a design blob while keeping its events, and the
/// `Ui<u16>` [`build_lui`](Ui::build_lui) wrapper is just the identity mapping to the child index.
impl<A: Copy + 'static, const N: usize> Ui<A, N> {
        /// Build `page` (from a parsed [`Lui`] blob) into the widget tree, replacing any current
        /// page and laying it out to the canvas. Each button emits `emit(index, child)`. The blob
        /// must be `'static` -- its strings become the widgets' `&'static str`. There is no page
        /// transition; this is an in-place build.
        pub fn build_lui_with(&mut self, page: &LuiPage<'static>, emit: impl Fn(usize, &LuiChild<'static>) -> Option<A>) -> Result<(), Error> {
                if let Some(root) = self.root {
                        self.destroy(root);
                }
                let win = self.create_window(None, Rect::new(0, 0, 0, 0), Some(page.title()), page.subtitle())?;
                //   scroll before the children, since it changes how the stack lays out rows that
                // do not fit (mirrors build_desc)
                if page.scroll() {
                        if let Some(w) = self.w_mut(win).window_mut() {
                                w.scroll = scroll::VERTICAL;
                        }
                }
                //   a single running index over every widget built, so a button's emit value and a
                // default tag (index + 1, when the design gives none) stay unique across a frame's
                // children too; on a flat page it is just the child position, as before
                let mut index = 0usize;
                for child in page.children() {
                        if child.kind == lui::code::KIND_FRAME {
                                //   a frame is a titleless window with its own layout: create it,
                                // size it, fill it with its (leaf) children, then lay it out. It
                                // takes ONLY an explicit tag -- a structural container is never
                                // addressed by position, and an auto tag would collide with an
                                // app's own (a frame landing on TAG_PLAY once wore "Play last" as a
                                // title). It also consumes no emit index, for the same reason
                                let frame = self.create_window(Some(win), Rect::new(0, 0, 0, 0), None, false)?;
                                if child.scroll() != 0 {
                                        if let Some(w) = self.w_mut(frame).window_mut() {
                                                w.scroll = child.scroll();
                                        }
                                }
                                self.apply_lui_size(frame, &child);
                                self.w_mut(frame).tag = child.tag;
                                for sub in child.children() {
                                        let id = self.create_lui_leaf(frame, &sub, index, &emit)?;
                                        self.apply_lui_leaf_box(id, &sub, index);
                                        index += 1;
                                }
                                match child.layout() {
                                        lui::code::LAYOUT_ROW => self.layout_row(frame, child.gap()),
                                        lui::code::LAYOUT_LINEAR => self.layout_linear(frame, child.gap()),
                                        _ => self.layout_stack(frame, child.gap()),
                                }
                        } else {
                                let id = self.create_lui_leaf(win, &child, index, &emit)?;
                                self.apply_lui_leaf_box(id, &child, index);
                                index += 1;
                        }
                }
                match page.layout() {
                        lui::code::LAYOUT_ROW => self.layout_row(win, page.gap()),
                        lui::code::LAYOUT_LINEAR => self.layout_linear(win, page.gap()),
                        _ => self.layout_stack(win, page.gap()),
                }
                self.relayout();
                Ok(())
        }

        /// Create a leaf child (a button or label) under `parent`, its button emitting
        /// `emit(index, child)`.
        fn create_lui_leaf(&mut self, parent: WidgetId, child: &LuiChild<'static>, index: usize, emit: &impl Fn(usize, &LuiChild<'static>) -> Option<A>) -> Result<WidgetId, Error> {
                match child.kind {
                        lui::code::KIND_LABEL => self.create_label(Some(parent), Rect::new(0, 0, 0, 0), child.text),
                        //   a button, or any unknown kind treated as one; its emitted value is the
                        // caller's to decide from the child
                        _ => self.create_button(Some(parent), Rect::new(0, 0, 0, 0), child.text, emit(index, child), Nav::Stay),
                }
        }

        /// Apply a blob child's size metrics (min/max, grow) to a built widget.
        fn apply_lui_size(&mut self, id: WidgetId, child: &LuiChild<'static>) {
                let w = self.w_mut(id);
                w.min_w = i32::from(child.min_w);
                w.min_h = i32::from(child.min_h);
                w.max_w = i32::from(child.max_w);
                w.max_h = i32::from(child.max_h);
                w.grow = child.grow;
        }

        /// A leaf's size plus its tag -- the design's, or `index + 1` as a default (tag 0 stays
        /// "untagged"), so a caller can find a leaf by a stable id or by position. Only leaves get
        /// the positional default; a frame takes an explicit tag only (see `build_lui_with`).
        fn apply_lui_leaf_box(&mut self, id: WidgetId, child: &LuiChild<'static>, index: usize) {
                self.apply_lui_size(id, child);
                self.w_mut(id).tag = if child.tag != 0 { child.tag } else { (index + 1) as u8 };
        }

        /// Navigate to an LUI-blob `page` WITH the page-transition animation -- the blob analogue of
        /// [`navigate`](Self::navigate) (`back` false) and [`navigate_back`](Self::navigate_back)
        /// (`back` true). The blob carries no parent links, so the caller keeps its own history and
        /// names both the target page and the direction. Buttons emit `emit(index, child)`, exactly
        /// as [`build_lui_with`](Self::build_lui_with) builds them; the difference is the slide.
        ///
        /// `descent` is the transition -- the edge the incoming page enters from -- which `back`
        /// reverses to mirror. A caller passes the target page's own [`descent`](LuiPage::descent)
        /// going forward, and the LEAVING page's going back, so a page departs the way it arrived.
        /// `None` falls back to [`set_default_descent`](Self::set_default_descent), then to a seed
        /// from the entering page's layout and the tree's [`layout_axis`](Self::set_layout_axis) -- a
        /// `Row`, or a `Linear` tree set horizontal, rises from the bottom; anything else slides in
        /// from the right.
        pub fn navigate_lui(&mut self, page: &LuiPage<'static>, back: bool, descent: Option<Descent>, emit: impl Fn(usize, &LuiChild<'static>) -> Option<A>) -> Result<(), Error> {
                //   start the transition before the tree changes, as show_page does for a const
                // Page: the outgoing image is captured at the first render step, off the live panel,
                // so destroying the old widget tree now (inside build_lui_with) is fine
                if self.root.is_some() && !self.rotating {
                        self.page_moving = true;
                        self.page_move_started = false;
                        self.page_move_back = back;
                        let horizontal = match page.layout() {
                                lui::code::LAYOUT_ROW => true,
                                lui::code::LAYOUT_LINEAR => self.layout_axis == Axis::Horizontal,
                                _ => false,
                        };
                        let seed = if horizontal { Descent::FromBottom } else { Descent::FromRight };
                        self.page_move_descent = descent.or(self.default_descent).unwrap_or(seed);
                }
                self.build_lui_with(page, emit)?;
                self.invalidate_all();
                Ok(())
        }
}

/// The `Ui<u16>` convenience over [`build_lui_with`](Ui::build_lui_with): a blob UI whose event type
/// is `u16` -- each button emits its child index, which [`LuiRuntime`](crate::LuiRuntime) resolves
/// back to the blob child's navigation and app event.
impl<const N: usize> Ui<u16, N> {
        pub fn build_lui(&mut self, page: &LuiPage<'static>) -> Result<(), Error> {
                self.build_lui_with(page, |i, _| Some(i as u16))
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        use light_display::{DisplayDriver, Frame, Region};
        use light_draw::PixelFormat;
        use light_core::hal::Clock;
        use light_font::Encoder;
        extern crate std;
        use std::vec::Vec as StdVec;

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Ev {
                Alpha,
                Beta,
                Item(u8),
        }

        static BTN_ALPHA: Desc<Ev> = Desc::button("Alpha").emit(Ev::Alpha);
        static BTN_BETA: Desc<Ev> = Desc::button("Beta").emit(Ev::Beta);
        static BTN_MORE: Desc<Ev> = Desc::button("More >").navigate(&PAGE_DETAIL);
        static MAIN: Desc<Ev> = Desc::window("Main").rounded(8).stack(2).children(&[&BTN_ALPHA, &BTN_BETA, &BTN_MORE]);
        static LBL: Desc<Ev> = Desc::label("swipe right to go back");
        static BTN_BACK: Desc<Ev> = Desc::button("< Back").back();
        static DETAIL: Desc<Ev> = Desc::window("More").stack(1).children(&[&LBL, &BTN_BACK]);
        static PAGE_MAIN: Page<Ev> = Page::new(&MAIN, None);
        static PAGE_DETAIL: Page<Ev> = Page::new(&DETAIL, Some(&PAGE_MAIN));

        static ITEM_1: Desc<Ev> = Desc::button("Item 1").emit(Ev::Item(1)).min_size(0, 20);
        static ITEM_2: Desc<Ev> = Desc::button("Item 2").emit(Ev::Item(2)).min_size(0, 20);
        static ITEM_3: Desc<Ev> = Desc::button("Item 3").emit(Ev::Item(3)).min_size(0, 20);
        static ITEM_4: Desc<Ev> = Desc::button("Item 4").emit(Ev::Item(4)).min_size(0, 20);
        static ITEM_5: Desc<Ev> = Desc::button("Item 5").emit(Ev::Item(5)).min_size(0, 20);
        static LIST: Desc<Ev> = Desc::frame().stack(0).scroll(scroll::VERTICAL).children(&[&ITEM_1, &ITEM_2, &ITEM_3, &ITEM_4, &ITEM_5]);
        static PAGE_LIST: Page<Ev> = Page::new(&LIST, None);

        static COL_1: Desc<Ev> = Desc::button("C1").emit(Ev::Item(1)).min_size(20, 0);
        static COL_2: Desc<Ev> = Desc::button("C2").emit(Ev::Item(2)).min_size(20, 0);
        static COL_3: Desc<Ev> = Desc::button("C3").emit(Ev::Item(3)).min_size(20, 0);
        static COL_4: Desc<Ev> = Desc::button("C4").emit(Ev::Item(4)).min_size(20, 0);
        static COL_5: Desc<Ev> = Desc::button("C5").emit(Ev::Item(5)).min_size(20, 0);
        static ROW_LIST: Desc<Ev> = Desc::frame().row(0).scroll(scroll::HORIZONTAL).children(&[&COL_1, &COL_2, &COL_3, &COL_4, &COL_5]);
        static PAGE_ROW: Page<Ev> = Page::new(&ROW_LIST, None);

        static PIN: Desc<Ev> = Desc::button("<").emit(Ev::Beta).min_size(12, 0).max_size(12, 0);
        static STRIP: Desc<Ev> = Desc::frame().row(0).scroll(scroll::HORIZONTAL).children(&[&COL_1, &COL_2, &COL_3, &COL_4, &COL_5]);
        static PINNED: Desc<Ev> = Desc::frame().row(2).children(&[&PIN, &STRIP]);
        static PAGE_PINNED: Page<Ev> = Page::new(&PINNED, None);

        //   a grow child flanked by pinned columns on BOTH sides, and the same row without the
        // grow (the last-absorbs-remainder default) as its control
        static PIN_R: Desc<Ev> = Desc::button(">").emit(Ev::Alpha).min_size(12, 0).max_size(12, 0);
        static GROW_STRIP: Desc<Ev> = Desc::frame().grow().row(0).scroll(scroll::HORIZONTAL).children(&[&COL_1, &COL_2, &COL_3, &COL_4, &COL_5]);
        static GROW_ROW: Desc<Ev> = Desc::frame().row(2).children(&[&PIN, &GROW_STRIP, &PIN_R]);
        static PAGE_GROW: Page<Ev> = Page::new(&GROW_ROW, None);
        static NOGROW_STRIP: Desc<Ev> = Desc::frame().row(0).scroll(scroll::HORIZONTAL).children(&[&COL_1, &COL_2, &COL_3, &COL_4, &COL_5]);
        static NOGROW_ROW: Desc<Ev> = Desc::frame().row(2).children(&[&PIN, &NOGROW_STRIP, &PIN_R]);
        static PAGE_NOGROW: Page<Ev> = Page::new(&NOGROW_ROW, None);

        //   the same stack content (layout seed would be Left) pinned to each cardinal, to
        // prove a per-page override beats the seed and the tree default
        static PAGE_D_BOTTOM: Page<Ev> = Page::new(&DETAIL, Some(&PAGE_MAIN)).descend(Descent::FromBottom);
        static PAGE_D_TOP: Page<Ev> = Page::new(&DETAIL, Some(&PAGE_MAIN)).descend(Descent::FromTop);
        static PAGE_D_RIGHT: Page<Ev> = Page::new(&DETAIL, Some(&PAGE_MAIN)).descend(Descent::FromRight);
        static PAGE_D_LEFT: Page<Ev> = Page::new(&DETAIL, Some(&PAGE_MAIN)).descend(Descent::FromLeft);

        //   generic layouts: the same descriptor lays out along whichever axis the tree carries
        static LINEAR_WIN: Desc<Ev> = Desc::window("Lin").linear(2).children(&[&BTN_ALPHA, &BTN_BETA]);
        static PAGE_LINEAR: Page<Ev> = Page::new(&LINEAR_WIN, None);
        static LINEAR_WIN_2: Desc<Ev> = Desc::window("Lin2").linear(2).children(&[&BTN_ALPHA, &BTN_BETA]);
        static PAGE_LINEAR_2: Page<Ev> = Page::new(&LINEAR_WIN_2, None);

        //   the reusable selectable-list component: rows generated once, each tagged and
        // emitting its own index. PICK_ROWS_BACK also appends a back row.
        crate::file_list! {
                PICK_ROWS,
                event: Ev,
                tag_base: 0x30u8,
                min_size: (0, 20),
                select: |i| Ev::Item(i),
                indices: [0, 1, 2],
        }
        static PICK_WIN: Desc<Ev> = Desc::window("Pick").stack(0).children(PICK_ROWS);
        static PICK_PAGE: Page<Ev> = Page::new(&PICK_WIN, None);
        crate::file_list! {
                PICK_ROWS_BACK,
                event: Ev,
                tag_base: 0x40u8,
                min_size: (0, 20),
                select: |i| Ev::Item(i),
                indices: [0, 1],
                back: &BTN_BACK,
        }

        //   a two-row title bar (a subtitle) vs the ordinary one-row bar, same content
        static SUB_WIN: Desc<Ev> = Desc::window("Bar").subtitle().stack(0).children(&[&BTN_ALPHA]);
        static SUB_PAGE: Page<Ev> = Page::new(&SUB_WIN, None);
        static PLAIN_WIN: Desc<Ev> = Desc::window("Bar").stack(0).children(&[&BTN_ALPHA]);
        static PLAIN_PAGE: Page<Ev> = Page::new(&PLAIN_WIN, None);

        fn font_blob() -> StdVec<u8> {
                let mut e = Encoder::new(4, 6, 5, 6);
                for c in 0x20u8..0x7f {
                        e.add(c, &[0xF0; 6]).unwrap();
                }
                e.encode()
        }

        /// A style over one font for every role, on the default theme -- what these tests bind and
        /// render with; a test that wants a specific theme sets it separately with `set_theme`,
        /// which `render` reads from the [`Ui`], not from this style's theme field.
        fn styled<'a>(font: &'a Font<'a>) -> Style<'a> {
                Style::new(Theme::DEFAULT, Fonts::uniform(font))
        }

        struct Mock {
                pushed: StdVec<Region>,
        }
        impl DisplayDriver for Mock {
                fn init(&mut self, _: &mut dyn Clock, _: u16, _: u16) {}
                fn chunk_count(&self, _: &Region) -> u16 {
                        1
                }
                fn chunks_per_poll(&self, _: &Region) -> u16 {
                        0
                }
                fn kick(&mut self, _: &Frame<'_>, r: &Region, _: u16) {
                        self.pushed.push(*r);
                }
                fn chunk_complete(&mut self) -> bool {
                        true
                }
                fn chunk_timeout_ms(&self) -> u32 {
                        10
                }
        }

        fn now() -> u64 {
                0
        }

        /// A 64x48 mono rig, like a small OLED.
        fn rig(buf: &mut [u8]) -> (FrameLayer, Display<'_, Mock>) {
                let display = Display::new(Mock { pushed: StdVec::new() }, buf, 64, 48, PixelFormat::Mono1, now);
                (FrameLayer::new(64, 48, PixelFormat::Mono1), display)
        }

        fn flush(layer: &mut FrameLayer, display: &mut Display<'_, Mock>) -> StdVec<Region> {
                while layer.poll(display).unwrap() {}
                core::mem::take(&mut display.driver().pushed)
        }

        #[test]
        fn a_stack_divides_the_content_area_and_the_last_row_takes_the_remainder() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_MAIN).unwrap();
                let root = ui.root().unwrap();
                let rows: StdVec<Rect> = ui.children(root).map(|c| ui.get(c).unwrap().rect).collect();
                assert_eq!(rows.len(), 3);
                // rows abut with the gap between, span the content width, and are in order
                assert_eq!(rows[1].y0, rows[0].y1 + 3);
                assert_eq!(rows[2].y0, rows[1].y1 + 3);
                assert!(rows[0].x0 == rows[1].x0 && rows[0].x1 == rows[2].x1);
                // the last row reaches the flush edge of the rounded frame (y1 - inset_x)
                assert_eq!(rows[2].y1, 47 - 3);
                // and the whole thing sits below the header band: border + cell + 2
                assert!(rows[0].y0 >= 1 + 6 + 2);
                // the flush last row is a rounded-bottom button with hit slop to the canvas edge
                let last = ui.children(root).last().unwrap();
                let b = ui.get(last).unwrap();
                assert_eq!(b.button().unwrap().corners, light_draw::corner::BOTTOM);
                assert_eq!(b.hit_slop_y1, 3);
        }

        #[test]
        fn focus_cycles_through_buttons_in_order_and_wraps() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_MAIN).unwrap();
                // the first button took focus on creation
                assert_eq!(ui.activate(), Some(Ev::Alpha));
                ui.focus_next();
                assert_eq!(ui.activate(), Some(Ev::Beta));
                ui.focus_prev();
                ui.focus_prev();
                // wrapped from Alpha to the last button, which navigates
                assert_eq!(ui.activate(), None);
                assert!(core::ptr::eq(ui.page().unwrap(), &PAGE_DETAIL));
                // the new page's first button has focus; back returns to Main
                assert_eq!(ui.activate(), None);
                assert!(core::ptr::eq(ui.page().unwrap(), &PAGE_MAIN));
                assert!(!ui.navigate_back(), "top-level page: nowhere to go");
        }

        #[test]
        fn a_tap_activates_on_release_at_its_start_point_and_a_wander_does_not() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_MAIN).unwrap();
                let root = ui.root().unwrap();
                let beta = ui.children(root).nth(1).unwrap();
                let r = ui.get(beta).unwrap().rect;
                let (cx, cy) = (((r.x0 + r.x1) / 2) as u16, ((r.y0 + r.y1) / 2) as u16);
                assert_eq!(ui.touch(cx, cy, true, 0), Touch::Pending);
                // a wobble within the slop, held past the minimum, is still a tap
                assert_eq!(ui.touch(cx + 3, cy + 2, true, 10_000), Touch::Pending);
                assert_eq!(ui.touch(cx + 3, cy + 2, false, 60_000), Touch::Tap { hit: true, emitted: Some(Ev::Beta) });
                assert_eq!(ui.focused(), Some(beta));
                // travel beyond the slop with nothing scrollable underneath: neither tap nor drag
                assert_eq!(ui.touch(cx, cy, true, 100_000), Touch::Pending);
                assert_eq!(ui.touch(cx + 30, cy, true, 110_000), Touch::Pending);
                assert_eq!(ui.touch(cx + 30, cy, false, 200_000), Touch::None);
                // a press too brief to be deliberate is dropped, even dead on the widget
                assert_eq!(ui.touch(cx, cy, true, 300_000), Touch::Pending);
                assert_eq!(ui.touch(cx, cy, false, 300_000 + TAP_MIN_HOLD_US / 2), Touch::None);
                // and a real tap on empty space (the header) is reported, not swallowed
                assert_eq!(ui.touch(30, 2, true, 400_000), Touch::Pending);
                assert_eq!(ui.touch(30, 2, false, 460_000), Touch::Tap { hit: false, emitted: None });
        }

        #[test]
        fn a_scrolling_stack_overflows_and_a_drag_moves_it_within_the_clamp() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_LIST).unwrap();
                let root = ui.root().unwrap();
                let win = ui.get(root).unwrap().window().unwrap().clone();
                // five rows pinned at 20 px in a 48 px frame: the content overflows
                assert_eq!(win.content_h, 100);
                let first = ui.children(root).next().unwrap();
                let y_before = ui.get(first).unwrap().rect.y0;
                // a drag upward from the middle of the list
                assert_eq!(ui.touch(32, 30, true, 0), Touch::Pending);
                assert_eq!(ui.touch(32, 10, true, 10_000), Touch::Drag);
                assert_eq!(ui.get(first).unwrap().rect.y0, y_before - 20);
                assert_eq!(ui.touch(32, 10, false, 20_000), Touch::DragEnd);
                // scrolling past the end is clamped: the content's far edge never passes the stop
                assert!(ui.scroll_by(root, 0, 1000));
                let vp_y1 = ui.viewport(root).y1;
                let last = ui.children(root).last().unwrap();
                assert_eq!(ui.get(last).unwrap().rect.y1, vp_y1);
                assert!(!ui.scroll_by(root, 0, 1), "nothing left to scroll");
                // the part of a widget above the viewport is untouchable even though its rect
                // covers the point (row 3 spans y = -15..4 here; the viewport starts at 3), and
                // focusing a widget scrolled out brings it in
                assert_eq!(ui.touch(32, 2, true, 100_000), Touch::Pending);
                assert_eq!(ui.touch(32, 2, false, 160_000), Touch::Tap { hit: false, emitted: None });
                assert_eq!(ui.touch(32, 4, true, 200_000), Touch::Pending);
                assert_eq!(ui.touch(32, 4, false, 260_000), Touch::Tap { hit: true, emitted: Some(Ev::Item(3)) });
                ui.set_focus(Some(first));
                assert_eq!(ui.get(first).unwrap().rect.y0, ui.viewport(root).y0);
        }

        #[test]
        fn a_scrolling_row_overflows_sideways_and_a_drag_moves_it_within_the_clamp() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_ROW).unwrap();
                let root = ui.root().unwrap();
                let win = ui.get(root).unwrap().window().unwrap().clone();
                // five columns pinned at 20 px in a 64 px frame: the content overflows
                assert_eq!(win.content_w, 100);
                // a scrolling row's columns keep their measured widths: no absorbed remainder
                let last = ui.children(root).last().unwrap();
                let last_rect = ui.get(last).unwrap().rect;
                assert_eq!(last_rect.x1 - last_rect.x0 + 1, 20);
                let first = ui.children(root).next().unwrap();
                let x_before = ui.get(first).unwrap().rect.x0;
                // a drag leftward from the middle of the row pulls the content with it
                assert_eq!(ui.touch(40, 24, true, 0), Touch::Pending);
                assert_eq!(ui.touch(20, 24, true, 10_000), Touch::Drag);
                assert_eq!(ui.get(first).unwrap().rect.x0, x_before - 20);
                assert_eq!(ui.touch(20, 24, false, 20_000), Touch::DragEnd);
                // scrolling past the end is clamped: the last column rests at the frame's edge
                assert!(ui.scroll_by(root, 1000, 0));
                assert_eq!(ui.get(last).unwrap().rect.x1, ui.viewport(root).x1);
                assert!(!ui.scroll_by(root, 1, 0), "nothing left to scroll");
                // and the row never scrolls vertically, whatever a drag asks for
                assert!(!ui.scroll_by(root, 0, 10));
                // the part of a column left of the viewport is untouchable even though its
                // rect covers the point
                let vp_x0 = ui.viewport(root).x0;
                if vp_x0 > 0 {
                        assert_eq!(ui.touch((vp_x0 - 1) as u16, 24, true, 100_000), Touch::Pending);
                        assert_eq!(ui.touch((vp_x0 - 1) as u16, 24, false, 160_000), Touch::Tap { hit: false, emitted: None });
                }
        }

        #[test]
        fn a_column_pinned_beside_a_scrolling_strip_holds_still_while_the_strip_drags() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_PINNED).unwrap();
                let root = ui.root().unwrap();
                let (pin, strip) = {
                        let mut kids = ui.children(root);
                        (kids.next().unwrap(), kids.next().unwrap())
                };
                // the strip's interior was laid out against its PLACED rect, not the
                // placeholder it was built with: its first column starts inside it
                let strip_rect = ui.get(strip).unwrap().rect;
                let first = ui.children(strip).next().unwrap();
                assert!(ui.get(first).unwrap().rect.x0 > strip_rect.x0);
                assert!(strip_rect.x0 > ui.get(pin).unwrap().rect.x1);
                // the strip overflows and scrolls; the outer row does not
                assert!(ui.get(strip).unwrap().window().unwrap().content_w > strip_rect.x1 - strip_rect.x0 + 1);
                assert!(!ui.scroll_by(root, 10, 0));
                // a drag over the strip pulls its columns and leaves the pinned column alone
                let pin_rect = ui.get(pin).unwrap().rect;
                let x_before = ui.get(first).unwrap().rect.x0;
                let sx = (strip_rect.x0 + 4) as u16;
                assert_eq!(ui.touch(sx + 17, 24, true, 0), Touch::Pending);
                assert_eq!(ui.touch(sx, 24, true, 10_000), Touch::Drag);
                assert_eq!(ui.get(first).unwrap().rect.x0, x_before - 17);
                assert_eq!(ui.get(pin).unwrap().rect, pin_rect);
                assert_eq!(ui.touch(sx, 24, false, 20_000), Touch::DragEnd);
                // the pinned column still answers a tap
                let (px, py) = (pin_rect.x0 as u16, (pin_rect.y0 + 2) as u16);
                assert_eq!(ui.touch(px, py, true, 100_000), Touch::Pending);
                assert_eq!(ui.touch(px, py, false, 160_000), Touch::Tap { hit: true, emitted: Some(Ev::Beta) });
        }

        #[test]
        fn a_grow_child_takes_the_surplus_so_pinned_columns_flank_it() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 12> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                let read = |ui: &Ui<Ev, 12>| {
                        let root = ui.root().unwrap();
                        let mut kids = ui.children(root);
                        let l = ui.get(kids.next().unwrap()).unwrap().rect;
                        let s = ui.get(kids.next().unwrap()).unwrap().rect;
                        let r = ui.get(kids.next().unwrap()).unwrap().rect;
                        (l, s, r)
                };
                let gap = 2;

                ui.navigate(&PAGE_GROW).unwrap();
                let (gl, gs, gr) = read(&ui);
                // both pinned columns keep their 12 px, wherever they sit
                assert_eq!(gl.x1 - gl.x0 + 1, 12);
                assert_eq!(gr.x1 - gr.x0 + 1, 12);
                // left pin, then the strip, then the right pin -- contiguous with the gap between
                assert_eq!(gs.x0, gl.x1 + 1 + gap);
                assert_eq!(gr.x0, gs.x1 + 1 + gap);

                ui.navigate(&PAGE_NOGROW).unwrap();
                let (_nl, ns, nr) = read(&ui);
                // grow gave the strip the surplus that the equal-split default strands after the
                // last column, so the strip is wider and the trailing pin sits flush at the edge
                // instead of floating mid-row (the bug this guards)
                assert!(gs.x1 - gs.x0 + 1 > ns.x1 - ns.x0 + 1, "grow widened the strip");
                assert!(gr.x1 > nr.x1, "grow pushed the trailing pin to the edge");
        }

        #[test]
        fn a_row_page_drops_in_vertically_and_lifts_out_while_a_stack_page_slides() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                let mut now = 0u64;
                let mut settle = |ui: &mut Ui<Ev, 8>, layer: &mut FrameLayer, display: &mut Display<'_, Mock>, now: &mut u64| {
                        let mut frames = 0;
                        while ui.is_animating() {
                                *now += 50_000;
                                ui.render(layer, display, &styled(&font), *now);
                                while layer.poll(display).unwrap() {}
                                frames += 1;
                                assert!(frames < 100, "a transition that never ends");
                        }
                };
                ui.navigate(&PAGE_MAIN).unwrap();
                settle(&mut ui, &mut layer, &mut display, &mut now);
                // forward into a Row page: the incoming enters from logical DOWN (+y), cover-style
                ui.navigate_returning(&PAGE_PINNED, &PAGE_MAIN).unwrap();
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert_eq!((ui.page_move_dx, ui.page_move_dy), (0, 1));
                settle(&mut ui, &mut layer, &mut display, &mut now);
                // back OUT of the Row page: mirrored -- the motion reverses to the other end
                assert!(ui.navigate_back());
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert_eq!((ui.page_move_dx, ui.page_move_dy), (0, -1));
                settle(&mut ui, &mut layer, &mut display, &mut now);
                // forward into a Stack page: the horizontal slide as ever
                ui.navigate(&PAGE_DETAIL).unwrap();
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert_eq!((ui.page_move_dx, ui.page_move_dy), (1, 0));
        }

        //   a minimal LUI blob assembled by hand -- crush's compiler is std/heavy and lives in
        // another crate, so the reader/transition are tested off a literal blob in the v2 layout:
        // two Linear pages, one button each carrying an app-event id
        fn lui_blob() -> StdVec<u8> {
                fn put_str(b: &mut StdVec<u8>, s: &str) {
                        b.push(s.len() as u8);
                        b.extend_from_slice(s.as_bytes());
                }
                let page = |title: &str, btn: &str, event: u16| {
                        let mut b = StdVec::new();
                        put_str(&mut b, title);
                        b.push(crate::lui::code::LAYOUT_LINEAR);
                        b.push(2); // gap
                        b.push(0); // scroll
                        b.push(0); // subtitle
                        b.push(0); // descent
                        b.push(1); // one child
                        b.push(crate::lui::code::KIND_BUTTON);
                        b.push(crate::lui::code::NAV_NONE);
                        b.extend_from_slice(&0u16.to_le_bytes()); // nav_page
                        b.extend_from_slice(&event.to_le_bytes());
                        b.push(0); // tag (0 -> default index+1)
                        b.extend_from_slice(&0u16.to_le_bytes()); // min_w
                        b.extend_from_slice(&0u16.to_le_bytes()); // min_h
                        b.extend_from_slice(&0u16.to_le_bytes()); // max_w
                        b.extend_from_slice(&0u16.to_le_bytes()); // max_h
                        b.push(0); // grow
                        put_str(&mut b, btn);
                        b
                };
                let bodies = [page("One", "Go", 7), page("Two", "Back", 8)];
                let mut blob = StdVec::new();
                blob.extend_from_slice(b"LUI3");
                blob.push(2); // schema version
                blob.push(0); // orientation (portrait)
                blob.extend_from_slice(&2u16.to_le_bytes()); // page_count
                blob.extend_from_slice(&0u16.to_le_bytes()); // root
                blob.extend_from_slice(&64u16.to_le_bytes()); // width
                blob.extend_from_slice(&48u16.to_le_bytes()); // height
                blob.extend_from_slice(&0u16.to_le_bytes()); // corner
                let mut off = (16 + 4 * bodies.len()) as u32;
                for body in &bodies {
                        blob.extend_from_slice(&off.to_le_bytes());
                        off += body.len() as u32;
                }
                for body in &bodies {
                        blob.extend_from_slice(body);
                }
                blob
        }

        /// [`navigate_lui`](Ui::navigate_lui) slides between blob pages with the page transition
        /// (the first page snaps -- nothing to slide from), maps each button's blob event through the
        /// closure, and mirrors forward/back on the same axis -- the parity the dictaphone needs.
        #[test]
        fn navigate_lui_slides_between_blob_pages_and_mirrors_on_back() {
                let fb = font_blob();
                let font = Font::parse(&fb).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let data: &'static [u8] = StdVec::leak(lui_blob());
                let lui = crate::Lui::parse(data).unwrap();
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                let map = |_i: usize, c: &crate::LuiChild<'static>| Some(Ev::Item(c.event as u8));
                let mut now = 0u64;
                let mut settle = |ui: &mut Ui<Ev, 8>, layer: &mut FrameLayer, display: &mut Display<'_, Mock>, now: &mut u64| {
                        let mut frames = 0;
                        while ui.is_animating() {
                                *now += 50_000;
                                ui.render(layer, display, &styled(&font), *now);
                                while layer.poll(display).unwrap() {}
                                frames += 1;
                                assert!(frames < 100, "a transition that never ends");
                        }
                };
                //   first page: nothing to slide from, so it snaps; the button maps its blob event
                ui.navigate_lui(&lui.page(0).unwrap(), false, None, map).unwrap();
                assert!(!ui.is_animating(), "the first page does not slide");
                assert_eq!(ui.widget_text(ui.find(1).unwrap()), Some("Go"));
                //   forward: a Linear tree on the default vertical axis slides on the horizontal
                ui.navigate_lui(&lui.page(1).unwrap(), false, None, map).unwrap();
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert!(ui.is_animating(), "forward navigation slides");
                let fwd = (ui.page_move_dx, ui.page_move_dy);
                assert_eq!(fwd.1, 0, "a vertical-axis Linear tree slides horizontally");
                settle(&mut ui, &mut layer, &mut display, &mut now);
                assert_eq!(ui.widget_text(ui.find(1).unwrap()), Some("Back"));
                //   back: the mirror -- same axis, reversed direction
                ui.navigate_lui(&lui.page(0).unwrap(), true, None, map).unwrap();
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert_eq!((ui.page_move_dx, ui.page_move_dy), (-fwd.0, 0), "back mirrors forward");
                settle(&mut ui, &mut layer, &mut display, &mut now);
                assert_eq!(ui.widget_text(ui.find(1).unwrap()), Some("Go"));
        }

        //   one page: a leaf button then a frame (horizontal-scroll linear strip) of two rows -- the
        // one-level nesting the wide dictaphone needs
        fn nested_lui_blob() -> StdVec<u8> {
                fn put_str(b: &mut StdVec<u8>, s: &str) {
                        b.push(s.len() as u8);
                        b.extend_from_slice(s.as_bytes());
                }
                fn prefix(b: &mut StdVec<u8>, kind: u8, tag: u8, grow: bool) {
                        b.push(kind);
                        b.push(crate::lui::code::NAV_NONE);
                        b.extend_from_slice(&0u16.to_le_bytes()); // nav_page
                        b.extend_from_slice(&0u16.to_le_bytes()); // event
                        b.push(tag);
                        b.extend_from_slice(&0u16.to_le_bytes()); // min_w
                        b.extend_from_slice(&0u16.to_le_bytes()); // min_h
                        b.extend_from_slice(&0u16.to_le_bytes()); // max_w
                        b.extend_from_slice(&0u16.to_le_bytes()); // max_h
                        b.push(grow as u8);
                }
                let mut body = StdVec::new();
                put_str(&mut body, "P");
                body.push(crate::lui::code::LAYOUT_LINEAR);
                body.push(2); // gap
                body.push(0); // scroll
                body.push(0); // subtitle
                body.push(0); // descent
                body.push(2); // two top-level children
                prefix(&mut body, crate::lui::code::KIND_BUTTON, 5, false);
                put_str(&mut body, "top");
                //   the frame: UNtagged (like the real scrolling strip), grows; then
                // layout/gap/scroll/count, then two leaf rows
                prefix(&mut body, crate::lui::code::KIND_FRAME, 0, true);
                body.push(crate::lui::code::LAYOUT_LINEAR);
                body.push(2); // gap
                body.push(crate::scroll::HORIZONTAL);
                body.push(2); // two children
                prefix(&mut body, crate::lui::code::KIND_BUTTON, 0x30, false);
                put_str(&mut body, "R0");
                prefix(&mut body, crate::lui::code::KIND_BUTTON, 0x31, false);
                put_str(&mut body, "R1");

                let mut blob = StdVec::new();
                blob.extend_from_slice(b"LUI3");
                blob.push(2); // schema version
                blob.push(0); // orientation (portrait)
                blob.extend_from_slice(&1u16.to_le_bytes()); // page_count
                blob.extend_from_slice(&0u16.to_le_bytes()); // root
                blob.extend_from_slice(&64u16.to_le_bytes()); // width
                blob.extend_from_slice(&48u16.to_le_bytes()); // height
                blob.extend_from_slice(&0u16.to_le_bytes()); // corner
                blob.extend_from_slice(&((16 + 4) as u32).to_le_bytes()); // page 0 offset
                blob.extend_from_slice(&body);
                blob
        }

        /// build_lui_with realises a one-level frame: the frame's leaf rows are built under it and
        /// findable by tag, alongside the top-level leaf -- what puts the landscape dictaphone's
        /// scrolling strip on the data path.
        #[test]
        fn build_lui_with_builds_a_nested_frame() {
                let fb = font_blob();
                let font = Font::parse(&fb).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, _display) = rig(&mut buf);
                let data: &'static [u8] = StdVec::leak(nested_lui_blob());
                let page = crate::Lui::parse(data).unwrap().page(0).unwrap();
                let mut ui: Ui<Ev, 16> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                let _ = &mut layer;
                ui.build_lui_with(&page, |_i, c: &crate::LuiChild<'static>| Some(Ev::Item(c.event as u8))).unwrap();
                assert_eq!(ui.widget_text(ui.find(5).expect("top button")), Some("top"));
                //   the strip's rows, built under the frame, are found by their own tags
                assert_eq!(ui.widget_text(ui.find(0x30).expect("row 0")), Some("R0"));
                assert_eq!(ui.widget_text(ui.find(0x31).expect("row 1")), Some("R1"));
        }

        /// A per-page [`Descent`] override and the tree-wide default both steer the transition,
        /// in that order of precedence, over the layout-derived seed; back mirrors. The rig is
        /// single-buffered mono, so the engine runs its logical over-mode and `page_move_dx`/`dy`
        /// are the logical unit: Up `(0,1)`, Down `(0,-1)`, Left `(1,0)`, Right `(-1,0)`.
        #[test]
        fn descent_overrides_and_the_tree_default_steer_the_transition() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                let mut now = 0u64;
                let mut settle = |ui: &mut Ui<Ev, 8>, layer: &mut FrameLayer, display: &mut Display<'_, Mock>, now: &mut u64| {
                        let mut frames = 0;
                        while ui.is_animating() {
                                *now += 50_000;
                                ui.render(layer, display, &styled(&font), *now);
                                while layer.poll(display).unwrap() {}
                                frames += 1;
                                assert!(frames < 100, "a transition that never ends");
                        }
                };
                ui.navigate(&PAGE_MAIN).unwrap();
                settle(&mut ui, &mut layer, &mut display, &mut now);

                //   a per-page override beats the layout seed: DETAIL is a stack (seed FromRight),
                // yet each pinned copy flows the way it names, and back reverses it
                for (page, fwd, rev) in [
                        (&PAGE_D_BOTTOM, (0, 1), (0, -1)),
                        (&PAGE_D_TOP, (0, -1), (0, 1)),
                        (&PAGE_D_RIGHT, (1, 0), (-1, 0)),
                        (&PAGE_D_LEFT, (-1, 0), (1, 0)),
                ] {
                        ui.navigate_returning(page, &PAGE_MAIN).unwrap();
                        now += 50_000;
                        ui.render(&mut layer, &mut display, &styled(&font), now);
                        assert_eq!((ui.page_move_dx, ui.page_move_dy), fwd, "forward into an overridden page");
                        settle(&mut ui, &mut layer, &mut display, &mut now);
                        assert!(ui.navigate_back());
                        now += 50_000;
                        ui.render(&mut layer, &mut display, &styled(&font), now);
                        assert_eq!((ui.page_move_dx, ui.page_move_dy), rev, "back out mirrors the arrival");
                        settle(&mut ui, &mut layer, &mut display, &mut now);
                }

                //   the tree default applies to a page with no override (DETAIL, seed FromRight)...
                ui.set_default_descent(Some(Descent::FromTop));
                ui.navigate_returning(&PAGE_DETAIL, &PAGE_MAIN).unwrap();
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert_eq!((ui.page_move_dx, ui.page_move_dy), (0, -1), "tree default overrides the seed");
                settle(&mut ui, &mut layer, &mut display, &mut now);
                assert!(ui.navigate_back());
                settle(&mut ui, &mut layer, &mut display, &mut now);

                //   ...but a page's own override still wins over the tree default
                ui.navigate_returning(&PAGE_D_RIGHT, &PAGE_MAIN).unwrap();
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert_eq!((ui.page_move_dx, ui.page_move_dy), (1, 0), "page override beats the tree default");
                settle(&mut ui, &mut layer, &mut display, &mut now);
                assert!(ui.navigate_back());
                settle(&mut ui, &mut layer, &mut display, &mut now);

                //   clearing the default restores the layout-derived flow: a Row page rises
                ui.set_default_descent(None);
                assert_eq!(ui.default_descent(), None);
                ui.navigate_returning(&PAGE_PINNED, &PAGE_MAIN).unwrap();
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert_eq!((ui.page_move_dx, ui.page_move_dy), (0, 1), "cleared default falls back to the seed");
        }

        /// A theme seeds the tree's default descent, but only as a starting value: an
        /// application that set one explicitly keeps it across any theme change.
        #[test]
        fn a_theme_seeds_the_descent_but_an_explicit_choice_wins() {
                //   a theme with a descent seeds a tree that has not chosen
                let mut ui: Ui<Ev, 8> = Ui::new();
                assert_eq!(ui.default_descent(), None);
                let mut t = Theme::DEFAULT;
                t.descent = Some(Descent::FromBottom);
                ui.set_theme(t);
                assert_eq!(ui.default_descent(), Some(Descent::FromBottom));

                //   an explicit choice wins, and survives a later theme that also names one
                ui.set_default_descent(Some(Descent::FromLeft));
                let mut t2 = Theme::DEFAULT;
                t2.descent = Some(Descent::FromTop);
                t2.bg = 0x1234; // distinct, so set_theme does not early-return
                ui.set_theme(t2);
                assert_eq!(ui.default_descent(), Some(Descent::FromLeft), "the app's choice outlives a restyle");

                //   and an explicit choice made FIRST blocks the seed entirely
                let mut ui2: Ui<Ev, 8> = Ui::new();
                ui2.set_default_descent(None);
                let mut t3 = Theme::DEFAULT;
                t3.descent = Some(Descent::FromBottom);
                t3.bg = 0x0001;
                ui2.set_theme(t3);
                assert_eq!(ui2.default_descent(), None, "an explicit None is a choice, not an absence");
        }

        /// A generic `Linear` window lays out along the tree's `Axis`, and the SAME window
        /// re-flows the other way when the axis flips -- the descriptor never changes. The
        /// navigation seed follows the effective axis too: a Linear page in a horizontal tree
        /// enters from the bottom, as a `Row` page would.
        #[test]
        fn a_linear_window_follows_the_tree_axis_and_reflows_when_it_flips() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);

                // the default axis is vertical
                assert_eq!(ui.layout_axis(), Axis::Vertical);
                ui.navigate(&PAGE_LINEAR).unwrap();
                let root = ui.root().unwrap();
                let r: StdVec<Rect> = ui.children(root).map(|c| ui.get(c).unwrap().rect).collect();
                assert_eq!(r.len(), 2);
                // vertical: the second child sits below the first, sharing the left edge
                assert!(r[1].y0 > r[0].y1 && r[0].x0 == r[1].x0, "linear should stack vertically by default");

                // flip the tree axis: the same live window re-lays side by side
                ui.set_layout_axis(Axis::Horizontal);
                let r: StdVec<Rect> = ui.children(root).map(|c| ui.get(c).unwrap().rect).collect();
                assert!(r[1].x0 > r[0].x1 && r[0].y0 == r[1].y0, "flipping the axis should re-flow horizontally");

                //   and a Linear page now seeds its navigation like a Row (enters from the
                // bottom): the mono rig runs the logical over-mode, so that is (0, 1)
                let mut now = 0u64;
                ui.navigate(&PAGE_LINEAR_2).unwrap();
                now += 50_000;
                ui.render(&mut layer, &mut display, &styled(&font), now);
                assert_eq!((ui.page_move_dx, ui.page_move_dy), (0, 1), "a horizontal-tree Linear page seeds from the bottom");
        }

        /// The `file_list!` component: it generates one tagged, index-emitting row per index
        /// (plus an optional back row), and the fill helpers address a row by tag and show the
        /// placeholder for an empty entry.
        #[test]
        fn file_list_rows_are_tagged_selectable_and_fillable() {
                // the generated slice has one entry per index, plus the back row when given
                assert_eq!(PICK_ROWS.len(), 3);
                assert_eq!(PICK_ROWS_BACK.len(), 3, "two rows and the appended back");

                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PICK_PAGE).unwrap();

                // each row carries tag_base + i and, activated, emits its own index
                for i in 0..3u8 {
                        let id = ui.find(0x30 + i).unwrap_or_else(|| panic!("row {i} present by tag"));
                        assert_eq!(ui.fire(id), Some(Ev::Item(i)), "row {i} emits its index");
                }

                // fill: a name shows as runtime text; an empty entry shows the placeholder label
                ui.fill_list(0x30, &["REC_0001.WAV", "", "keep"], "-");
                let row0 = ui.get(ui.find(0x30).unwrap()).unwrap();
                assert_eq!(row0.text.as_str(), "REC_0001.WAV");
                let row1 = ui.get(ui.find(0x31).unwrap()).unwrap();
                assert!(row1.text.as_str().is_empty(), "an empty entry clears the runtime text");
                if let Kind::Button(b) = &row1.kind {
                        assert_eq!(b.label, "-", "and falls back to the placeholder label");
                } else {
                        panic!("a row is a button");
                }
                // a row past the end is simply left alone (no panic)
                ui.set_list_text(0x30, 9, "ignored", "-");
        }

        /// A `.subtitle()` window reserves a second title row -- its content starts one text
        /// row lower than the same window without one -- and `set_subtitle` fills that row;
        /// on a one-row window it is a no-op.
        #[test]
        fn a_subtitle_reserves_a_second_title_row_and_fills_it() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let cell_h = i32::from(font.cell_height());
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);

                ui.navigate(&PLAIN_PAGE).unwrap();
                let proot = ui.root().unwrap();
                let plain_top = ui.get(ui.children(proot).next().unwrap()).unwrap().rect.y0;

                ui.navigate(&SUB_PAGE).unwrap();
                let sroot = ui.root().unwrap();
                let sub_top = ui.get(ui.children(sroot).next().unwrap()).unwrap().rect.y0;
                assert_eq!(sub_top - plain_top, cell_h, "the subtitle reserves one extra text row of header");

                ui.set_subtitle(sroot, "0:12 / 1:23");
                assert_eq!(ui.get(sroot).unwrap().window().unwrap().subtitle.unwrap().as_str(), "0:12 / 1:23");

                // a one-row window has no subtitle to set
                ui.navigate(&PLAIN_PAGE).unwrap();
                let proot = ui.root().unwrap();
                ui.set_subtitle(proot, "ignored");
                assert!(ui.get(proot).unwrap().window().unwrap().subtitle.is_none());
        }

        /// `set_indicator` carries the shape, lit flag and column for both a `Dot` and a `Play`.
        #[test]
        fn set_indicator_carries_the_shape() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PLAIN_PAGE).unwrap();
                let root = ui.root().unwrap();

                ui.set_indicator(root, Some((IndicatorShape::Play, true, 5)));
                assert_eq!(ui.get(root).unwrap().window().unwrap().indicator, Some((IndicatorShape::Play, true, 5)));
                ui.set_indicator(root, Some((IndicatorShape::Dot, false, 3)));
                assert_eq!(ui.get(root).unwrap().window().unwrap().indicator, Some((IndicatorShape::Dot, false, 3)));
                ui.set_indicator(root, None);
                assert_eq!(ui.get(root).unwrap().window().unwrap().indicator, None);
        }

        /// A tap on a button lights the press flash at once (so a slow handler still reads as
        /// acknowledged), and the flash lapses on its own once ACTIVATE_FLASH_US has passed.
        #[test]
        fn a_tap_flashes_the_button_then_lapses() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PLAIN_PAGE).unwrap();
                let btn = ui.children(ui.root().unwrap()).next().unwrap();
                let r = ui.get(btn).unwrap().rect;
                let (cx, cy) = (((r.x0 + r.x1) / 2) as u16, ((r.y0 + r.y1) / 2) as u16);

                // a tap: down, then up after a valid hold, on the same spot
                ui.touch(cx, cy, true, 0);
                let up = TAP_MIN_HOLD_US + 1;
                assert_eq!(ui.touch(cx, cy, false, up), Touch::Tap { hit: true, emitted: Some(Ev::Alpha) });
                assert!(ui.is_animating(), "the press flash keeps the loop rendering");

                // a render before the deadline keeps it; one after clears it
                ui.render(&mut layer, &mut display, &styled(&font), up + ACTIVATE_FLASH_US / 2);
                assert!(ui.is_animating(), "still flashing mid-window");
                ui.render(&mut layer, &mut display, &styled(&font), up + ACTIVATE_FLASH_US + 1);
                assert!(!ui.is_animating(), "the flash lapses on its own");
        }

        /// The COVER transition (forward into a Row page), at the rotation the landscape apps
        /// run: the incoming page is rendered into the back buffer and slid up over the static
        /// outgoing, which stays in the front. Mid-step, the covered region must be the incoming
        /// image displaced along the viewer's vertical, and the uncovered region the outgoing.
        #[test]
        fn a_row_page_covers_by_sliding_the_incoming_up_over_the_static_outgoing() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut front = std::vec![0u8; 64 * 48 * 2];
                let mut back = std::vec![0u8; 64 * 48 * 2];
                let mut display = Display::new(Mock { pushed: StdVec::new() }, &mut front, 64, 48, PixelFormat::Rgb565, now);
                display.set_back_buffer(&mut back);
                let mut layer = FrameLayer::new(64, 48, PixelFormat::Rgb565);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_MAIN).unwrap();
                let mut t = 0u64;
                let mut settle = |ui: &mut Ui<Ev, 8>, layer: &mut FrameLayer, display: &mut Display<'_, Mock>, t: &mut u64| loop {
                        ui.render(layer, display, &styled(&font), *t);
                        while layer.poll(display).unwrap() {}
                        *t += 50_000;
                        if !ui.is_animating() && !ui.is_dirty() {
                                break;
                        }
                };
                settle(&mut ui, &mut layer, &mut display, &mut t);
                ui.set_rotation(&mut layer, Rotation::R270);
                settle(&mut ui, &mut layer, &mut display, &mut t);
                //   keep a copy of the outgoing (PAGE_MAIN) as it stands in the front before the
                // transition -- the cover must preserve it in the uncovered region. Freeze to
                // reach the front, copy it, thaw; the transition below freezes for itself.
                let outgoing: StdVec<u8> = {
                        assert!(display.freeze());
                        let (f, _) = display.frame_and_capture().unwrap();
                        let v = f.to_vec();
                        display.thaw();
                        v
                };
                ui.navigate(&PAGE_PINNED).unwrap();
                // the first step freezes (render-mode) and renders the incoming into the back
                let t0 = t;
                assert!(ui.render(&mut layer, &mut display, &styled(&font), t0));
                assert!(display.is_frozen());
                while layer.poll(&mut display).unwrap() {}
                let m = layer.transform();
                let (ux, uy) = (m.b.signum(), m.d.signum());
                assert_ne!((ux, uy), (0, 0));
                let span = if ux != 0 { 64 } else { 48 };
                // at the midpoint the blit offset is span/2 (span - travel, travel = span/2)
                assert!(ui.render(&mut layer, &mut display, &styled(&font), t0 + u64::from(ui.page_move_ms) * 500));
                while layer.poll(&mut display).unwrap() {}
                let off = span / 2;
                let (off_x, off_y) = (ux * off, uy * off);
                let (f, incoming) = display.frame_and_capture().expect("frozen mid-transition");
                let mut covered = 0;
                for dy in 0..48i32 {
                        for dx in 0..64i32 {
                                let (sx, sy) = (dx - off_x, dy - off_y);
                                let d = ((dy * 64 + dx) * 2) as usize;
                                if !(0..64).contains(&sx) || !(0..48).contains(&sy) {
                                        //   uncovered: the front still holds the outgoing, untouched
                                        assert_eq!(&f[d..d + 2], &outgoing[d..d + 2], "uncovered ({dx},{dy}) is not the static outgoing");
                                        continue;
                                }
                                //   covered: the incoming (in the back buffer) displaced up
                                let s = ((sy * 64 + sx) * 2) as usize;
                                assert_eq!(&f[d..d + 2], &incoming[s..s + 2], "covered ({dx},{dy}) is not the incoming image displaced by ({off_x},{off_y})");
                                covered += 1;
                        }
                }
                assert_eq!(covered, 64 * 48 / 2);
                // run out: thawed, and the settled tree is the Row page
                settle(&mut ui, &mut layer, &mut display, &mut t);
                assert!(!display.is_frozen());
        }

        //   like Mock, but snapshots the full frame it is handed each push, so a test can compare
        // what two displays would actually show. chunk_count is 1 over the full region here, so
        // frame.buf IS the whole composite
        struct CapMock {
                pushed: StdVec<Region>,
                last_full: StdVec<u8>,
        }
        impl DisplayDriver for CapMock {
                fn init(&mut self, _: &mut dyn Clock, _: u16, _: u16) {}
                fn chunk_count(&self, _: &Region) -> u16 {
                        1
                }
                fn chunks_per_poll(&self, _: &Region) -> u16 {
                        0
                }
                fn kick(&mut self, frame: &Frame<'_>, r: &Region, _: u16) {
                        self.pushed.push(*r);
                        self.last_full = frame.buf.to_vec();
                }
                fn chunk_complete(&mut self) -> bool {
                        true
                }
                fn chunk_timeout_ms(&self) -> u32 {
                        10
                }
        }

        #[test]
        fn region_buffering_slide_matches_the_capture_path() {
                //   the region slide must produce, frame for frame, exactly what the verified
                // capture (double-buffer) slide does for a horizontal reveal -- that is what proves
                // the in-place shift is correct without a second framebuffer. Drive a capture
                // display and a single-buffer region display through the same transition on the same
                // clock, and compare the pushed frames pixel for pixel.
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let (w, h) = (40u16, 24u16);
                let px = (w as usize) * (h as usize) * 2;

                let mut cap_front = std::vec![0u8; px];
                let mut cap_back = std::vec![0u8; px];
                let mut cap = Display::new(CapMock { pushed: StdVec::new(), last_full: StdVec::new() }, &mut cap_front, w, h, PixelFormat::Rgb565, now);
                cap.set_back_buffer(&mut cap_back);
                let mut cap_layer = FrameLayer::new(w, h, PixelFormat::Rgb565);
                let mut cap_ui: Ui<Ev, 8> = Ui::new();

                let mut reg_buf = std::vec![0u8; px];
                let mut reg = Display::new(CapMock { pushed: StdVec::new(), last_full: StdVec::new() }, &mut reg_buf, w, h, PixelFormat::Rgb565, now);
                reg.set_region_buffering(true);
                let mut reg_layer = FrameLayer::new(w, h, PixelFormat::Rgb565);
                let mut reg_ui: Ui<Ev, 8> = Ui::new();
                assert!(reg.region_buffering() && !cap.region_buffering());

                fn settle(ui: &mut Ui<Ev, 8>, layer: &mut FrameLayer, display: &mut Display<'_, CapMock>, font: &Font<'_>, t: &mut u64) {
                        loop {
                                ui.render(layer, display, &styled(font), *t);
                                while layer.poll(display).unwrap() {}
                                *t += 50_000;
                                if !ui.is_animating() && !ui.is_dirty() {
                                        break;
                                }
                        }
                }

                for (ui, layer, display) in [
                        (&mut cap_ui, &mut cap_layer, &mut cap as &mut Display<'_, CapMock>),
                        (&mut reg_ui, &mut reg_layer, &mut reg),
                ] {
                        ui.set_style(&styled(&font));
                        ui.fit(layer);
                        ui.navigate(&PAGE_MAIN).unwrap();
                }
                let mut tc = 0u64;
                let mut tr = 0u64;
                settle(&mut cap_ui, &mut cap_layer, &mut cap, &font, &mut tc);
                settle(&mut reg_ui, &mut reg_layer, &mut reg, &font, &mut tr);
                // same starting image
                assert_eq!(cap.driver().last_full, reg.driver().last_full, "settled main differs before the transition");

                cap_ui.navigate(&PAGE_DETAIL).unwrap();
                reg_ui.navigate(&PAGE_DETAIL).unwrap();
                let t0 = tc.max(tr);
                // step both through the transition on the same clock, comparing each pushed frame
                let ms = u64::from(cap_ui.page_move_ms);
                for k in 0..=6u64 {
                        let t = t0 + ms * 1000 * k / 6;
                        cap_ui.render(&mut cap_layer, &mut cap, &styled(&font), t);
                        while cap_layer.poll(&mut cap).unwrap() {}
                        reg_ui.render(&mut reg_layer, &mut reg, &styled(&font), t);
                        while reg_layer.poll(&mut reg).unwrap() {}
                        assert_eq!(cap.driver().last_full, reg.driver().last_full, "region and capture frames differ at step {k}/6 (t={t})");
                }
                // and the settled child page is identical
                settle(&mut cap_ui, &mut cap_layer, &mut cap, &font, &mut tc);
                settle(&mut reg_ui, &mut reg_layer, &mut reg, &font, &mut tr);
                assert_eq!(cap.driver().last_full, reg.driver().last_full, "settled child differs after the transition");
                assert!(!reg.is_double_buffered(), "region path used a single buffer");
        }

        #[test]
        fn render_pushes_only_the_changed_widgets_after_the_first_full_frame() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_MAIN).unwrap();
                assert!(ui.render(&mut layer, &mut display, &styled(&font), 0));
                assert_eq!(flush(&mut layer, &mut display), [Region::full(64, 48)]);
                assert!(!ui.render(&mut layer, &mut display, &styled(&font), 0), "nothing dirty");
                // moving focus dirties exactly the two buttons
                let root = ui.root().unwrap();
                let rows: StdVec<Rect> = ui.children(root).map(|c| ui.get(c).unwrap().rect).collect();
                ui.focus_next();
                assert!(ui.render(&mut layer, &mut display, &styled(&font), 0));
                let mut pushed = flush(&mut layer, &mut display);
                pushed.sort_by_key(|r| r.y0);
                // the previous frame's full-canvas invalidation carries forward once
                assert_eq!(pushed, [Region::full(64, 48)]);
                ui.focus_next();
                assert!(ui.render(&mut layer, &mut display, &styled(&font), 0));
                let mut pushed = flush(&mut layer, &mut display);
                pushed.sort_by_key(|r| r.y0);
                // rows 0 and 1 from last frame's invalidation, rows 1 and 2 from this one: three
                // disjoint rows (the gap keeps them apart), row 1 merged with itself, and nothing
                // of the header or the frame
                assert_eq!(pushed.len(), 3);
                assert_eq!(pushed[0].y0, rows[0].y0 as u16);
                assert_eq!(pushed[2].y1, rows[2].y1 as u16);
        }

        #[test]
        fn rotation_relayouts_against_the_new_aspect_and_untransforms_taps() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (mut layer, mut display) = rig(&mut buf);
                let mut ui: Ui<Ev, 8> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&PAGE_MAIN).unwrap();
                ui.set_rotation(&mut layer, Rotation::R90);
                // a mono, single-buffered panel cannot animate: the rotation snaps at the next
                // render, which is where it is applied
                assert!(ui.render(&mut layer, &mut display, &styled(&font), 0));
                assert!(!ui.is_animating());
                assert_eq!(ui.logical_size(), (48, 64));
                let root = ui.root().unwrap();
                assert_eq!(ui.get(root).unwrap().rect, Rect::new(0, 0, 47, 63));
                // a panel point maps through the same transform the canvas draws with: logical
                // (10, 20) under R90 on a 64x48 panel is physical (63 - 20, 10)
                let (hit, _) = ui.press_at(43, 10);
                let _ = hit;
                let (lx, ly) = ui.untransform(43, 10);
                assert_eq!((lx, ly), (10, 20));
                // and a swipe along the panel's x reads as vertical to the user
                assert_eq!(ui.swipe_direction((10, 20), (50, 22)), Some(SwipeDir::Up));
        }

        static T_A: Desc<Ev> = Desc::button("Alpha").emit(Ev::Alpha).tag(1);
        static T_B: Desc<Ev> = Desc::button("Beta").emit(Ev::Beta).tag(2);
        static T_C: Desc<Ev> = Desc::button("Gamma").emit(Ev::Alpha).tag(3);
        static T_MORE: Desc<Ev> = Desc::button("More >").navigate(&T_PAGE_DETAIL);
        static T_LIST: Desc<Ev> = Desc::button("List >").navigate(&T_PAGE_LIST);
        static T_MAIN: Desc<Ev> = Desc::window("mk4 demo").rounded(24).stack(2).children(&[&T_A, &T_B, &T_C, &T_MORE, &T_LIST]);
        static T_LBL: Desc<Ev> = Desc::label("swipe right to go back");
        static T_BACK: Desc<Ev> = Desc::button("< Back").back();
        static T_DETAIL: Desc<Ev> = Desc::window("More").rounded(24).stack(2).children(&[&T_LBL, &T_A, &T_B, &T_BACK]);
        static T_I1: Desc<Ev> = Desc::button("Item 1").emit(Ev::Item(1)).min_size(0, 44);
        static T_I2: Desc<Ev> = Desc::button("Item 2").emit(Ev::Item(2)).min_size(0, 44);
        static T_I3: Desc<Ev> = Desc::button("Item 3").emit(Ev::Item(3)).min_size(0, 44);
        static T_I4: Desc<Ev> = Desc::button("Item 4").emit(Ev::Item(4)).min_size(0, 44);
        static T_I5: Desc<Ev> = Desc::button("Item 5").emit(Ev::Item(5)).min_size(0, 44);
        static T_I6: Desc<Ev> = Desc::button("Item 6").emit(Ev::Item(6)).min_size(0, 44);
        static T_I7: Desc<Ev> = Desc::button("Item 7").emit(Ev::Item(7)).min_size(0, 44);
        static T_LBACK: Desc<Ev> = Desc::button("< Back").back().min_size(0, 44);
        static T_LISTW: Desc<Ev> = Desc::window("List").rounded(24).stack(2).scroll(scroll::VERTICAL).children(&[&T_I1, &T_I2, &T_I3, &T_I4, &T_I5, &T_I6, &T_I7, &T_LBACK]);
        static T_PAGE_MAIN: Page<Ev> = Page::new(&T_MAIN, None);
        static T_PAGE_DETAIL: Page<Ev> = Page::new(&T_DETAIL, Some(&T_PAGE_MAIN));
        static T_PAGE_LIST: Page<Ev> = Page::new(&T_LISTW, Some(&T_PAGE_MAIN));

        /// The touch169 demo, end to end on the host: every page built, painted at every
        /// rotation, scrolled and navigated, with the real panel geometry and a 12x19 cell.
        #[test]
        fn the_touch169_demo_builds_paints_and_navigates_at_every_rotation() {
                let mut e = Encoder::new(12, 19, 15, 16);
                for c in 0x20u8..0x7f {
                        e.add(c, &[0xFF; 38]).unwrap();
                }
                let blob = e.encode();
                let font = Font::parse(&blob).unwrap();
                let mut front = std::vec![0u8; 240 * 280 * 2];
                let mut back = std::vec![0u8; 240 * 280 * 2];
                let mut display = Display::new(Mock { pushed: StdVec::new() }, &mut front, 240, 280, PixelFormat::Rgb565, now);
                display.set_back_buffer(&mut back);
                let mut layer = FrameLayer::new(240, 280, PixelFormat::Rgb565);
                let mut ui: Ui<Ev, 12> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&T_PAGE_MAIN).unwrap();
                // time advances 50 ms per pass, so the animations run their course
                let mut t: u64 = 0;
                let settle = |ui: &mut Ui<Ev, 12>, layer: &mut FrameLayer, display: &mut Display<'_, Mock>, t: &mut u64| {
                        let mut frames = 0;
                        loop {
                                if ui.render(layer, display, &styled(&font), *t) {
                                        frames += 1;
                                }
                                let _ = flush(layer, display);
                                *t += 50_000;
                                if !ui.is_animating() && !ui.is_dirty() {
                                        break;
                                }
                                assert!(frames < 100, "an animation that never ends");
                        }
                        frames
                };
                assert!(settle(&mut ui, &mut layer, &mut display, &mut t) >= 1);
                for rot in [Rotation::R90, Rotation::R180, Rotation::R270, Rotation::R0] {
                        ui.set_rotation(&mut layer, rot);
                        // a turn of 280 ms at 50 ms a pass: several animation frames, then the
                        // settled tree at the new orientation
                        assert!(settle(&mut ui, &mut layer, &mut display, &mut t) >= 5);
                        assert_eq!(layer.rotation(), rot);
                        for _ in 0..6 {
                                ui.focus_next();
                                assert_eq!(settle(&mut ui, &mut layer, &mut display, &mut t), 1);
                        }
                }
                assert_eq!(ui.logical_size(), (240, 280));
                // into the list -- a page transition -- then drag it to the end, tap the back row
                let root = ui.root().unwrap();
                let list_btn = ui.children(root).last().unwrap();
                ui.set_focus(Some(list_btn));
                assert_eq!(ui.activate(), None);
                assert!(core::ptr::eq(ui.page().unwrap(), &T_PAGE_LIST));
                assert!(ui.is_animating());
                assert!(settle(&mut ui, &mut layer, &mut display, &mut t) >= 3);
                assert_eq!(ui.touch(120, 200, true, t), Touch::Pending);
                let mut dragged = false;
                for y in (20..200).rev().step_by(10) {
                        // pending until the finger has travelled the slop, a drag from then on
                        match ui.touch(120, y, true, t) {
                                Touch::Pending => assert!(!dragged && 200 - y <= DRAG_SLOP as u16),
                                Touch::Drag => dragged = true,
                                other => panic!("unexpected {other:?}"),
                        }
                        settle(&mut ui, &mut layer, &mut display, &mut t);
                }
                assert!(dragged);
                assert_eq!(ui.touch(120, 20, false, t), Touch::DragEnd);
                let root = ui.root().unwrap();
                let _ = ui.scroll_by(root, 0, 1000);
                let back = ui.children(root).last().unwrap();
                let r = ui.get(back).unwrap().rect;
                let (cx, cy) = (((r.x0 + r.x1) / 2) as u16, ((r.y0 + r.y1) / 2) as u16);
                assert_eq!(ui.touch(cx, cy, true, t), Touch::Pending);
                assert_eq!(ui.touch(cx, cy, false, t + 100_000), Touch::Tap { hit: true, emitted: None });
                assert!(core::ptr::eq(ui.page().unwrap(), &T_PAGE_MAIN));
                // a tap during the return transition still lands: only rotation blocks input
                assert!(ui.is_animating());
                assert!(settle(&mut ui, &mut layer, &mut display, &mut t) >= 3);
                assert!(!display.is_frozen());
        }

        #[test]
        fn a_rotation_animates_from_a_captured_frame_and_ignores_taps_until_it_settles() {
                let mut e = Encoder::new(12, 19, 15, 16);
                for c in 0x20u8..0x7f {
                        e.add(c, &[0xFF; 38]).unwrap();
                }
                let blob = e.encode();
                let font = Font::parse(&blob).unwrap();
                let mut front = std::vec![0u8; 240 * 280 * 2];
                let mut back = std::vec![0u8; 240 * 280 * 2];
                let mut display = Display::new(Mock { pushed: StdVec::new() }, &mut front, 240, 280, PixelFormat::Rgb565, now);
                display.set_back_buffer(&mut back);
                let mut layer = FrameLayer::new(240, 280, PixelFormat::Rgb565);
                let mut ui: Ui<Ev, 12> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                ui.navigate(&T_PAGE_MAIN).unwrap();
                assert!(ui.render(&mut layer, &mut display, &styled(&font), 0));
                let _ = flush(&mut layer, &mut display);
                ui.set_rotation(&mut layer, Rotation::R90);
                // the first step captures the frame and freezes swapping; the layer is still
                // at R0 and the tree still laid out for it
                assert!(ui.render(&mut layer, &mut display, &styled(&font), 1_000), "an animation frame was drawn");
                assert!(display.is_frozen());
                assert_eq!(layer.rotation(), Rotation::R0);
                assert_eq!(flush(&mut layer, &mut display), [Region::full(240, 280)], "an animation frame is a whole-panel push");
                // taps are refused mid-turn
                assert_eq!(ui.press_at(120, 100), (false, None));
                assert_eq!(ui.touch(120, 100, true, 0), Touch::None);
                assert!(ui.render(&mut layer, &mut display, &styled(&font), 150_000));
                let _ = flush(&mut layer, &mut display);
                // past the duration: thawed, committed, the settled tree drawn in one call
                assert!(ui.render(&mut layer, &mut display, &styled(&font), 300_000));
                assert!(!display.is_frozen());
                assert!(!ui.is_animating());
                assert_eq!(layer.rotation(), Rotation::R90);
                assert_eq!(ui.logical_size(), (280, 240));
                assert_eq!(flush(&mut layer, &mut display), [Region::full(240, 280)]);
        }

        #[test]
        fn a_page_too_big_for_the_arena_is_refused_and_leaves_nothing_behind() {
                let blob = font_blob();
                let font = Font::parse(&blob).unwrap();
                let mut buf = [0u8; 64 * 48 / 8];
                let (layer, _d) = rig(&mut buf);
                let mut ui: Ui<Ev, 3> = Ui::new();
                ui.set_style(&styled(&font));
                ui.fit(&layer);
                assert_eq!(ui.navigate(&PAGE_MAIN), Err(Error::Full));
                assert!(ui.root().is_none());
                assert!(ui.widgets.iter().all(|s| s.is_none()));
        }
}
