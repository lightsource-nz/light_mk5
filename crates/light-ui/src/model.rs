use crate::{Rect, Page};

/// Longest label the toolkit renders. Labels are truncated to their widget anyway; this bounds
/// the work a single draw does.
pub const TEXT_MAX: usize = 64;
/// A handle to a widget in its `Ui`'s arena. Stale after the widget is destroyed: the arena
/// answers `None` for it, and a handle from a torn-down page cannot reach another page's widget
/// except by index reuse, which is why handlers conventionally navigate last and touch nothing
/// afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WidgetId(pub(crate) u8);
/// Whether a window's content may exceed its frame and be moved through it, per axis. OR-able.
pub mod scroll {
        pub const NONE: u8 = 0;
        pub const VERTICAL: u8 = 1 << 0;
        pub const HORIZONTAL: u8 = 1 << 1;
}
/// How a window arranges its children when layout is (re-)run. Recorded on the window so a
/// rotation or resize can re-apply it without the application being told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
        /// Children placed by hand; relayout leaves them where they are.
        None,
        /// Equal-height rows, one per visible child, `gap` pixels apart. Pins the vertical
        /// axis regardless of the tree's [`Axis`].
        Stack { gap: u8 },
        /// Equal-width columns, one per visible child, `gap` pixels apart: the horizontal
        /// counterpart of `Stack`. Pins the horizontal axis regardless of the tree's [`Axis`].
        Row { gap: u8 },
        /// A line of children along whichever axis the tree carries
        /// ([`Ui::set_layout_axis`]): `Stack` when the tree is [`Axis::Vertical`], `Row` when
        /// [`Axis::Horizontal`]. The window keeps this generic recording, so flipping the
        /// tree axis re-lays it the other way without the descriptor changing. This is how a
        /// tree is authored once and instantiated portrait or landscape from the outside.
        Linear { gap: u8 },
        /// Equal cells in `cols` columns, filled row-major, `gap` pixels apart on both axes:
        /// a keypad, a palette, a page of icons. Pins BOTH axes regardless of the tree's
        /// [`Axis`]; a grid is the one layout whose shape does not follow the orientation.
        Grid { cols: u8, gap: u8 },
}
/// The axis a generic [`Layout::Linear`] runs along: a whole tree's primary layout
/// direction, set with [`Ui::set_layout_axis`]. `Vertical` -- the default -- stacks children
/// top to bottom; `Horizontal` lays them side by side. `Stack` and `Row` ignore it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
        Vertical,
        Horizontal,
}
/// The edge a child page enters from as it opens: press a button and the incoming page
/// slides in from this edge, the outgoing one retreating the same way; back navigation runs
/// the reverse. Named by the origin edge, which is how it reads at the call site
/// (`FromBottom` rises up into view). A whole tree can be pointed one way with
/// [`Ui::set_default_descent`], and a single page can override it with [`Page::descend`].
/// Left unset everywhere, a page keeps the historical layout-derived flow (a `Row` page
/// enters from the bottom, everything else from the right) -- see [`Ui`]'s navigation section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Descent {
        FromTop,
        FromBottom,
        FromLeft,
        FromRight,
}

impl Descent {
        /// Whether this descent runs the vertical axis (`FromTop`/`FromBottom`) rather than
        /// the horizontal one.
        pub const fn vertical(self) -> bool {
                matches!(self, Descent::FromTop | Descent::FromBottom)
        }

        /// The signed unit of this descent's axis in the transition engine's logical
        /// convention (see `Ui::page_step`): `FromBottom`/`FromLeft` are `+1`,
        /// `FromTop`/`FromRight` are `-1`. The vertical and horizontal axes carry different
        /// senses because the engine's cover-forward path runs logical `+y` while its reveal
        /// path runs logical `-x`; the two forward defaults, `Row`->`FromBottom` and
        /// `Stack`->`FromRight`, fall out as `+1` and `-1` respectively, which is what keeps
        /// the historical flow byte-for-byte.
        pub(crate) const fn axis_sign(self) -> i32 {
                match self {
                        Descent::FromBottom | Descent::FromLeft => 1,
                        Descent::FromTop | Descent::FromRight => -1,
                }
        }
}
/// Where a button takes the interface when activated, after emitting its event.
#[derive(Clone, Copy)]
pub enum Nav<A: 'static> {
        Stay,
        To(&'static Page<A>),
        Back,
}

impl<A> core::fmt::Debug for Nav<A> {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                match self {
                        Nav::Stay => f.write_str("Stay"),
                        Nav::To(p) => write!(f, "To({:p})", *p),
                        Nav::Back => f.write_str("Back"),
                }
        }
}
#[derive(Clone, Copy, Debug)]
pub struct Window {
        pub title: Option<&'static str>,
        /// Gap between the frame and the content area the stack layout divides.
        pub padding: u8,
        pub border: bool,
        /// 0 for a square frame; otherwise the frame is rounded and content keeps clear of the
        /// curve -- see [`Ui::set_corner_radius`]. Resolved from the theme at creation --
        /// [`Theme::screen_radius`] for the outer window, [`Theme::radius`] inside -- unless
        /// the descriptor names its own.
        pub corner_radius: u8,
        pub layout: Layout,
        /// `scroll::*` flags. While any is set, painting and hit-testing clip the children to the
        /// viewport, which is what lets content live partly outside the frame.
        pub scroll: u8,
        /// How far the content is moved, per axis, >= 0: `scroll_y == 20` means the content has
        /// moved UP by 20 and its first 20 rows are above the viewport. Rects stay absolute --
        /// scrolling shifts every rect under the window -- so these exist to be clamped and
        /// reasoned about, not as a second coordinate space.
        pub scroll_x: i32,
        pub scroll_y: i32,
        /// Extent of the laid-out content from the viewport origin at scroll 0. Maintained by the
        /// stack layout; measured from the children on demand for a hand-placed window.
        pub content_w: i32,
        pub content_h: i32,
        /// A status symbol drawn INLINE in the title bar -- a recording light, a play head --
        /// `Some((shape, lit, col))` centres it on title character cell `col`, so the caller
        /// leaves a blank there for it (e.g. the space in `REC 0:12`). `lit` false is the dark
        /// phase of a flash; `None` is off. Because it sits on a space the title already holds,
        /// blinking never reflows the text. See [`Ui::set_indicator`] and [`IndicatorShape`].
        pub indicator: Option<(IndicatorShape, bool, u16)>,
        /// An optional SECOND title row: `None` is a one-row header, `Some` makes the header
        /// band two rows and carries the runtime text of the lower one (empty draws blank).
        /// A narrow bar splits a status too long for one row onto it. See [`Ui::set_subtitle`].
        pub subtitle: Option<TextSlot>,
}
/// The shape of a title-bar [`indicator`](Window::indicator): a filled `Dot` (a recording
/// light) or a right-pointing `Play` triangle (a play head). Both paint in
/// [`Theme::indicator`](crate::theme::Theme::indicator).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndicatorShape {
        Dot,
        Play,
}
#[derive(Clone, Copy, Debug)]
pub struct Button<A: 'static> {
        pub label: &'static str,
        /// Published by the caller when the button is activated.
        pub emit: Option<A>,
        pub nav: Nav<A>,
        /// Set by the stack layout on a row that sits flush against a rounded window, so the row
        /// follows the container's curve; `corners` names only the ones that touch it.
        pub corner_radius: u8,
        pub corners: u8,
        /// The button's SURFACE when unfocused: a gradient wash under the outline in place
        /// of the flat background. The focused fill is the UI-level style instead.
        pub shade: Option<Shade>,
}
#[derive(Clone, Copy, Debug)]
pub struct Label {
        pub text: &'static str,
}
#[derive(Clone, Copy, Debug)]
pub enum Kind<A: 'static> {
        Window(Window),
        Button(Button<A>),
        Label(Label),
}
/// A vertical shade across a surface: `from` at the top edge, `to` at the bottom. An
/// RGB565 idea -- a mono canvas renders any shade as a solid fill -- carried by the
/// focused widget's fill ([`Ui::set_focus_shade`]) and by any button built
/// [`Desc::shaded`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shade {
        pub from: u16,
        pub to: u16,
}
/// Owned widget text: a small copy the application rewrites at runtime
/// ([`Ui::set_text`]) where the descriptor's `&'static str` cannot carry a formatted
/// value -- an elapsed time, a file name. Empty means unset: the static label shows.
#[derive(Clone, Copy, Debug)]
pub struct TextSlot {
        buf: [u8; Self::CAP],
        pub(crate) len: u8,
}

impl TextSlot {
        /// Sized for a row on a small panel; longer text truncates at a char boundary.
        pub const CAP: usize = 31;
        pub(crate) const EMPTY: Self = Self { buf: [0; Self::CAP], len: 0 };

        pub(crate) fn set(&mut self, s: &str) {
                let mut take = s.len().min(Self::CAP);
                while !s.is_char_boundary(take) {
                        take -= 1;
                }
                self.buf[..take].copy_from_slice(&s.as_bytes()[..take]);
                self.len = take as u8;
        }

        pub(crate) fn as_str(&self) -> &str {
                core::str::from_utf8(&self.buf[..usize::from(self.len)]).unwrap_or("")
        }
}
#[derive(Clone, Copy, Debug)]
pub struct Widget<A: 'static> {
        pub kind: Kind<A>,
        /// Runtime text override -- see [`Ui::set_text`].
        pub(crate) text: TextSlot,
        pub rect: Rect,
        pub visible: bool,
        pub focusable: bool,
        pub enabled: bool,
        /// Extra rows below `rect.y1` that count as a hit but are never drawn: the strip beneath
        /// a row laid out flush to a rounded container -- padding, border, safe inset -- has no
        /// widget of its own and reads as part of the row. A hit extension rather than a taller
        /// rect: what is wrong there is the target, not the picture.
        pub hit_slop_y1: i32,
        /// Bounds on what auto-layout may make of this widget, 0 for unconstrained. A minimum is
        /// what makes a stack OVERFLOW rather than shrink its rows without limit; min wins over
        /// max, on the grounds that a widget too small to use is the worse failure.
        pub min_w: i32,
        pub min_h: i32,
        pub max_w: i32,
        pub max_h: i32,
        /// Stretches to take the surplus in a horizontal layout, so pinned siblings can flank it.
        pub grow: bool,
        /// An application-chosen mark, for finding a widget again after a build; 0 = untagged.
        pub tag: u8,
        pub(crate) parent: Option<WidgetId>,
        pub(crate) next_sibling: Option<WidgetId>,
        pub(crate) first_child: Option<WidgetId>,
}

impl<A: Copy> Widget<A> {
        pub fn window(&self) -> Option<&Window> {
                match &self.kind {
                        Kind::Window(w) => Some(w),
                        _ => None,
                }
        }
        pub fn button(&self) -> Option<&Button<A>> {
                match &self.kind {
                        Kind::Button(b) => Some(b),
                        _ => None,
                }
        }
        pub(crate) fn window_mut(&mut self) -> Option<&mut Window> {
                match &mut self.kind {
                        Kind::Window(w) => Some(w),
                        _ => None,
                }
        }
        pub(crate) fn is_scrolling_window(&self) -> bool {
                matches!(&self.kind, Kind::Window(w) if w.scroll != 0)
        }
}
