use heapless::Vec;
use light_core::trace;
use crate::{Ui, WidgetId, Layout, Rect, scroll, rect_empty};

impl<A: Copy, const N: usize> Ui<A, N> {
        /// Mark a window scrollable along the given axes. The layout pass this triggers is also
        /// what clamps the offset, putting content back inside the frame when an axis stops.
        pub fn set_scroll(&mut self, id: WidgetId, flags: u8) {
                let Some(win) = self.w_mut(id).window_mut() else { return };
                if win.scroll == flags {
                        return;
                }
                win.scroll = flags;
                self.relayout_window(id);
                self.invalidate_widget(id);
        }
        /// Content extents for a hand-placed window, from its children's rects with the offset
        /// added back -- the extent is a property of the content, not of where it is scrolled to.
        fn measure_content(&mut self, id: WidgetId) {
                let vp = self.viewport(id);
                let (sx, sy) = {
                        let win = self.w(id).window().expect("a window");
                        (win.scroll_x, win.scroll_y)
                };
                let mut w = 0;
                let mut h = 0;
                for c in self.children(id).collect::<Vec<_, N>>() {
                        let cw = self.w(c);
                        if !cw.visible {
                                continue;
                        }
                        w = w.max(cw.rect.x1 + sx - vp.x0 + 1);
                        h = h.max(cw.rect.y1 + sy - vp.y0 + 1);
                }
                let win = self.w_mut(id).window_mut().expect("a window");
                win.content_w = w;
                win.content_h = h;
        }
        /// Scroll to an absolute offset, clamped to `[0, content - viewport]`: the content's far
        /// edge never comes past the viewport's, and an axis without its flag never moves. That
        /// clamp is the entire safety argument for scrolling; everything else is a rect shift.
        /// Returns whether anything moved.
        pub fn scroll_to(&mut self, id: WidgetId, x: i32, y: i32) -> bool {
                if self.w(id).window().is_none() {
                        return false;
                }
                let vp = self.viewport(id);
                if rect_empty(&vp) {
                        return false;
                }
                if self.w(id).window().expect("a window").layout == Layout::None {
                        self.measure_content(id);
                }
                let stop_y1 = self.scroll_stop_y1(id);
                let (nx, ny, sx, sy) = {
                        let win = self.w(id).window().expect("a window");
                        let max_sx = if win.scroll & scroll::HORIZONTAL != 0 { (win.content_w - (vp.x1 - vp.x0 + 1)).max(0) } else { 0 };
                        let max_sy = if win.scroll & scroll::VERTICAL != 0 { (win.content_h - (stop_y1 - vp.y0 + 1)).max(0) } else { 0 };
                        let nx = x.clamp(0, max_sx);
                        let ny = y.clamp(0, max_sy);
                        // the content shifts OPPOSITE to the offset's change
                        (nx, ny, win.scroll_x - nx, win.scroll_y - ny)
                };
                if sx == 0 && sy == 0 {
                        return false;
                }
                {
                        let win = self.w_mut(id).window_mut().expect("a window");
                        win.scroll_x = nx;
                        win.scroll_y = ny;
                }
                // rects stay ABSOLUTE: scrolling shifts everything under the window
                let mut c = self.w(id).first_child;
                while let Some(cid) = c {
                        let cw = self.w_mut(cid);
                        cw.rect = Rect::new(cw.rect.x0 + sx, cw.rect.y0 + sy, cw.rect.x1 + sx, cw.rect.y1 + sy);
                        c = self.next(cid, id);
                }
                self.invalidate_widget(id);
                trace!("ui: window scrolled to ({nx}, {ny})");
                true
        }
        /// Scroll by `(dx, dy)` -- positive `dy` scrolls DOWN the content, i.e. the content moves
        /// up through the frame. Clamped as [`scroll_to`](Self::scroll_to).
        pub fn scroll_by(&mut self, id: WidgetId, dx: i32, dy: i32) -> bool {
                let Some(win) = self.w(id).window() else { return false };
                let (x, y) = (win.scroll_x + dx, win.scroll_y + dy);
                self.scroll_to(id, x, y)
        }
        /// Scroll every scrollable ancestor of `id` by as little as brings it into view --
        /// innermost first, since an outer window's decision has to see where the inner one left
        /// it. Called by [`set_focus`](Self::set_focus), so focus-driven navigation scrolls for free.
        pub fn scroll_into_view(&mut self, id: WidgetId) {
                let mut p = self.w(id).parent;
                while let Some(pid) = p {
                        p = self.w(pid).parent;
                        if !self.w(pid).is_scrolling_window() {
                                continue;
                        }
                        let mut vp = self.viewport(pid);
                        // the last row's "in view" reaches to the scroll stop, so focusing it
                        // rides it down flush against the frame, where a drag leaves it
                        if self.last_visible_child(pid) == Some(id) {
                                vp.y1 = self.scroll_stop_y1(pid);
                        }
                        let r = self.w(id).rect;
                        // as little as brings it in: far edge first, then the near edge overrides,
                        // so a widget taller than the viewport shows its top
                        let mut dx = 0;
                        let mut dy = 0;
                        if r.y1 > vp.y1 {
                                dy = r.y1 - vp.y1;
                        }
                        if r.y0 - dy < vp.y0 {
                                dy = r.y0 - vp.y0;
                        }
                        if r.x1 > vp.x1 {
                                dx = r.x1 - vp.x1;
                        }
                        if r.x0 - dx < vp.x0 {
                                dx = r.x0 - vp.x0;
                        }
                        if dx != 0 || dy != 0 {
                                self.scroll_by(pid, dx, dy);
                        }
                }
        }
}
