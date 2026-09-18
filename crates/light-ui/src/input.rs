use light_core::{debug, error, trace};
use crate::{Ui, WidgetId, Kind, Nav, Rect, rect_contains, rect_intersect};

/// How far a finger may wander (logical pixels, either axis) before a touch stops being a
/// prospective tap and becomes a drag. 16, raised from a first guess of 8, which classified
/// real taps as travel: a fingertip rolls several pixels as it presses and the CST816T adds its
/// own jitter, so taps were dropped or turned into 1 px scrolls. Still below any swipe threshold.
pub const DRAG_SLOP: i32 = 16;
/// How long a contact must rest -- within [`DRAG_SLOP`] of where it landed -- before its
/// release counts as a TAP. A brush or a jittered graze is shorter than this and is dropped
/// rather than fired as a press: the deliberateness filter. A small fraction of a second, so
/// an ordinary tap (~100 ms) clears it comfortably while an accidental flick does not.
pub const TAP_MIN_HOLD_US: u64 = 50_000;
/// How long a button wears its pressed look after activation: a brief acknowledgement so a
/// tap reads as landed even when the action behind it (opening a card, starting a take) is
/// slow to change anything else. See [`Ui::touch`].
pub const ACTIVATE_FLASH_US: u64 = 120_000;
/// What [`Ui::touch`] did with the sample it was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Touch<A> {
        None,
        /// A finger is down and the touch is undecided.
        Pending,
        /// Drag-scrolling, from the sample it engaged on. The caller should tell its gesture
        /// tracker the movement was consumed, or the release also classifies as a swipe.
        Drag,
        /// The release of a drag.
        DragEnd,
        /// The release of a tap: whether it landed on a widget, and what that widget emitted.
        Tap { hit: bool, emitted: Option<A> },
}
/// A swipe's direction in the LOGICAL frame -- the one the user is looking at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwipeDir {
        Up,
        Down,
        Left,
        Right,
}

impl<A: Copy, const N: usize> Ui<A, N> {
        fn is_focusable(&self, id: WidgetId) -> bool {
                let w = self.w(id);
                w.focusable && w.visible && w.enabled
        }
        /// One pass collecting the focusable before/after the focused one including both wrap
        /// cases; a backward pre-order traversal has no cheap formulation.
        fn focus_relative(&self, forward: bool) -> Option<WidgetId> {
                let mut first = None;
                let mut last = None;
                let mut before = None;
                let mut after = None;
                let mut seen = false;
                for id in self.walk() {
                        if !self.is_focusable(id) {
                                continue;
                        }
                        if first.is_none() {
                                first = Some(id);
                        }
                        if seen && after.is_none() {
                                after = Some(id);
                        }
                        if Some(id) == self.focused {
                                seen = true;
                                before = last;
                        }
                        last = Some(id);
                }
                first?;
                // no current focus, or it was hidden/disabled out of the cycle: start over
                if !seen {
                        return first;
                }
                if forward { after.or(first) } else { before.or(last) }
        }
        pub fn set_focus(&mut self, id: Option<WidgetId>) {
                if self.focused == id {
                        return;
                }
                // both the widget losing the highlight and the one gaining it change appearance
                if let Some(old) = self.focused {
                        if self.get(old).is_some() {
                                self.invalidate_widget(old);
                        }
                }
                self.focused = id;
                if let Some(id) = id {
                        // brought into its scrolling ancestor's viewport BEFORE the invalidation,
                        // so the rect invalidated is where the widget actually ended up
                        self.scroll_into_view(id);
                        self.invalidate_widget(id);
                }
        }
        pub fn focus_next(&mut self) {
                let id = self.focus_relative(true);
                self.set_focus(id);
        }
        pub fn focus_prev(&mut self) {
                let id = self.focus_relative(false);
                self.set_focus(id);
        }
        /// Fire a button: returns what it emits, then navigates if it says to. EVERYTHING is read
        /// before navigation, because navigation destroys the tree, the button included -- found
        /// the hard way in the C implementation this replaces, where reading a field after a
        /// navigating handler dereferenced freed memory and wedged the core.
        pub(crate) fn fire(&mut self, id: WidgetId) -> Option<A> {
                let (emit, nav, label) = match self.w(id).button() {
                        Some(b) => (b.emit, b.nav, b.label),
                        None => return None,
                };
                debug!("ui: button '{label}' activated");
                match nav {
                        Nav::Stay => {}
                        Nav::To(page) => {
                                if let Err(e) = self.navigate(page) {
                                        error!("ui: navigation from '{label}' failed: {e:?}");
                                }
                        }
                        Nav::Back => {
                                if !self.navigate_back() {
                                        debug!("ui: back from '{label}': nowhere to go");
                                }
                        }
                }
                emit
        }
        /// Activate the focused widget: what it emitted, if anything.
        pub fn activate(&mut self) -> Option<A> {
                let id = self.focused?;
                if !self.is_focusable(id) {
                        return None;
                }
                self.fire(id)
        }
        /// A panel point into logical coordinates, clamped into the canvas.
        pub(crate) fn untransform(&self, x: i32, y: i32) -> (i32, i32) {
                let m = &self.transform;
                let det = m.a * m.d - m.b * m.c;
                let px = x - m.tx;
                let py = y - m.ty;
                (((m.d * px - m.b * py) * det).clamp(0, (self.width - 1).max(0)), ((m.a * py - m.c * px) * det).clamp(0, (self.height - 1).max(0)))
        }
        pub(crate) fn canvas_rect(&self) -> Rect {
                Rect::new(0, 0, self.width - 1, self.height - 1)
        }
        fn widget_hit(&self, id: WidgetId, x: i32, y: i32) -> bool {
                let w = self.w(id);
                let mut r = w.rect;
                r.y1 += w.hit_slop_y1;
                rect_contains(&r, x, y)
        }
        /// Hit-testing with the same clipping the paint path applies: a widget scrolled out of its
        /// window's viewport is exactly as untouchable as it is invisible. Last match wins: deeper
        /// and later-drawn widgets are visited last, and those are on top where they overlap.
        fn hit_test(&self, id: WidgetId, mut clip: Rect, x: i32, y: i32, mut best: Option<WidgetId>) -> Option<WidgetId> {
                if !self.w(id).visible {
                        return best;
                }
                if self.is_focusable(id) && rect_contains(&clip, x, y) && self.widget_hit(id, x, y) {
                        best = Some(id);
                }
                if self.w(id).is_scrolling_window() {
                        let mut vp = self.viewport(id);
                        vp.y1 = vp.y1.max(self.scroll_stop_y1(id));
                        if !rect_intersect(&mut clip, &vp) {
                                return best;
                        }
                }
                for c in self.children(id) {
                        best = self.hit_test(c, clip, x, y, best);
                }
                best
        }
        /// The innermost scrollable window whose viewport contains the point: a drag belongs to
        /// the surface actually under the finger, not an ancestor that also scrolls.
        fn scroll_window_at(&self, id: WidgetId, mut clip: Rect, x: i32, y: i32, mut best: Option<WidgetId>) -> Option<WidgetId> {
                if !self.w(id).visible {
                        return best;
                }
                if self.w(id).is_scrolling_window() {
                        let mut vp = self.viewport(id);
                        vp.y1 = vp.y1.max(self.scroll_stop_y1(id));
                        if !rect_intersect(&mut vp, &clip) {
                                return best;
                        }
                        if rect_contains(&vp, x, y) {
                                best = Some(id);
                        }
                        clip = vp;
                }
                for c in self.children(id) {
                        best = self.scroll_window_at(c, clip, x, y, best);
                }
                best
        }
        /// Hit-test a PANEL point and, if it lands on an actionable widget, focus AND activate it
        /// -- a touch is a complete interaction. Returns `(hit, emitted)`.
        pub fn press_at(&mut self, x: u16, y: u16) -> (bool, Option<A>) {
                //   silently ignored while the interface is turning: the panel shows the
                // pre-rotation image being animated while the tree is laid out for the OLD
                // rotation and the transform is not yet the new one, so a tap would resolve
                // against a layout matching neither what is on screen nor where it will settle
                if self.rotating {
                        return (false, None);
                }
                let (lx, ly) = self.untransform(i32::from(x), i32::from(y));
                let Some(root) = self.root else { return (false, None) };
                let Some(hit) = self.hit_test(root, self.canvas_rect(), lx, ly, None) else { return (false, None) };
                self.set_focus(Some(hit));
                (true, self.fire(hit))
        }
        fn touch_reset(&mut self) {
                self.touch_down = false;
                self.touch_dragging = false;
                self.touch_moved = false;
                self.drag_window = None;
        }
        /// The stateful entry point: feed it the panel's CURRENT state every tick -- position and
        /// whether a finger is down -- and it runs the whole tap-versus-drag interaction.
        ///
        /// - A TAP fires on RELEASE only when the contact never strayed beyond `drag_slop` from
        ///   where it landed AND rested there at least [`TAP_MIN_HOLD_US`] -- a deliberate press,
        ///   not a brush or a jittered graze. It is delivered at the START point (with scrollable
        ///   content a down-edge fires on every drag's first contact, so the start is the intent).
        /// - A touch that moves beyond the slop over a scrollable window becomes a DRAG: the window
        ///   under the START point scrolls to follow the finger until release.
        /// - A touch that travels with nothing scrollable under it commits to neither, and the
        ///   release is left for the gesture pipeline -- a swipe on a non-scrolling page navigates.
        ///
        /// `now_us` is the current time (the same clock the animations run on); the caller passes
        /// it every sample so the hold can be measured without the toolkit owning a clock.
        pub fn touch(&mut self, x: u16, y: u16, touching: bool, now_us: u64) -> Touch<A> {
                // mid-rotation samples reset the tracker rather than being remembered: the
                // layout the touch began against is being replaced
                if self.rotating {
                        self.touch_reset();
                        return Touch::None;
                }
                if !touching {
                        if !self.touch_down {
                                return Touch::None;
                        }
                        let was_drag = self.touch_dragging;
                        let moved = self.touch_moved;
                        let held_us = now_us.saturating_sub(self.touch_start_us);
                        let (sx, sy) = self.touch_start;
                        let adx = (self.touch_last.0 - sx).abs();
                        let ady = (self.touch_last.1 - sy).abs();
                        self.touch_reset();
                        //   a tap must have stayed put (never beyond the slop, start to last) AND
                        // rested long enough. Too brief or wandered = intent unclear, dropped
                        let strayed = moved || adx > self.drag_slop || ady > self.drag_slop;
                        let too_brief = held_us < TAP_MIN_HOLD_US;
                        let verdict = if was_drag { "drag" } else if strayed { "strayed" } else if too_brief { "too brief" } else { "tap" };
                        // the verdict and the numbers, because "taps sometimes don't work" is
                        // otherwise undiagnosable; TRACE, since it fires on every touch
                        trace!("ui: release moved ({adx}, {ady}) slop {}, held {} us -> {}", self.drag_slop, held_us, verdict);
                        if was_drag {
                                return Touch::DragEnd;
                        }
                        if strayed || too_brief {
                                return Touch::None;
                        }
                        let hit = self.root.and_then(|root| self.hit_test(root, self.canvas_rect(), sx, sy, None));
                        return match hit {
                                Some(id) => {
                                        self.set_focus(Some(id));
                                        //   acknowledge the tap at once: a button wears its pressed
                                        // look for ACTIVATE_FLASH_US whether or not it was focused,
                                        // so a slow handler (a card open, a take starting) no longer
                                        // reads as a dropped tap. Set before firing, so a fire that
                                        // navigates clears it as the old tree is destroyed.
                                        if matches!(self.w(id).kind, Kind::Button(_)) {
                                                self.flash = Some((id, now_us + ACTIVATE_FLASH_US));
                                                self.invalidate_widget(id);
                                        }
                                        Touch::Tap { hit: true, emitted: self.fire(id) }
                                }
                                None => Touch::Tap { hit: false, emitted: None },
                        };
                }

                let (lx, ly) = self.untransform(i32::from(x), i32::from(y));
                if !self.touch_down {
                        self.touch_down = true;
                        self.touch_dragging = false;
                        self.touch_moved = false;
                        self.drag_window = None;
                        self.touch_start = (lx, ly);
                        self.touch_last = (lx, ly);
                        self.touch_start_us = now_us;
                        return Touch::Pending;
                }

                if !self.touch_dragging {
                        let adx = (lx - self.touch_start.0).abs();
                        let ady = (ly - self.touch_start.1).abs();
                        if adx > self.drag_slop || ady > self.drag_slop {
                                //   strayed beyond the slop: no longer a tap, whatever happens next
                                self.touch_moved = true;
                                let (sx, sy) = self.touch_start;
                                let target = self.root.and_then(|root| self.scroll_window_at(root, self.canvas_rect(), sx, sy, None));
                                if let Some(t) = target {
                                        self.touch_dragging = true;
                                        self.drag_window = Some(t);
                                        // engage with the full movement since the touch began, so
                                        // the content catches up rather than staying a slop behind;
                                        // the content follows the finger, hence the negation
                                        self.scroll_by(t, sx - lx, sy - ly);
                                }
                        }
                        self.touch_last = (lx, ly);
                        return if self.touch_dragging { Touch::Drag } else { Touch::Pending };
                }

                if let Some(t) = self.drag_window {
                        if self.get(t).is_some() {
                                let (px, py) = self.touch_last;
                                self.scroll_by(t, px - lx, py - ly);
                        }
                }
                self.touch_last = (lx, ly);
                Touch::Drag
        }
        /// Classify a swipe from its two endpoints in PANEL coordinates, in the LOGICAL frame. A
        /// controller classifies gestures in the panel's frame, fixed to the glass, while the user
        /// swipes relative to the interface, which rotates; at 90 or 270 the two are perpendicular.
        /// Both endpoints go through the same untransform a tap does, so only one place knows how
        /// the frames relate. `None` for no dominant axis, including both ends clamping to one edge.
        pub fn swipe_direction(&self, start: (u16, u16), end: (u16, u16)) -> Option<SwipeDir> {
                // discarded while turning, for the reason a tap is
                if self.rotating {
                        return None;
                }
                let (ax, ay) = self.untransform(i32::from(start.0), i32::from(start.1));
                let (bx, by) = self.untransform(i32::from(end.0), i32::from(end.1));
                let (dx, dy) = (bx - ax, by - ay);
                if dx == 0 && dy == 0 {
                        return None;
                }
                // the dominant axis decides; ties go to horizontal, consistently
                Some(if dx.abs() >= dy.abs() {
                        if dx > 0 { SwipeDir::Right } else { SwipeDir::Left }
                } else if dy > 0 {
                        SwipeDir::Down
                } else {
                        SwipeDir::Up
                })
        }
}
