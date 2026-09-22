use heapless::Vec;
use light_core::warn;
use crate::{Ui, WidgetId, Widget, Window, Kind, Layout, Axis, FontRole, Rect, scroll, corner_drop, rect_empty};

impl<A: Copy, const N: usize> Ui<A, N> {
        /// Set the axis every [`Layout::Linear`] window in this tree runs along -- the whole
        /// interface's portrait/landscape choice, made from the outside. Re-lays the current
        /// page so a change takes effect at once; `Stack` and `Row` windows are unaffected.
        pub fn set_layout_axis(&mut self, axis: Axis) {
                if self.layout_axis == axis {
                        return;
                }
                self.layout_axis = axis;
                if let Some(root) = self.root {
                        self.relayout_window(root);
                        self.invalidate_all();
                }
        }
        /// The tree's generic-layout axis, [`Axis::Vertical`] unless
        /// [`set_layout_axis`](Self::set_layout_axis) changed it.
        pub fn layout_axis(&self) -> Axis {
                self.layout_axis
        }
        pub(crate) fn inset_x(win: &Window) -> i32 {
                i32::from(win.padding) + if win.border { 1 } else { 0 }
        }
        /// Text rows in a window's title band: two when it carries a [`subtitle`](Window::subtitle),
        /// one otherwise. The single source `viewport` and `paint_window` share, so the reserved
        /// band and the painted band always agree.
        pub(crate) fn title_rows(win: &Window) -> i32 {
                if win.subtitle.is_some() { 2 } else { 1 }
        }
        /// A window's VIEWPORT: the area content shows through, inside border, padding, header
        /// band and corner clearance. One function so painting, hit-testing, the stack layout and
        /// the scroll clamp can never disagree about where content is allowed to be.
        ///
        /// A rounded corner is cleared vertically only as far as the arc reaches in at `inset_x`,
        /// not by the whole radius: at radius 40 with a 3 px inset that is 25 rows rather than 40.
        /// A titled window loses the header band, which is a lower bound on the content top
        /// rather than an addition -- adding would push content down twice for the same corner.
        pub(crate) fn viewport(&self, id: WidgetId) -> Rect {
                let w = self.w(id);
                let win = w.window().expect("a window");
                let inset_x = Self::inset_x(win);
                let drop = corner_drop(win.corner_radius, inset_x);
                let inset_y = drop.max(inset_x);
                let mut content = Rect::new(w.rect.x0 + inset_x, w.rect.y0 + inset_y, w.rect.x1 - inset_x, w.rect.y1 - inset_y);
                if win.title.is_some() {
                        //   a small breathing gap below the title bar before the content, so the top
                        // control does not butt against the separator. A rounded window already has
                        // it -- its corner_drop clears the curve by more than this -- so the max()
                        // leaves those unchanged and only gives a square window the same spacing.
                        const HEADER_GAP: i32 = 4;
                        let header_bottom = w.rect.y0 + if win.border { 1 } else { 0 } + Self::title_rows(win) * self.role_ch(FontRole::Title) + 2;
                        content.y0 = content.y0.max(header_bottom + HEADER_GAP);
                }
                content
        }
        /// Where a vertically scrolling stack's travel ends: the last row comes to rest at the
        /// flush edge (`rect.y1 - inset_x`), not the viewport's bottom. The plain viewport bottom
        /// for anything that earns no corner treatment.
        pub(crate) fn scroll_stop_y1(&self, id: WidgetId) -> i32 {
                let vp = self.viewport(id);
                let w = self.w(id);
                let win = w.window().expect("a window");
                if !matches!(win.layout, Layout::Stack { .. }) || win.scroll & scroll::VERTICAL == 0 {
                        return vp.y1;
                }
                let inset_x = Self::inset_x(win);
                if i32::from(win.corner_radius) - inset_x <= 0 {
                        return vp.y1;
                }
                (w.rect.y1 - inset_x).max(vp.y1)
        }
        pub(crate) fn last_visible_child(&self, id: WidgetId) -> Option<WidgetId> {
                self.children(id).filter(|c| self.w(*c).visible).last()
        }
        fn row_height(w: &Widget<A>, mut h: i32) -> i32 {
                if w.max_h != 0 && h > w.max_h {
                        h = w.max_h;
                }
                if w.min_h != 0 && h < w.min_h {
                        h = w.min_h;
                }
                h
        }
        fn row_width(w: &Widget<A>, mut width: i32) -> i32 {
                if w.max_w != 0 && width > w.max_w {
                        width = w.max_w;
                }
                if w.min_w != 0 && width < w.min_w {
                        width = w.min_w;
                }
                width
        }
        /// Divide the window's content area into equal-height rows, one per visible child, `gap`
        /// pixels apart. What makes a 64x128 OLED usable without hand-computing rects, and the
        /// companion to focus cycling: a stack has an obvious visual order.
        ///
        /// The last row of a NON-scrolling stack under a rounded frame runs all the way down and
        /// takes the container's curve as its own bottom corners: insetting a rounded rect by
        /// `inset_x` leaves a rounded rect of radius `R - inset_x` about the same arc centres, so
        /// a row ending at `y1 - inset_x` with that corner radius traces the container exactly --
        /// provided it is at least that tall, which is checked and withdrawn otherwise. A
        /// scrolling stack's rows move, so corners minted for one position are wrong the moment
        /// they do: its last row wears rounded bottom corners permanently instead, as the list's
        /// end cap, sitting flush when the clamp lands it at the stop.
        pub fn layout_stack(&mut self, id: WidgetId, gap: u8) {
                if let Some(win) = self.w_mut(id).window_mut() {
                        win.layout = Layout::Stack { gap };
                }
                self.lay_vertical(id, gap);
        }
        /// The vertical placement pass of [`layout_stack`], without recording the layout kind:
        /// shared with a [`Layout::Linear`] window in a vertical tree, which stays `Linear` so
        /// a later axis flip re-lays it the other way.
        fn lay_vertical(&mut self, id: WidgetId, gap: u8) {
                let gap = i32::from(gap);
                let mut content = self.viewport(id);
                let plain_y1 = content.y1;
                let (rect, inset_x, corner_radius, scroll_flags) = {
                        let w = self.w(id);
                        let win = w.window().expect("a window");
                        (w.rect, Self::inset_x(win), win.corner_radius, win.scroll)
                };
                let scroll_v = scroll_flags & scroll::VERTICAL != 0;

                let mut flush_r = if scroll_v { 0 } else { (i32::from(corner_radius) - inset_x).max(0) };
                if flush_r > 0 {
                        content.y1 = rect.y1 - inset_x;
                }
                let cap_r = if scroll_v { (i32::from(corner_radius) - inset_x).max(0) } else { 0 };

                let count = self.children(id).filter(|c| self.w(*c).visible).count() as i32;
                if count == 0 || rect_empty(&content) {
                        return;
                }

                let mut total_h = content.y1 - content.y0 + 1;
                let mut row_h = (total_h - gap * (count - 1)) / count;
                if row_h < 1 {
                        if !scroll_v {
                                warn!("ui: window content ({} px) too short for {} stacked rows", total_h, count);
                        }
                        row_h = 1;
                }
                // the withdrawal: rows too short to contain the curve pull the bottom back
                if flush_r > 0 && row_h < flush_r {
                        content.y1 = plain_y1;
                        flush_r = 0;
                        total_h = content.y1 - content.y0 + 1;
                        row_h = ((total_h - gap * (count - 1)) / count).max(1);
                }

                // the content's extent, measured before anything is placed, so the offset is
                // clamped against it FIRST and rows are laid out against a legal offset
                let viewport_w = content.x1 - content.x0 + 1;
                let mut content_h = 0;
                let mut content_w = 0;
                let kids: Vec<WidgetId, N> = self.children(id).filter(|c| self.w(*c).visible).collect();
                for &c in kids.iter() {
                        content_h += Self::row_height(self.w(c), row_h);
                        content_w = content_w.max(Self::row_width(self.w(c), viewport_w));
                }
                content_h += gap * (count - 1);
                let stop_y1 = self.scroll_stop_y1(id);
                let (scroll_x, scroll_y) = {
                        let win = self.w_mut(id).window_mut().expect("a window");
                        win.content_h = content_h;
                        win.content_w = content_w;
                        let mut max_sy = content_h - (stop_y1 - content.y0 + 1);
                        let mut max_sx = content_w - viewport_w;
                        if !scroll_v || max_sy < 0 {
                                max_sy = 0;
                        }
                        if win.scroll & scroll::HORIZONTAL == 0 || max_sx < 0 {
                                max_sx = 0;
                        }
                        win.scroll_y = win.scroll_y.clamp(0, max_sy);
                        win.scroll_x = win.scroll_x.clamp(0, max_sx);
                        (win.scroll_x, win.scroll_y)
                };

                let floor_y = if self.w(id).parent.is_some() { rect.y1 } else { self.height - 1 };
                let mut y = content.y0 - scroll_y;
                let x0 = content.x0 - scroll_x;
                for (index, &c) in kids.iter().enumerate() {
                        let last = index as i32 + 1 == count;
                        let mut h = Self::row_height(self.w(c), row_h);
                        let width = Self::row_width(self.w(c), viewport_w);
                        // the last row of a non-scrolling stack absorbs the division remainder
                        // as well as reaching the bottom edge
                        if last && !scroll_v {
                                h = Self::row_height(self.w(c), content.y1 - y + 1);
                        }
                        let cw = self.w_mut(c);
                        cw.rect = Rect::new(x0, y, x0 + width - 1, y + h - 1);
                        // a flush last row claims the strip beneath it for hit-testing: padding,
                        // border and (for the root) the safe inset, where a thumb reaching for
                        // the bottom button lands. Only when flush: a row that pulled back stops
                        // short on purpose
                        cw.hit_slop_y1 = 0;
                        if last && flush_r > 0 {
                                cw.hit_slop_y1 = (floor_y - cw.rect.y1).clamp(0, 255);
                        }
                        if let Kind::Button(b) = &mut cw.kind {
                                let r = if last && flush_r > 0 {
                                        flush_r
                                } else if last && cap_r > 0 && h >= cap_r {
                                        cap_r
                                } else {
                                        0
                                };
                                b.corner_radius = r as u8;
                                b.corners = if r > 0 { light_draw::corner::BOTTOM } else { light_draw::corner::NONE };
                        }
                        y += h + gap;
                }
                // a child that is itself a laid-out window was arranged against the rect it
                // had BEFORE this pass moved it: re-lay its interior against the new one
                for &c in kids.iter() {
                        self.relayout_window(c);
                }
                self.invalidate_widget(id);
        }
        /// Divide the window's content area into equal-width columns, one per visible
        /// child, `gap` pixels apart: [`layout_stack`](Self::layout_stack) turned on its
        /// side, for an interface on glass wider than it is tall. Columns take the full
        /// content height; a child's `min_w`/`max_w` pins its width. A non-scrolling row's
        /// last column absorbs the division remainder; a row marked
        /// [`scroll::HORIZONTAL`] instead lets its columns run past the frame and drags
        /// sideways, the clamp working exactly as the stack's vertical one does. No
        /// corner-flush treatment either way. Vertical scrolling is pinned off: a row is
        /// never taller than its window.
        pub fn layout_row(&mut self, id: WidgetId, gap: u8) {
                if let Some(win) = self.w_mut(id).window_mut() {
                        win.layout = Layout::Row { gap };
                }
                self.lay_horizontal(id, gap);
        }
        /// The horizontal placement pass of [`layout_row`], without recording the layout kind:
        /// shared with a [`Layout::Linear`] window in a horizontal tree, which stays `Linear`.
        fn lay_horizontal(&mut self, id: WidgetId, gap: u8) {
                let gap = i32::from(gap);
                let content = self.viewport(id);
                let scroll_h = {
                        let win = self.w(id).window().expect("a window");
                        win.scroll & scroll::HORIZONTAL != 0
                };
                let count = self.children(id).filter(|c| self.w(*c).visible).count() as i32;
                if count == 0 || rect_empty(&content) {
                        return;
                }
                let total_w = content.x1 - content.x0 + 1;
                let mut col_w = (total_w - gap * (count - 1)) / count;
                if col_w < 1 {
                        if !scroll_h {
                                warn!("ui: window content ({} px) too narrow for {} columns", total_w, count);
                        }
                        col_w = 1;
                }
                let viewport_h = content.y1 - content.y0 + 1;
                let kids: Vec<WidgetId, N> = self.children(id).filter(|c| self.w(*c).visible).collect();
                //   each column's width, settled before anything is placed so the extent is known
                // and the scroll offset can be clamped against it. With a child marked to stretch
                // (`grow`), pinned siblings keep their own widths wherever they sit and the surplus
                // is split between the grow children -- so a fixed button can flank a stretching one
                // on either side. Without one, the historical rule holds: equal columns, the last
                // absorbing the division remainder. A scrolling row has no surplus, so grow is moot.
                let grow_count = kids.iter().filter(|&&c| self.w(c).grow).count() as i32;
                let mut widths: Vec<i32, N> = Vec::new();
                if grow_count > 0 && !scroll_h {
                        let mut fixed = 0;
                        for &c in kids.iter() {
                                if !self.w(c).grow {
                                        fixed += Self::row_width(self.w(c), 0);
                                }
                        }
                        let surplus = (total_w - gap * (count - 1) - fixed).max(grow_count);
                        let each = surplus / grow_count;
                        let (mut given, mut seen) = (0, 0);
                        for &c in kids.iter() {
                                let w = if self.w(c).grow {
                                        seen += 1;
                                        // the last grow column takes the rounding slack, so the row fills exactly
                                        let w = if seen == grow_count { surplus - given } else { each };
                                        given += each;
                                        w
                                } else {
                                        Self::row_width(self.w(c), 0)
                                };
                                let _ = widths.push(w);
                        }
                } else {
                        for (index, &c) in kids.iter().enumerate() {
                                let last = index as i32 + 1 == count;
                                // the last column of a non-scrolling row absorbs the division remainder;
                                // a scrolling row's columns keep their measured widths
                                let w = if last && !scroll_h {
                                        let used: i32 = widths.iter().sum::<i32>() + gap * (count - 1);
                                        Self::row_width(self.w(c), total_w - used)
                                } else {
                                        Self::row_width(self.w(c), col_w)
                                };
                                let _ = widths.push(w);
                        }
                }
                let content_w = widths.iter().sum::<i32>() + gap * (count - 1);
                let scroll_x = {
                        let win = self.w_mut(id).window_mut().expect("a window");
                        win.content_w = content_w;
                        win.content_h = viewport_h;
                        let mut max_sx = content_w - total_w;
                        if !scroll_h || max_sx < 0 {
                                max_sx = 0;
                        }
                        win.scroll_x = win.scroll_x.clamp(0, max_sx);
                        win.scroll_y = 0;
                        win.scroll_x
                };
                let mut x = content.x0 - scroll_x;
                for (index, &c) in kids.iter().enumerate() {
                        let w = widths[index];
                        let h = Self::row_height(self.w(c), viewport_h);
                        let cw = self.w_mut(c);
                        cw.rect = Rect::new(x, content.y0, x + w - 1, content.y0 + h - 1);
                        cw.hit_slop_y1 = 0;
                        if let Kind::Button(b) = &mut cw.kind {
                                b.corner_radius = 0;
                                b.corners = light_draw::corner::NONE;
                        }
                        x += w + gap;
                }
                // a child that is itself a laid-out window was arranged against the rect it
                // had BEFORE this pass moved it: re-lay its interior against the new one
                for &c in kids.iter() {
                        self.relayout_window(c);
                }
                self.invalidate_widget(id);
        }
        /// Divide the window's content area into `cols` equal columns and as many equal rows as
        /// the visible children fill row-major, `gap` pixels apart on both axes: a keypad, a
        /// palette, a page of icons. Both axes are pinned -- a grid ignores the tree's
        /// [`Axis`] -- and the cells are as uniform as the children's `min`/`max` bounds allow: a
        /// column is as wide as the widest bound among its cells and a row as tall as the
        /// tallest, so one pinned cell resizes its line rather than breaking the grid. The last
        /// column, and the last row of a non-scrolling grid, absorb the division remainders so
        /// the cells reach the content edges. Rows pinned taller than their share overflow, and a
        /// window marked [`scroll::VERTICAL`] scrolls them, the clamp working as the stack's does
        /// (and [`scroll::HORIZONTAL`] the same for pinned columns). No corner-flush treatment:
        /// a grid's cells are all alike.
        pub fn layout_grid(&mut self, id: WidgetId, cols: u8, gap: u8) {
                if let Some(win) = self.w_mut(id).window_mut() {
                        win.layout = Layout::Grid { cols, gap };
                }
                self.lay_grid(id, cols, gap);
        }
        fn lay_grid(&mut self, id: WidgetId, cols: u8, gap: u8) {
                let gap = i32::from(gap);
                let content = self.viewport(id);
                let (scroll_v, scroll_h) = {
                        let win = self.w(id).window().expect("a window");
                        (win.scroll & scroll::VERTICAL != 0, win.scroll & scroll::HORIZONTAL != 0)
                };
                let kids: Vec<WidgetId, N> = self.children(id).filter(|c| self.w(*c).visible).collect();
                let count = kids.len() as i32;
                if count == 0 || rect_empty(&content) {
                        return;
                }
                // 0 columns is nonsense; and a grid never carries an empty trailing column
                let cols = i32::from(cols).max(1).min(count);
                let rows = (count + cols - 1) / cols;
                let total_w = content.x1 - content.x0 + 1;
                let total_h = content.y1 - content.y0 + 1;
                let mut col_w = (total_w - gap * (cols - 1)) / cols;
                if col_w < 1 {
                        if !scroll_h {
                                warn!("ui: window content ({} px) too narrow for {} grid columns", total_w, cols);
                        }
                        col_w = 1;
                }
                let mut row_h = (total_h - gap * (rows - 1)) / rows;
                if row_h < 1 {
                        if !scroll_v {
                                warn!("ui: window content ({} px) too short for {} grid rows", total_h, rows);
                        }
                        row_h = 1;
                }
                //   every line's size settled before anything is placed, so the extent is known and
                // the offset can be clamped against it. A line takes the largest of its cells'
                // bounded shares; the last line of a non-scrolling axis takes the remainder
                // instead, as a stack's last row does, so a pinned line before it shrinks the
                // last rather than pushing it out of the window
                let mut widths: Vec<i32, N> = Vec::new();
                for col in 0..cols {
                        let last = col + 1 == cols;
                        let share = if last && !scroll_h { (total_w - widths.iter().sum::<i32>() - gap * (cols - 1)).max(1) } else { col_w };
                        let w = kids.iter().skip(col as usize).step_by(cols as usize).map(|&c| Self::row_width(self.w(c), share)).max().unwrap_or(share);
                        let _ = widths.push(w);
                }
                let mut heights: Vec<i32, N> = Vec::new();
                for row in 0..rows {
                        let last = row + 1 == rows;
                        let share = if last && !scroll_v { (total_h - heights.iter().sum::<i32>() - gap * (rows - 1)).max(1) } else { row_h };
                        let h = kids.iter().skip((row * cols) as usize).take(cols as usize).map(|&c| Self::row_height(self.w(c), share)).max().unwrap_or(share);
                        let _ = heights.push(h);
                }
                let content_w = widths.iter().sum::<i32>() + gap * (cols - 1);
                let content_h = heights.iter().sum::<i32>() + gap * (rows - 1);
                let (scroll_x, scroll_y) = {
                        let win = self.w_mut(id).window_mut().expect("a window");
                        win.content_w = content_w;
                        win.content_h = content_h;
                        let mut max_sx = content_w - total_w;
                        let mut max_sy = content_h - total_h;
                        if !scroll_h || max_sx < 0 {
                                max_sx = 0;
                        }
                        if !scroll_v || max_sy < 0 {
                                max_sy = 0;
                        }
                        win.scroll_x = win.scroll_x.clamp(0, max_sx);
                        win.scroll_y = win.scroll_y.clamp(0, max_sy);
                        (win.scroll_x, win.scroll_y)
                };
                let mut y = content.y0 - scroll_y;
                for (row, &h) in heights.iter().enumerate() {
                        let mut x = content.x0 - scroll_x;
                        for (col, &w) in widths.iter().enumerate() {
                                let Some(&c) = kids.get(row * cols as usize + col) else { break };
                                let cw = self.w_mut(c);
                                cw.rect = Rect::new(x, y, x + w - 1, y + h - 1);
                                cw.hit_slop_y1 = 0;
                                if let Kind::Button(b) = &mut cw.kind {
                                        b.corner_radius = 0;
                                        b.corners = light_draw::corner::NONE;
                                }
                                x += w + gap;
                        }
                        y += h + gap;
                }
                // a child that is itself a laid-out window was arranged against the rect it
                // had BEFORE this pass moved it: re-lay its interior against the new one
                for &c in kids.iter() {
                        self.relayout_window(c);
                }
                self.invalidate_widget(id);
        }
        /// Re-run whichever layout the window recorded. A no-op for hand-placed children.
        pub(crate) fn relayout_window(&mut self, id: WidgetId) {
                let Some(win) = self.w(id).window() else { return };
                match win.layout {
                        Layout::Stack { gap } => self.layout_stack(id, gap),
                        Layout::Row { gap } => self.layout_row(id, gap),
                        Layout::Linear { gap } => self.layout_linear(id, gap),
                        Layout::Grid { cols, gap } => self.layout_grid(id, cols, gap),
                        Layout::None => {}
                }
        }
        /// Lay a window along the tree's current [`Axis`] and record it as [`Layout::Linear`],
        /// so a later [`set_layout_axis`](Self::set_layout_axis) re-lays it the other way. The
        /// public entry for a generic layout.
        pub fn layout_linear(&mut self, id: WidgetId, gap: u8) {
                if let Some(win) = self.w_mut(id).window_mut() {
                        win.layout = Layout::Linear { gap };
                }
                match self.layout_axis {
                        Axis::Vertical => self.lay_vertical(id, gap),
                        Axis::Horizontal => self.lay_horizontal(id, gap),
                }
        }
        /// Round the window's frame and keep its content clear of the curve. The clearance is NOT
        /// uniform: a corner only eats into rows within the radius of the top and bottom edges,
        /// so content is pushed down and up by the radius while the horizontal inset stays at
        /// border + padding -- insetting all four sides by the radius would give back exactly the
        /// width this exists to recover. Re-lays-out.
        pub fn set_corner_radius(&mut self, id: WidgetId, radius: u8) {
                let Some(win) = self.w_mut(id).window_mut() else { return };
                if win.corner_radius == radius {
                        return;
                }
                win.corner_radius = radius;
                self.relayout_window(id);
                self.invalidate_widget(id);
        }
        /// Re-run layout against the canvas as it is now: the root is resized to fill it (less the
        /// safe inset), then every window re-applies the arrangement it recorded, in pre-order so a
        /// window is resized by its parent before it lays out its own children.
        pub fn relayout(&mut self) {
                let Some(root) = self.root else { return };
                let inset = self.safe_inset;
                self.w_mut(root).rect = Rect::new(inset, inset, self.width - 1 - inset, self.height - 1 - inset);
                let mut cur = Some(root);
                while let Some(id) = cur {
                        cur = self.next(id, root);
                        self.relayout_window(id);
                }
        }
}
