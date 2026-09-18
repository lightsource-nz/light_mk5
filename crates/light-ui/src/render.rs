use light_display::{Display, DisplayDriver, Region};
use light_display::frames::FrameLayer;
use light_draw::{lerp565, Canvas, Point};
use light_font::Font;
use crate::{Ui, WidgetId, Kind, IndicatorShape, Shade, Style, FontRole, Rect, TEXT_MAX, isqrt, rect_intersect, corner_indent};
use crate::anim::Step;

impl<A: Copy, const N: usize> Ui<A, N> {
        /// The union of everything invalidated since the last repaint, in LOGICAL
        /// coordinates -- what the next `render` will paint, known BEFORE it paints. `None`
        /// when the tree is clean or the WHOLE canvas is pending (`is_dirty` distinguishes
        /// the two). A single-buffered scanned panel schedules its draw against this: the
        /// bounds against the beam.
        pub fn dirty_bounds(&self) -> Option<Rect> {
                if self.pending_all {
                        return None;
                }
                let mut it = self.pending.iter();
                let first = *it.next()?;
                Some(it.fold(first, |a, r| Rect::new(a.x0.min(r.x0), a.y0.min(r.y0), a.x1.max(r.x1), a.y1.max(r.y1))))
        }
        pub fn is_dirty(&self) -> bool {
                self.dirty
        }
        /// Mark a widget's area as needing to reach the panel. Mutators call this; public for
        /// anything that changes what a custom widget draws.
        pub fn invalidate_widget(&mut self, id: WidgetId) {
                let r = self.w(id).rect;
                if !self.pending_all && self.pending.push(r).is_err() {
                        self.pending_all = true;
                }
                self.dirty = true;
        }
        /// Mark the whole canvas. Needed once at startup, before the first render.
        pub fn invalidate_all(&mut self) {
                self.pending_all = true;
                self.dirty = true;
        }
        /// Draw `text` at `(x, y)` truncated to `max_width` and the canvas. The rasteriser clips
        /// per pixel; what this owes it is bounding the STRING so a long label is cut to its widget
        /// rather than painted across the clip, and refusing an origin the canvas cannot hold.
        fn draw_text_fitted(&self, c: &mut Canvas<'_>, font: &Font<'_>, x: i32, y: i32, text: &str, max_width: i32) {
                //   the truncation math uses the cell of the font actually being drawn, so it is
                // correct for any role without the toolkit knowing which one this is
                let cell_w = i32::from(font.cell_width());
                if cell_w == 0 || x < 0 || y < 0 || x >= self.width || y >= self.height {
                        return;
                }
                let fit_widget = (max_width.max(0) / cell_w) as usize;
                let fit_canvas = ((self.width - x) / cell_w) as usize;
                let mut len = text.len().min(fit_widget).min(fit_canvas).min(TEXT_MAX);
                while !text.is_char_boundary(len) {
                        len -= 1;
                }
                if len == 0 {
                        return;
                }
                c.text(font, Point::new(x, y), &text[..len]);
        }
        /// Horizontally centre `len` glyphs of cell width `cell_w` within `[x0, x1]`, never left
        /// of `x0`.
        fn centre_x(&self, x0: i32, x1: i32, len: usize, cell_w: i32) -> i32 {
                let avail = x1 - x0 + 1;
                let used = len as i32 * cell_w;
                if used >= avail { x0 } else { x0 + (avail - used) / 2 }
        }
        /// Clamp a widget's rect only as far as the rasteriser's coordinates demand; the SHAPE is
        /// drawn at its true geometry and the clip cuts it at its container's edge, so a
        /// half-scrolled widget is cropped rather than redrawn smaller.
        fn draw_rect_of(&self, r: Rect) -> Rect {
                Rect::new(r.x0.max(0), r.y0.max(0), r.x1.min(self.width - 1), r.y1.min(self.height - 1))
        }
        fn paint_window(&self, c: &mut Canvas<'_>, style: &Style<'_>, id: WidgetId, clip: &Rect) {
                let w = self.w(id);
                let win = w.window().expect("a window");
                let mut visible = w.rect;
                if !rect_intersect(&mut visible, clip) {
                        return;
                }
                let r = self.draw_rect_of(w.rect);
                //   the window owns every pixel of its rect, but its CHILDREN own theirs:
                // background goes only into the GAPS around them, so on a draw-over frame
                // (no clear -- the single-buffered scanout, where a cleared live buffer
                // flashes black under the beam) every pixel is written once, with its final
                // value. Filling the whole interior first read back as flicker on the glass:
                // the beam catching the focused button between the wash and its repaint.
                // Stacked children are in y order and disjoint; the canvas clip crops the
                // bands of scrolled-away children
                //   children own only the pixels they will PAINT: a scrolling window's
                // children clip to its viewport, so the part of a rect scrolled past it --
                // or the rows past the viewport bottom -- is background here, not child
                // territory. The first fit trusted raw rects, and the unowned strip below
                // the viewport kept every earlier frame: the jumble that taught this
                let mut owned = *clip;
                if w.is_scrolling_window() {
                        let mut vp = self.viewport(id);
                        vp.y1 = vp.y1.max(self.scroll_stop_y1(id));
                        if !rect_intersect(&mut owned, &vp) {
                                owned = Rect::new(0, 0, -1, -1);
                        }
                }
                let saved_fg = c.fg;
                //   the title band is painted once, in its own colour, BEFORE the gap loop, which
                // then starts below it -- so no pixel is written twice (a double-written pixel
                // reads as flicker on the single-buffered scanout). A titled window's band takes
                // the theme's bar colour, falling back to the background when the theme sets none
                let header_inset = if win.border { 1 } else { 0 };
                let header_bottom = if win.title.is_some() {
                        (r.y0 + header_inset + Self::title_rows(win) * self.role_ch(FontRole::Title) + 2).min(r.y1 + 1)
                } else {
                        r.y0
                };
                if header_bottom > r.y0 {
                        c.fg = self.theme.bar.unwrap_or(self.theme.bg);
                        c.rect(Point::new(r.x0, r.y0), Point::new(r.x1, header_bottom - 1), true);
                }
                c.fg = c.bg;
                let mut band_top = header_bottom;
                for child in self.children(id) {
                        let mut cr = self.w(child).rect;
                        if !rect_intersect(&mut cr, &owned) {
                                continue;
                        }
                        if cr.y0 > band_top {
                                c.rect(Point::new(r.x0, band_top), Point::new(r.x1, cr.y0 - 1), true);
                        }
                        let sy0 = cr.y0.max(band_top);
                        if sy0 <= cr.y1 {
                                if cr.x0 > r.x0 {
                                        c.rect(Point::new(r.x0, sy0), Point::new(cr.x0 - 1, cr.y1), true);
                                }
                                if cr.x1 < r.x1 {
                                        c.rect(Point::new(cr.x1 + 1, sy0), Point::new(r.x1, cr.y1), true);
                                }
                        }
                        band_top = band_top.max(cr.y1 + 1);
                }
                if band_top <= r.y1 {
                        c.rect(Point::new(r.x0, band_top), Point::new(r.x1, r.y1), true);
                }
                c.fg = self.theme.frame;
                if win.border {
                        if win.corner_radius != 0 {
                                c.rect_rounded(Point::new(r.x0, r.y0), Point::new(r.x1, r.y1), u16::from(win.corner_radius), light_draw::corner::ALL, false);
                        } else {
                                c.rect(Point::new(r.x0, r.y0), Point::new(r.x1, r.y1), false);
                        }
                }
                let Some(title) = win.title else {
                        c.fg = saved_fg;
                        return;
                };
                let title = if w.text.len > 0 { w.text.as_str() } else { title };
                // the header band the stack layout reserves: title at the very top, separator
                // under it; the two must agree on its height (cell_h + 2). A rounded corner is
                // cleared SIDEWAYS here, not downward: the title is one short string, and the
                // indent is taken at its TOP row, where the arc is furthest in
                let f = &w.rect;
                let inset = if win.border { 1 } else { 0 };
                let ty = f.y0 + inset;
                let indent = corner_indent(win.corner_radius, i32::from(win.corner_radius) - inset);
                let tx = f.x0 + indent + inset + 1;
                c.fg = self.theme.title;
                let text_right = f.x1 - indent - inset;
                self.draw_text_fitted(c, style.fonts.font(FontRole::Title), tx, ty, title, text_right - tx + 1);
                //   an INLINE status symbol, centred on the title cell the caller left blank
                // for it (e.g. the space in "REC 0:12"); sized to sit inside one cell so it
                // never touches the glyphs either side. Only drawn when lit -- the dark
                // phase shows the blank cell, so a flash never reflows the title
                if let Some((shape, true, col)) = win.indicator {
                        let (cw, ch) = (self.role_cw(FontRole::Title), self.role_ch(FontRole::Title));
                        let d = (ch * 3 / 5).clamp(3, cw);
                        let cx = tx + i32::from(col) * cw + cw / 2;
                        let cy = ty + ch / 2;
                        let (bx0, by0) = (cx - d / 2, cy - d / 2);
                        let (bx1, by1) = (bx0 + d - 1, by0 + d - 1);
                        if bx0 >= 0 && by0 >= 0 && bx1 <= text_right {
                                c.fg = self.theme.indicator;
                                match shape {
                                        IndicatorShape::Dot => {
                                                c.rect_rounded(Point::new(bx0, by0), Point::new(bx1, by1), (d / 2) as u16, light_draw::corner::ALL, true);
                                        }
                                        IndicatorShape::Play => {
                                                //   a right-pointing triangle: a vertical base at the
                                                // left, apex at the middle-right, each row a horizontal
                                                // span that tapers to the tip
                                                let mut dy = 0;
                                                while dy < d {
                                                        let wpx = d - 2 * (dy - d / 2).abs();
                                                        if wpx > 0 {
                                                                let y = by0 + dy;
                                                                c.line(Point::new(bx0, y), Point::new(bx0 + wpx - 1, y));
                                                        }
                                                        dy += 1;
                                                }
                                        }
                                }
                        }
                }
                //   the second title row, when the window carries one: a status too long for a
                // narrow bar sits here. Empty draws nothing but the band stays two rows tall
                if let Some(sub) = win.subtitle {
                        if sub.len > 0 {
                                c.fg = self.theme.title;
                                self.draw_text_fitted(c, style.fonts.font(FontRole::Title), tx, ty + self.role_ch(FontRole::Title), sub.as_str(), text_right - tx + 1);
                        }
                }
                // the separator sits below the title row(s), where the arc has come most of the way out
                c.fg = self.theme.frame;
                let sep_y = ty + Self::title_rows(win) * self.role_ch(FontRole::Title);
                let sep_indent = corner_indent(win.corner_radius, i32::from(win.corner_radius) - (sep_y - f.y0));
                let sep_x0 = f.x0 + sep_indent + inset;
                if sep_y <= f.y1 && sep_y >= 0 && sep_x0 >= 0 {
                        c.line(Point::new(sep_x0, sep_y), Point::new(f.x1 - sep_indent - inset, sep_y));
                }
                c.fg = saved_fg;
        }
        fn paint_button(&self, c: &mut Canvas<'_>, style: &Style<'_>, id: WidgetId, clip: &Rect) {
                let w = self.w(id);
                let btn = w.button().expect("a button");
                let mut visible = w.rect;
                if !rect_intersect(&mut visible, clip) {
                        return;
                }
                let r = self.draw_rect_of(w.rect);
                let focused = self.focused == Some(id);
                let pressed = matches!(self.flash, Some((f, _)) if f == id);
                let (p0, p1) = (Point::new(r.x0, r.y0), Point::new(r.x1, r.y1));
                //   a flush row's corners follow the CONTAINER'S OWN ARC -- equal radius,
                // same centre, so its curve parallels the frame's through the corner and
                // spans only the short segment inside the row -- rather than a tangent
                // quarter-circle of reduced radius, whose sweep climbed the row's sides
                // and met the frame at the corner apex (judged on the 1.69's glass)
                let flush = btn.corners == light_draw::corner::BOTTOM && btn.corner_radius != 0;
                let flush_drawn = flush && self.paint_flush_bottom(c, id, focused);
                if !flush_drawn {
                        //   a flush-marked row that is NOT docked at the frame (a scrolled
                        // list row in flight) is an ordinary square row: the frame-following
                        // corners belong to the docked position, and the old fallback --
                        // rect_rounded, whose clamp shrank and re-anchored the arc -- is the
                        // very picture this construction replaced. A button the layout gave
                        // no corner treatment wears the theme's radius on all four instead
                        let themed = btn.corners == light_draw::corner::NONE;
                        let radius = if flush {
                                0
                        } else if themed {
                                u16::from(self.theme.radius)
                        } else {
                                u16::from(btn.corner_radius)
                        };
                        let corners = if themed { light_draw::corner::ALL } else { btn.corners };
                        let saved_fg = c.fg;
                        if pressed {
                                //   the just-tapped acknowledgement: a solid inverted fill,
                                // distinct from both the resting and the focused looks (a
                                // record button is usually already focused, so reusing that
                                // would show nothing). Brief -- see ACTIVATE_FLASH_US
                                c.fg = self.theme.focus_text;
                                if radius != 0 {
                                        c.rect_rounded(p0, p1, radius, corners, true);
                                } else {
                                        c.rect(p0, p1, true);
                                }
                                c.fg = self.theme.button_outline;
                                if radius != 0 {
                                        c.rect_rounded(p0, p1, radius, corners, false);
                                } else {
                                        c.rect(p0, p1, false);
                                }
                        } else if focused {
                                //   the selection: shaded when the theme carries a focus
                                // surface -- the cell reads as LIT -- else a solid fill in
                                // the outline color
                                match self.theme.focus_surface {
                                        Some(s) if radius != 0 => c.rect_rounded_shaded(p0, p1, radius, corners, s.from, s.to),
                                        Some(s) => c.rect_shaded(p0, p1, s.from, s.to),
                                        None => {
                                                c.fg = self.theme.button_outline;
                                                if radius != 0 {
                                                        c.rect_rounded(p0, p1, radius, corners, true);
                                                } else {
                                                        c.rect(p0, p1, true);
                                                }
                                        }
                                }
                        } else {
                                //   an unfocused button owns its rect too: the interior takes
                                // its surface -- its own shade, the theme's button surface,
                                // or the flat background -- so a draw-over frame leaves
                                // nothing of the previous image inside the outline
                                match btn.shade.or(self.theme.button_surface) {
                                        Some(s) if radius != 0 => c.rect_rounded_shaded(p0, p1, radius, corners, s.from, s.to),
                                        Some(s) => c.rect_shaded(p0, p1, s.from, s.to),
                                        None => {
                                                c.fg = c.bg;
                                                if radius != 0 {
                                                        c.rect_rounded(p0, p1, radius, corners, true);
                                                } else {
                                                        c.rect(p0, p1, true);
                                                }
                                        }
                                }
                                c.fg = self.theme.button_outline;
                                if radius != 0 {
                                        c.rect_rounded(p0, p1, radius, corners, false);
                                } else {
                                        c.rect(p0, p1, false);
                                }
                        }
                        c.fg = saved_fg;
                }
                //   the label in the theme's voice: the focus text over the focused fill,
                // the button text otherwise. Uniform for 1 bpp and RGB565, since both go
                // through the same colour path
                let saved_fg = c.fg;
                c.fg = if pressed {
                        self.theme.button_outline
                } else if focused {
                        self.theme.focus_text
                } else {
                        self.theme.button_text
                };
                let label = if w.text.len > 0 { w.text.as_str() } else { btn.label };
                if !label.is_empty() {
                        // positioned from the TRUE rect, never the clamped one: centring against
                        // the clamp made a label creep as its row crossed the canvas edge
                        let f = &w.rect;
                        let (inner_x0, inner_x1) = (f.x0 + 1, f.x1 - 1);
                        let inner_h = f.y1 - f.y0 - 1;
                        //   centre the text that will ACTUALLY be drawn: a label too wide for the
                        // button is truncated by draw_text_fitted, so centring the full length
                        // would shove the visible part off to one side. Count the glyphs that fit,
                        // then centre those.
                        let cw = self.role_cw(FontRole::Body).max(1);
                        let avail = inner_x1 - inner_x0 + 1;
                        let mut fit = label.len().min((avail.max(0) / cw) as usize);
                        while fit > 0 && !label.is_char_boundary(fit) {
                                fit -= 1;
                        }
                        let tx = self.centre_x(inner_x0, inner_x1, fit, cw);
                        let ty = (f.y0 + 1 + (inner_h - self.role_ch(FontRole::Body)) / 2).max(f.y0 + 1);
                        self.draw_text_fitted(c, style.fonts.font(FontRole::Body), tx, ty, label, avail);
                }
                c.fg = saved_fg;
                // TODO a distinct look for disabled buttons wants a colour model 1 bpp lacks
        }
        /// A flush bottom row drawn CONCENTRIC with its container's corner arcs: the same
        /// centres, radius reduced by exactly the row's inset, so the row's curve runs
        /// parallel to the frame's the whole way around the corner at a constant gap.
        /// This exists because `rect_rounded` cannot draw it: its safety clamp caps the
        /// radius at half the row's height and anchors the arc to the ROW's corner, which
        /// shrank the curve and pushed it through the frame at the apex. `false` when the
        /// geometry degenerates (no rounded parent, uneven insets, row too short) and the
        /// caller should draw the ordinary way.
        fn paint_flush_bottom(&self, c: &mut Canvas<'_>, id: WidgetId, focused: bool) -> bool {
                let w = self.w(id);
                let Some(parent) = w.parent else { return false };
                let pw = self.w(parent);
                let Some(pwin) = pw.window() else { return false };
                let pr = self.draw_rect_of(pw.rect);
                //   the clamp mirrors rect_rounded's, so these centres are the ones the
                // frame was actually drawn with
                let rad = i32::from(pwin.corner_radius).min((pr.x1 - pr.x0) / 2).min((pr.y1 - pr.y0) / 2);
                let row = self.draw_rect_of(w.rect);
                //   the parallel gap is the row's SIDE inset from the frame (exact by
                // construction); the bottom must be DOCKED at the flush edge, judged with
                // a pixel of tolerance because the scroll clamp's arithmetic may land the
                // rest position one off -- a strict test left a scrolled list's corners
                // square forever
                let side = row.x0 - pr.x0;
                if side <= 0 || pr.x1 - row.x1 != side || rad <= side {
                        return false;
                }
                let gap = pr.y1 - row.y1;
                if (gap - side).abs() > 1 || gap <= 0 {
                        return false;
                }
                let r_in = rad - side;
                let (cxl, cxr, cy) = (pr.x0 + rad, pr.x1 - rad, pr.y1 - rad);
                //   the inner arc is tangent to the row's bottom and side edges, so the
                // quadrant must fit above the row's top
                if row.y0 > cy || cxl > cxr {
                        return false;
                }

                //   the interior, span by span; doubles as the unfocused surface wash and
                // the focused fill -- below the centres the curve pulls the span ends in,
                // and a shade colors each span on its way down
                let btn_shade = match &w.kind {
                        Kind::Button(b) => b.shade,
                        _ => None,
                };
                let den = row.y1 - row.y0;
                let fill = |c: &mut Canvas<'_>, shade: Option<Shade>| {
                        let saved_fg = c.fg;
                        for y in row.y0..=row.y1 {
                                let (mut x0, mut x1) = (row.x0, row.x1);
                                if y > cy {
                                        let dy = y - cy;
                                        let s = isqrt((r_in * r_in - dy * dy).max(0) as u32) as i32;
                                        x0 = x0.max(cxl - s);
                                        x1 = x1.min(cxr + s);
                                }
                                if x0 <= x1 {
                                        if let Some(s) = shade {
                                                c.fg = lerp565(s.from, s.to, y - row.y0, den);
                                        }
                                        c.rect(Point::new(x0, y), Point::new(x1, y), true);
                                }
                        }
                        c.fg = saved_fg;
                };
                if focused {
                        if self.theme.focus_surface.is_none() {
                                let saved_fg = c.fg;
                                c.fg = self.theme.button_outline;
                                fill(c, None);
                                c.fg = saved_fg;
                        } else {
                                fill(c, self.theme.focus_surface);
                        }
                } else {
                        let wash = btn_shade.or(self.theme.button_surface);
                        if wash.is_none() {
                                let saved_fg = c.fg;
                                c.fg = c.bg;
                                fill(c, None);
                                c.fg = saved_fg;
                        } else {
                                fill(c, wash);
                        }
                        //   straight edges to the tangent points, then the concentric arcs.
                        // Inside a SCROLLING window the arcs are the corner MASK's to draw:
                        // it repaints after the children, and its erase spans (isqrt) and
                        // arc() disagree by the odd pixel -- an arc drawn here came back
                        // with bites taken out of it
                        let saved_fg = c.fg;
                        c.fg = self.theme.button_outline;
                        c.line(Point::new(row.x0, row.y0), Point::new(row.x1, row.y0));
                        c.line(Point::new(row.x0, row.y0), Point::new(row.x0, cy));
                        c.line(Point::new(row.x1, row.y0), Point::new(row.x1, cy));
                        c.line(Point::new(cxl, row.y1), Point::new(cxr, row.y1));
                        if !pw.is_scrolling_window() {
                                c.arc(Point::new(cxl, cy), r_in as u16, 90, 180);
                                c.arc(Point::new(cxr, cy), r_in as u16, 0, 90);
                        }
                        c.fg = saved_fg;
                }
                true
        }
        fn paint_label(&self, c: &mut Canvas<'_>, style: &Style<'_>, id: WidgetId, clip: &Rect) {
                let w = self.w(id);
                let Kind::Label(l) = &w.kind else { return };
                let mut visible = w.rect;
                if !rect_intersect(&mut visible, clip) {
                        return;
                }
                //   the label owns its rect: bg beneath the text, for the draw-over frames
                // where nothing else erases what was here last frame
                let r = self.draw_rect_of(w.rect);
                let saved_fg = c.fg;
                c.fg = c.bg;
                c.rect(Point::new(r.x0, r.y0), Point::new(r.x1, r.y1), true);
                c.fg = saved_fg;
                let text = if w.text.len > 0 { w.text.as_str() } else { l.text };
                let saved_fg = c.fg;
                c.fg = self.theme.text;
                self.draw_text_fitted(c, style.fonts.font(FontRole::Body), w.rect.x0, w.rect.y0, text, w.rect.x1 - w.rect.x0 + 1);
                c.fg = saved_fg;
        }
        /// `clip` is by value so each subtree narrows its own copy: a scrolling window's children
        /// paint only inside its viewport. The same narrowing happens in `hit_test`, and the two
        /// must agree: what cannot be seen must not respond.
        pub(crate) fn paint_clipped(&self, c: &mut Canvas<'_>, style: &Style<'_>, id: WidgetId, mut clip: Rect) {
                if !self.w(id).visible {
                        return;
                }
                // the walk's clip becomes the canvas's, so the primitives cut every shape at the
                // container edge; coordinates are safe -- the walk starts at the canvas and only
                // ever intersects
                c.set_clip(Region::new(clip.x0 as u16, clip.y0 as u16, clip.x1 as u16, clip.y1 as u16));
                match &self.w(id).kind {
                        Kind::Window(_) => self.paint_window(c, style, id, &clip),
                        Kind::Button(_) => self.paint_button(c, style, id, &clip),
                        Kind::Label(_) => self.paint_label(c, style, id, &clip),
                }
                // a scrolling window confines its children to its viewport; its own frame and
                // title were drawn against the wider clip, which keeps the frame visible while
                // content moves beneath it. The content region runs to the scroll STOP for every
                // row, so a row straddling the bottom paints into the corner band
                let scrolling = self.w(id).is_scrolling_window();
                let win_clip = clip;
                if scrolling {
                        let mut vp = self.viewport(id);
                        vp.y1 = vp.y1.max(self.scroll_stop_y1(id));
                        if !rect_intersect(&mut clip, &vp) {
                                return;
                        }
                }
                // children after the parent, in sibling order: later draws on top
                for child in self.children(id) {
                        self.paint_clipped(c, style, child, clip);
                }
                //   the curve belongs to the CONTAINER, not to whichever row is passing: a
                // rounded scrolling window re-masks its bottom corners after its children,
                // so content slides beneath a curve that never moves. Corner treatment that
                // rode the last row vanished the moment a scroll moved it off the stop
                if scrolling {
                        c.set_clip(Region::new(win_clip.x0.max(0) as u16, win_clip.y0.max(0) as u16, win_clip.x1.max(0) as u16, win_clip.y1.max(0) as u16));
                        self.paint_scroll_corner_mask(c, id);
                }
        }
        /// Erase whatever content reached outside the rounded viewport's bottom corners --
        /// the concentric curve the frame's own arcs imply at the content inset -- and
        /// redraw the border arcs over it. Runs AFTER a scrolling window's children.
        fn paint_scroll_corner_mask(&self, c: &mut Canvas<'_>, id: WidgetId) {
                let w = self.w(id);
                let Some(win) = w.window() else { return };
                let pr = self.draw_rect_of(w.rect);
                let rad = i32::from(win.corner_radius).min((pr.x1 - pr.x0) / 2).min((pr.y1 - pr.y0) / 2);
                let inset = Self::inset_x(win);
                let r_in = rad - inset;
                if r_in <= 0 {
                        return;
                }
                let (cxl, cxr, cy) = (pr.x0 + rad, pr.x1 - rad, pr.y1 - rad);
                let saved_fg = c.fg;
                c.fg = c.bg;
                for y in cy..=pr.y1 {
                        let dy = y - cy;
                        let s = if dy < r_in { isqrt((r_in * r_in - dy * dy) as u32) as i32 } else { 0 };
                        //   outside the content curve, inside the frame: erased. The border
                        // itself is repainted below
                        let left_edge = cxl - s;
                        if left_edge > pr.x0 {
                                c.rect(Point::new(pr.x0, y), Point::new(left_edge - 1, y), true);
                        }
                        let right_edge = cxr + s;
                        if right_edge < pr.x1 {
                                c.rect(Point::new(right_edge + 1, y), Point::new(pr.x1, y), true);
                        }
                }
                c.fg = self.theme.frame;
                if win.border {
                        c.arc(Point::new(cxl, cy), rad as u16, 90, 180);
                        c.arc(Point::new(cxr, cy), rad as u16, 0, 90);
                }
                //   the inner boundary -- the concentric arcs and the straight run between
                // them -- drawn HERE, after the erase, because the erase's isqrt spans and
                // arc()'s trig sampling disagree by the odd pixel and an arc drawn earlier
                // came back gap-toothed. Rendered whenever the container holds enough
                // content to scroll: the boundary belongs to the container, marking where
                // content ends against the curve, at every scroll position
                let vp = self.viewport(id);
                let scrollable = win.content_h > self.scroll_stop_y1(id) - vp.y0 + 1;
                if scrollable {
                        c.line(Point::new(cxl, cy + r_in), Point::new(cxr, cy + r_in));
                        c.arc(Point::new(cxl, cy), r_in as u16, 90, 180);
                        c.arc(Point::new(cxr, cy), r_in as u16, 0, 90);
                }
                c.fg = saved_fg;
        }
        /// Paint the whole tree onto a cleared canvas. The ENTIRE tree, not just the dirty
        /// widgets, because every frame is a full repaint; only the pushed REGION is optimised,
        /// which is where the cost that scales with panel size lives.
        pub fn paint(&self, c: &mut Canvas<'_>, style: &Style<'_>) {
                //   the theme's ground: every bg wash in the walk paints with this
                c.bg = self.theme.bg;
                if let Some(root) = self.root {
                        self.paint_clipped(c, style, root, self.canvas_rect());
                }
                // the clip is canvas state: left narrowed it would crop whatever draws next
                c.clear_clip();
        }
        /// Repaint and push, if anything is dirty. Call every pass; the layer's pacing decides
        /// when a frame happens, and this is a no-op on the passes in between. On a refused frame
        /// the dirty flag and the regions survive, so the repaint happens on a later pass.
        pub fn render<D: DisplayDriver>(&mut self, layer: &mut FrameLayer, display: &mut Display<'_, D>, style: &Style<'_>, now_us: u64) -> bool {
                //   a lapsed press flash reverts here: cleared and the button re-drawn normal.
                // Guarded against a widget the flash outlived (it should have been cleared on
                // destroy, but a stale id must never be dereferenced)
                if let Some((id, until)) = self.flash {
                        if now_us >= until {
                                self.flash = None;
                                if self.get(id).is_some() {
                                        self.invalidate_widget(id);
                                }
                        }
                }
                //   the two animations are mutually exclusive by construction, from both ends:
                // show_page declines while a rotation runs, and set_rotation defers while a
                // transition runs. The transition goes first only because it is the one that
                // hands a rotation on when it finishes; a step that finishes falls through to
                // draw the settled tree below rather than waiting a pass
                if self.page_moving {
                        match self.page_step(layer, display, style, now_us) {
                                Step::Drew => return true,
                                Step::Waiting => return false,
                                Step::Finished => {}
                        }
                }
                if self.rotating {
                        match self.rotation_step(layer, display, now_us) {
                                Step::Drew => return true,
                                Step::Waiting => return false,
                                Step::Finished => {}
                        }
                }
                if !self.dirty || self.root.is_none() {
                        return false;
                }
                let Some(mut c) = layer.frame_begin(display, now_us) else { return false };
                self.paint(&mut c, style);
                drop(c);
                self.commit(layer);
                layer.frame_end(display);
                true
        }
        /// Hand the regions invalidated since the last repaint to the layer and mark the tree
        /// clean. `render` does this; a caller running the frame itself (to time its phases, or
        /// to draw over the tree) calls it between `paint` and `frame_end`.
        pub fn commit(&mut self, layer: &mut FrameLayer) {
                if self.pending_all {
                        layer.invalidate_all();
                } else {
                        for r in self.pending.iter() {
                                layer.invalidate(*r);
                        }
                }
                self.pending.clear();
                self.pending_all = false;
                self.dirty = false;
        }
}
