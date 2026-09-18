use crate::{Ui, WidgetId, Error, Nav, Shade, Layout, Descent, Window, Button, Label, Kind, TextSlot, Rect, scroll};
use light_core::error;

/// A selectable list as reusable data: the rows of a file picker or menu, generated once
/// instead of hand-written per screen. Emits a `pub static` slice of row [`Desc`]s ready to
/// drop into a scrolling window's `children`; each row `i` carries tag `tag_base + i` (address
/// it later with [`Ui::set_list_text`]/[`Ui::fill_list`]) and emits the app event the caller's
/// `select` maps its index to -- so the toolkit stays event-agnostic. Sizing is the caller's
/// (`min_size`: a height for a vertical list, a width for a horizontal one), and the list runs
/// along whichever [`Axis`] its window carries, so one definition serves portrait and landscape.
/// Optional `prev:`/`next:` buttons (each a `&'static Desc`) bracket the rows -- `prev` before the
/// first, `next` after the last -- for a paged list: show or hide them per page with
/// [`Ui::set_visible`] (a hidden one collapses out on the next [`Ui::relayout`]). An optional
/// `back:` button is appended last, for a layout that scrolls the way back in with the list rather
/// than pinning it. Supply them in this order: `prev`, `next`, `back`.
///
/// ```ignore
/// light_ui::file_list! {
///         FILE_ROWS,
///         event: AppEvent,
///         tag_base: TAG_ROW_BASE,
///         min_size: (0, 56),
///         select: |i| AppEvent::Pick(i),
///         indices: [0, 1, 2, 3, 4, 5, 6, 7],
///         prev: &BTN_PREV,
///         next: &BTN_NEXT,
///         back: &BTN_BACK,
/// }
/// // static FILE_ROWS: &[&Desc<AppEvent>]  --  Desc::frame().linear(gap).children(FILE_ROWS)
/// ```
#[macro_export]
macro_rules! file_list {
        (
                $name:ident,
                event: $ev:ty,
                tag_base: $base:expr,
                min_size: ($w:expr, $h:expr),
                select: |$i:ident| $emit:expr,
                indices: [$($idx:literal),* $(,)?]
                $(, prev: $prev:expr)?
                $(, next: $next:expr)?
                $(, back: $back:expr)?
                $(,)?
        ) => {
                pub static $name: &[&$crate::Desc<$ev>] = &[
                        $( $prev, )?
                        $(
                                &$crate::Desc::button("-")
                                        .emit({ let $i: u8 = $idx; $emit })
                                        .tag(($base) + $idx)
                                        .min_size($w, $h),
                        )*
                        $( $next, )?
                        $( $back, )?
                ];
        };
}
/// A page: a descriptor tree plus its place in the interface's structure.
///
/// PARENT, NOT HISTORY. `parent` describes where a page sits, the way a directory knows its
/// containing directory, so back goes somewhere predictable however the user arrived and costs
/// no stack -- a history list would have to be bounded, and the bound would be reached by
/// exactly the aimless wandering it exists to serve. Pages are `static` and reference each
/// other across a cycle (a child names its parent, the parent's button names the child).
pub struct Page<A: 'static> {
        pub content: &'static Desc<A>,
        pub parent: Option<&'static Page<A>>,
        /// This page's own navigation-descent direction, overriding the tree default and the
        /// layout-derived fallback. `None` defers to [`Ui::set_default_descent`], then to the
        /// layout seed. See [`Descent`].
        pub descend: Option<Descent>,
}

impl<A> Page<A> {
        pub const fn new(content: &'static Desc<A>, parent: Option<&'static Page<A>>) -> Self {
                Self { content, parent, descend: None }
        }

        /// Pin the direction this page's own arrival and departure flow, overriding the tree
        /// default. The page being entered decides going forward, the page being left coming
        /// back, so a page's opening and closing always mirror.
        pub const fn descend(mut self, descend: Descent) -> Self {
                self.descend = Some(descend);
                self
        }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DescKind {
        Window,
        Button,
        Label,
}
/// A widget tree as data, realised by [`Ui::build`]. The PARENT lists its children in the order
/// they appear -- sibling order is paint order, focus order and the top-to-bottom order of a
/// stack at once, and that is a fact of the source, not of linking. Descriptors are `const`,
/// hold no state, and live in flash: one can build the same subtree into two contexts.
///
/// ```ignore
/// static BTN_OK: Desc<AppEvent> = Desc::button("OK").emit(AppEvent::Ok);
/// static MAIN: Desc<AppEvent> = Desc::window("Title").rounded(8).stack(2).children(&[&BTN_OK]);
/// ```
pub struct Desc<A: 'static> {
        kind: DescKind,
        text: Option<&'static str>,
        emit: Option<A>,
        nav: Nav<A>,
        shade: Option<Shade>,
        corner_radius: u8,
        pub(crate) layout: Layout,
        scroll: u8,
        min_w: i32,
        min_h: i32,
        max_w: i32,
        max_h: i32,
        /// Only meaningful for a hand-placed widget under a `Layout::None` parent; a root's rect
        /// comes from the canvas.
        rect: Option<Rect>,
        tag: u8,
        /// A window only: reserve a second title row (see [`Window::subtitle`]).
        subtitle: bool,
        /// Take the surplus width in a horizontal layout, so pinned siblings can flank it (see
        /// [`Desc::grow`]).
        grow: bool,
        children: &'static [&'static Desc<A>],
}

impl<A: Copy> Desc<A> {
        const fn base(kind: DescKind, text: Option<&'static str>) -> Self {
                Self { kind, text, emit: None, nav: Nav::Stay, corner_radius: 0, layout: Layout::None, scroll: scroll::NONE, min_w: 0, min_h: 0, max_w: 0, max_h: 0, rect: None, tag: 0, subtitle: false, grow: false, children: &[], shade: None }
        }
        pub const fn window(title: &'static str) -> Self {
                Self::base(DescKind::Window, Some(title))
        }
        /// An untitled frame.
        pub const fn frame() -> Self {
                Self::base(DescKind::Window, None)
        }
        /// Give a titled window a second title row (see [`Window::subtitle`]), filled at
        /// runtime with [`Ui::set_subtitle`] -- for a narrow bar that splits a long status
        /// across two rows. The header band is two rows whether or not the lower one has text,
        /// so the content area never jumps.
        pub const fn subtitle(mut self) -> Self {
                self.subtitle = true;
                self
        }
        pub const fn button(label: &'static str) -> Self {
                Self::base(DescKind::Button, Some(label))
        }
        pub const fn label(text: &'static str) -> Self {
                Self::base(DescKind::Label, Some(text))
        }
        pub const fn emit(mut self, event: A) -> Self {
                self.emit = Some(event);
                self
        }
        pub const fn navigate(mut self, page: &'static Page<A>) -> Self {
                self.nav = Nav::To(page);
                self
        }
        pub const fn back(mut self) -> Self {
                self.nav = Nav::Back;
                self
        }
        /// Name a window's own corner radius, overriding the theme's -- which otherwise
        /// supplies [`Theme::screen_radius`] for the outer window and [`Theme::radius`]
        /// inside. 0 (the unset value) means themed, so a square frame under a rounded
        /// theme is expressed in the theme, not here.
        pub const fn rounded(mut self, radius: u8) -> Self {
                self.corner_radius = radius;
                self
        }
        /// A vertical gradient surface for a button, `from` at the top: shown while
        /// unfocused, under the outline, in place of the flat background.
        pub const fn shaded(mut self, from: u16, to: u16) -> Self {
                self.shade = Some(Shade { from, to });
                self
        }
        pub const fn stack(mut self, gap: u8) -> Self {
                self.layout = Layout::Stack { gap };
                self
        }
        pub const fn row(mut self, gap: u8) -> Self {
                self.layout = Layout::Row { gap };
                self
        }
        /// Lay children along the tree's [`Axis`] ([`Ui::set_layout_axis`]) rather than a
        /// fixed one -- a `Stack` in a vertical tree, a `Row` in a horizontal one. Authors a
        /// window once and lets the board choose portrait or landscape. See [`Layout::Linear`].
        pub const fn linear(mut self, gap: u8) -> Self {
                self.layout = Layout::Linear { gap };
                self
        }
        pub const fn scroll(mut self, flags: u8) -> Self {
                self.scroll = flags;
                self
        }
        pub const fn min_size(mut self, w: i32, h: i32) -> Self {
                self.min_w = w;
                self.min_h = h;
                self
        }
        pub const fn max_size(mut self, w: i32, h: i32) -> Self {
                self.max_w = w;
                self.max_h = h;
                self
        }
        pub const fn rect(mut self, x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
                self.rect = Some(Rect::new(x0, y0, x1, y1));
                self
        }
        pub const fn tag(mut self, tag: u8) -> Self {
                self.tag = tag;
                self
        }
        /// Claim the leftover space in a horizontal layout ([`Layout::Row`], or [`Layout::Linear`]
        /// in a horizontal tree). Without any grow child, the layout splits the width evenly and
        /// the last column absorbs the remainder -- so a fixed-width button can only sit at the
        /// end. Mark the child that should stretch, and the others keep their pinned widths
        /// wherever they are, letting pinned buttons flank a stretching one on both sides (the
        /// wide recordings list's paging buttons bracket the scrolling strip this way). Leftover
        /// splits evenly between multiple grow children. Honored by the horizontal layout only.
        pub const fn grow(mut self) -> Self {
                self.grow = true;
                self
        }
        pub const fn children(mut self, children: &'static [&'static Desc<A>]) -> Self {
                self.children = children;
                self
        }
}

impl<A: Copy, const N: usize> Ui<A, N> {
        /// `parent` may be `None` for the root. `rect` is absolute.
        ///
        /// The corner radius comes from the theme: the OUTER window wears the glass's own
        /// curvature ([`Theme::screen_radius`], zero on square screens) so the frame
        /// parallels the screen edge, and every interior container the house radius.
        pub fn create_window(&mut self, parent: Option<WidgetId>, rect: Rect, title: Option<&'static str>, subtitle: bool) -> Result<WidgetId, Error> {
                let corner_radius = if parent.is_none() { self.theme.screen_radius } else { self.theme.radius };
                let subtitle = if subtitle { Some(TextSlot::EMPTY) } else { None };
                let win = Window { title, padding: 2, border: true, corner_radius, layout: Layout::None, scroll: scroll::NONE, scroll_x: 0, scroll_y: 0, content_w: 0, content_h: 0, indicator: None, subtitle };
                self.add(parent, Kind::Window(win), rect, false)
        }
        pub fn create_button(&mut self, parent: Option<WidgetId>, rect: Rect, label: &'static str, emit: Option<A>, nav: Nav<A>) -> Result<WidgetId, Error> {
                let id = self.add(parent, Kind::Button(Button { label, emit, nav, corner_radius: 0, corners: light_draw::corner::NONE, shade: None }), rect, true)?;
                // the first focusable widget takes focus, so a two-button rig always has
                // somewhere to start cycling from
                if self.focused.is_none() {
                        self.focused = Some(id);
                }
                Ok(id)
        }
        pub fn create_label(&mut self, parent: Option<WidgetId>, rect: Rect, text: &'static str) -> Result<WidgetId, Error> {
                self.add(parent, Kind::Label(Label { text }), rect, false)
        }
        fn build_desc(&mut self, parent: Option<WidgetId>, desc: &'static Desc<A>) -> Result<WidgetId, Error> {
                let rect = desc.rect.unwrap_or(Rect::new(0, 0, 0, 0));
                let id = match desc.kind {
                        DescKind::Window => {
                                let id = self.create_window(parent, rect, desc.text, desc.subtitle)?;
                                //   before the children exist: the corner clearance is then already
                                // accounted for when the single layout pass runs; and scrolling
                                // changes how the stack treats rows that do not fit. An unset desc
                                // radius keeps the themed one create_window resolved
                                let win = self.w_mut(id).window_mut().expect("a window");
                                if desc.corner_radius != 0 {
                                        win.corner_radius = desc.corner_radius;
                                }
                                win.scroll = desc.scroll;
                                id
                        }
                        DescKind::Button => {
                                let id = self.create_button(parent, rect, desc.text.unwrap_or(""), desc.emit, desc.nav)?;
                                if let Kind::Button(b) = &mut self.w_mut(id).kind {
                                        b.shade = desc.shade;
                                }
                                id
                        }
                        DescKind::Label => self.create_label(parent, rect, desc.text.unwrap_or(""))?,
                };
                {
                        let w = self.w_mut(id);
                        w.min_w = desc.min_w;
                        w.min_h = desc.min_h;
                        w.max_w = desc.max_w;
                        w.max_h = desc.max_h;
                        w.grow = desc.grow;
                        w.tag = desc.tag;
                }
                for child in desc.children {
                        self.build_desc(Some(id), child)?;
                }
                // after the children, since a stack divides the content area between them
                match desc.layout {
                        Layout::Stack { gap } => self.layout_stack(id, gap),
                        Layout::Row { gap } => self.layout_row(id, gap),
                        Layout::Linear { gap } => self.layout_linear(id, gap),
                        Layout::None => {}
                }
                Ok(id)
        }
        /// Realise `desc` and its children under `parent` (`None` for the root). Building a ROOT
        /// also re-lays-out, sizing the tree to the canvas: a descriptor cannot carry the root's
        /// rect, which is what lets one serve a 64x128 OLED and a 240x280 panel unchanged. On
        /// `Error::Full` the partial subtree is torn down again.
        pub fn build(&mut self, parent: Option<WidgetId>, desc: &'static Desc<A>) -> Result<WidgetId, Error> {
                let before = self.widgets.len();
                match self.build_desc(parent, desc) {
                        Ok(id) => {
                                if parent.is_none() {
                                        self.relayout();
                                }
                                Ok(id)
                        }
                        Err(e) => {
                                // whatever got built is unreachable garbage otherwise
                                for i in before..self.widgets.len() {
                                        self.widgets[i] = None;
                                }
                                if parent.is_none() {
                                        self.root = None;
                                        self.focused = None;
                                }
                                error!("ui: building a page needs more than {} widgets", N);
                                Err(e)
                        }
                }
        }
}
