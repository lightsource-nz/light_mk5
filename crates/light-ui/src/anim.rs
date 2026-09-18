use light_core::debug;
use light_draw::{Rotation, Flip, Region};
use light_display::{Display, DisplayDriver};
use light_display::frames::FrameLayer;
use crate::{Ui, Style, Rect};

/// How long a rotation takes to animate: long enough to read as a turn rather than a glitch,
/// short enough not to feel like waiting. Input is still collected during it; only the drawing
/// is given over to the animation.
pub const ROTATE_MS: u32 = 280;
/// How long a page transition takes. Shorter than a rotation: a rotation re-orients the whole
/// interface and wants to be followed, while a page change is a step through a structure the
/// user already has in mind, and waiting for it is what makes an interface feel slow.
pub const PAGE_MOVE_MS: u32 = 180;
/// What one pass of an animation did.
pub(crate) enum Step {
        Drew,
        Waiting,
        Finished,
}
fn quadrant(r: Rotation) -> i32 {
        match r {
                Rotation::R0 => 0,
                Rotation::R90 => 1,
                Rotation::R180 => 2,
                Rotation::R270 => 3,
        }
}
/// The shortest signed turn between two quadrants, in degrees: -90, 0, 90 or 180. Going the
/// long way round would animate three quarters of a turn to reach a neighbour.
fn rotation_delta_degrees(from: Rotation, to: Rotation) -> i32 {
        let mut q = (quadrant(to) + 4 - quadrant(from)) % 4;
        if q == 3 {
                q = -1;
        }
        q * 90
}

impl<A: Copy, const N: usize> Ui<A, N> {
        pub fn is_animating(&self) -> bool {
                //   an active press flash keeps the loop rendering so the deadline is noticed and
                // the button reverts on its own
                self.rotating || self.page_moving || self.flash.is_some()
        }
        /// Rotate the whole interface, keeping it upright as the device is turned. Re-orients the
        /// layer, re-lays-out (the canvas aspect has just flipped) and repaints everything. Safe
        /// between frames precisely because every frame is a full repaint. A no-op when unchanged.
        ///
        /// The toolkit knows nothing about orientation SENSORS: the application maps its IMU's
        /// orientation onto a rotation, because that depends on whether the panel is natively
        /// portrait or landscape, which is a board fact.
        pub fn set_rotation(&mut self, layer: &mut FrameLayer, rotation: Rotation) {
                //   compared against the target rather than the live rotation: mid-animation the
                // live one is still the OLD value, so a repeated orientation report would
                // otherwise restart the turn on every tick
                if self.rotating {
                        if self.rotate_target == rotation {
                                return;
                        }
                } else if layer.rotation() == rotation {
                        return;
                }
                if self.page_moving {
                        //   the target is recorded rather than the rotation restarted from
                        // scratch, so a board turned twice mid-transition settles where it ended up
                        self.rotate_deferred = Some(rotation);
                        debug!("ui: rotation to {rotation:?} deferred until the page transition finishes");
                        return;
                }
                let from = if self.rotating { self.rotate_target } else { layer.rotation() };
                self.rotating = true;
                self.rotate_started = false;
                self.rotate_target = rotation;
                self.rotate_degrees = rotation_delta_degrees(from, rotation);
                self.dirty = true;
                debug!("ui: rotating {} degrees to {rotation:?}", self.rotate_degrees);
        }
        /// Apply a rotation for real: re-orient the layer, drop the regions measured against the
        /// old geometry, re-lay-out and repaint everything -- the panel still shows the old
        /// arrangement in the old orientation, so nothing short of the whole canvas is safe.
        fn commit_rotation(&mut self, layer: &mut FrameLayer, rotation: Rotation) {
                layer.set_orientation(rotation, Flip::None);
                layer.invalidate_all();
                self.fit(layer);
                self.invalidate_all();
                let (w, h) = self.logical_size();
                debug!("ui rotation now {rotation:?}, canvas {w}x{h}");
        }
        /// One step of the rotation animation: a frame of the turn drawn, nothing drawn because
        /// the display was not ready, or the turn finished and the real rotation applied -- in
        /// which case the caller draws the settled tree without waiting a pass.
        pub(crate) fn rotation_step<D: DisplayDriver>(&mut self, layer: &mut FrameLayer, display: &mut Display<'_, D>, now_us: u64) -> Step {
                if !self.rotate_started {
                        if !display.is_double_buffered() || !display.format().is_rgb565() {
                                // nowhere to hold the image, or nothing to rotate it with: a
                                // correct snap beats a broken animation
                                let target = self.rotate_target;
                                self.rotating = false;
                                self.commit_rotation(layer, target);
                                return Step::Finished;
                        }
                        if !display.freeze() {
                                // the panel is still reading the front buffer; capture next pass
                                return Step::Waiting;
                        }
                        self.rotate_started = true;
                        self.rotate_start_us = now_us;
                }
                let elapsed = now_us.saturating_sub(self.rotate_start_us);
                if elapsed >= u64::from(self.rotate_ms) * 1000 {
                        // swapping back on before committing, so the repaint that follows runs
                        // against a normal double-buffered display again
                        display.thaw();
                        self.rotating = false;
                        let target = self.rotate_target;
                        self.commit_rotation(layer, target);
                        return Step::Finished;
                }
                let Some(c) = layer.frame_begin(display, now_us) else { return Step::Waiting };
                drop(c);
                // linear in time: at roughly ten frames for the whole turn an eased curve is
                // below what the eye picks out, and linear keeps the angle predictable.
                // Shrunk to whatever still fits, or the corners are sliced off mid-turn
                let angle = ((self.rotate_degrees as i64 * elapsed as i64) / (i64::from(self.rotate_ms) * 1000)) as i16;
                if let Some((front, captured)) = display.frame_and_capture() {
                        let mut c = layer.canvas(front);
                        let scale = c.scale_inscribed(angle);
                        c.blit_rotated(captured, angle, scale);
                }
                layer.invalidate_all();
                layer.frame_end(display);
                Step::Drew
        }
        /// One step of a page transition; the same contract as `rotation_step`.
        pub(crate) fn page_step<D: DisplayDriver>(&mut self, layer: &mut FrameLayer, display: &mut Display<'_, D>, style: &Style<'_>, now_us: u64) -> Step {
                if !self.page_move_started {
                        //   the slide runs one of three ways. CAPTURE (a back buffer): the outgoing
                        // is frozen and blit. On ONE buffer a mirrored slide is reveal-off on open,
                        // cover-in on close -- the outgoing page is the mover, sliding off to reveal
                        // the child and back to cover it -- and each half wants a different
                        // single-buffer mechanic: REGION scrolls the outgoing off in place (the
                        // reveal, RGB565, opted in, no second frame), while OVER redraws the incoming
                        // at a shrinking offset (the cover, any format). So a region board reveals
                        // forward with REGION and covers back with OVER; everything else
                        // single-buffered stays on OVER both ways.
                        let capture = display.is_double_buffered() && display.format().is_rgb565();
                        let region = !capture && !self.page_move_back && display.region_buffering();
                        self.page_move_region = region;
                        self.page_move_over = !capture && !region;
                        if self.page_move_over {
                                //   logical space end to end -- the canvas transform does the
                                // physical mapping -- so direction is just the arrival side:
                                // forward from logical +x, back from -x, like the capture mode.
                                // The vertical pair mirrors it a quarter-turn: forward drops in
                                // from logical -y, back rises from +y
                                let (w, h) = layer.logical_size();
                                //   over mode only ever slides the INCOMING in (no capture to move
                                // the outgoing), so it approximates the cover along the descent's
                                // axis. The offset shrinks from `span` to zero, so a positive unit
                                // starts the incoming past the far edge and brings it back this
                                // way: the vertical axis takes the descent's sign directly (down
                                // enters from the top), the horizontal one its negation (left
                                // enters from the right). Back reverses either.
                                let asign = self.page_move_descent.axis_sign();
                                //   normally back reverses the arrival side. But when this OVER is a
                                // region board's cover-BACK (the reveal-forward ran as a shift), the
                                // cover must enter the edge the reveal LEFT from, so it does not
                                // reverse -- and only on the vertical axis, since the horizontal one
                                // already negates the sign below, so its mirror falls out with the
                                // ordinary reversal.
                                let region_cover_back = display.region_buffering();
                                let dir = if self.page_move_back && !(region_cover_back && self.page_move_descent.vertical()) { -1 } else { 1 };
                                if self.page_move_descent.vertical() {
                                        self.page_move_dx = 0;
                                        self.page_move_dy = asign * dir;
                                        self.page_move_span = i32::from(h);
                                } else {
                                        self.page_move_dx = -asign * dir;
                                        self.page_move_dy = 0;
                                        self.page_move_span = i32::from(w);
                                }
                        } else if region {
                                //   region runs the OPEN half only (the reveal): the outgoing is
                                // already in the live buffer, to be scrolled off in place -- no freeze,
                                // no second frame. The physical unit and span are the capture reveal's,
                                // taken from the transform so the shift is rotation-correct; the OVER
                                // cover-back (above) is aligned to leave and return the same edge.
                                let m = layer.transform();
                                let asign = self.page_move_descent.axis_sign();
                                let (ux, uy) = if self.page_move_descent.vertical() {
                                        (m.b.signum(), m.d.signum())
                                } else {
                                        (m.a.signum(), m.c.signum())
                                };
                                let sign = asign;
                                let (pw, ph) = layer.physical_size();
                                self.page_move_dx = sign * ux;
                                self.page_move_dy = sign * uy;
                                self.page_move_span = if ux != 0 { i32::from(pw) } else { i32::from(ph) };
                        } else {
                                //   a mirrored slide: OPEN reveals, CLOSE covers. On open (forward)
                                // freeze the outgoing into the back and slide it off the live incoming
                                // -- the outgoing leaves, the child is revealed. On close (back) render
                                // the incoming (the returning parent) into the back ONCE and blit it
                                // back on over the static outgoing (the child) -- the parent returns,
                                // covering the child. Same page, same edge, one motion reversed.
                                let cover = self.page_move_back;
                                if cover {
                                        let Some(back) = display.freeze_render() else {
                                                return Step::Waiting; // busy: try next pass
                                        };
                                        let mut c = layer.canvas(back);
                                        c.clear();
                                        self.paint(&mut c, style);
                                        drop(c);
                                } else if !display.freeze() {
                                        return Step::Waiting; // busy: capture next pass
                                }
                                //   the direction is chosen in LOGICAL terms and converted here,
                                // because the blit works in physical space: the transform's a and c
                                // are the physical components of logical +x (b and d of +y), exactly
                                // one of each pair non-zero for a pure rotation, so this picks the
                                // axis the viewer calls horizontal (or vertical) whatever the board's
                                // orientation. The sign is the descent axis's alone and does NOT turn
                                // on `back`: the reveal slides the outgoing toward that unit, and the
                                // cover brings the incoming back FROM it (its offset shrinking to
                                // zero), so open and close leave and return the same edge.
                                let m = layer.transform();
                                let asign = self.page_move_descent.axis_sign();
                                let (ux, uy) = if self.page_move_descent.vertical() {
                                        (m.b.signum(), m.d.signum())
                                } else {
                                        (m.a.signum(), m.c.signum())
                                };
                                let sign = asign;
                                let (pw, ph) = layer.physical_size();
                                self.page_move_dx = sign * ux;
                                self.page_move_dy = sign * uy;
                                self.page_move_span = if ux != 0 { i32::from(pw) } else { i32::from(ph) };
                        }
                        self.page_move_started = true;
                        self.page_move_start_us = now_us;
                        self.page_move_travel = 0;
                }
                let elapsed = now_us.saturating_sub(self.page_move_start_us);
                if elapsed >= u64::from(self.page_move_ms) * 1000 {
                        if !self.page_move_over {
                                display.thaw();
                        }
                        self.page_moving = false;
                        self.invalidate_all();
                        //   a rotation that arrived mid-transition runs now. Taken BEFORE the
                        // call: set_rotation checks page_moving, already false, so it proceeds
                        if let Some(r) = self.rotate_deferred.take() {
                                self.set_rotation(layer, r);
                        }
                        return Step::Finished;
                }
                let travel = ((self.page_move_span as i64 * elapsed as i64) / (i64::from(self.page_move_ms) * 1000)) as i32;
                let cover = !self.page_move_over && !self.page_move_region && self.page_move_back;
                if self.page_move_over {
                        //   no clear: the outgoing image IS the ground the incoming page
                        // slides in over
                        let Some(mut c) = layer.frame_begin_over(display, now_us) else { return Step::Waiting };
                        c.set_offset(self.page_move_dx * (self.page_move_span - travel), self.page_move_dy * (self.page_move_span - travel));
                        self.paint(&mut c, style);
                        drop(c);
                } else if cover {
                        //   COVER (close): the incoming page (the returning parent, rendered into the
                        // back buffer once at setup) slides back on over the static outgoing (the
                        // child, in the untouched front). Blit it at a SHRINKING offset along the
                        // descent axis: at full span it sits off-screen past the edge it left from,
                        // at zero it fully covers. The front keeps showing the child wherever the
                        // parent has not yet reached. Just a blit per step -- the incoming drawn once.
                        let Some(c) = layer.frame_begin_over(display, now_us) else { return Step::Waiting };
                        drop(c);
                        if let Some((front, incoming)) = display.frame_and_capture() {
                                let off = self.page_move_span - travel;
                                let mut c = layer.canvas(front);
                                c.blit_offset(incoming, self.page_move_dx * off, self.page_move_dy * off);
                        }
                } else if self.page_move_region {
                        //   REGION: one live buffer, no capture. The outgoing image is already in
                        // it; scroll the still-outgoing part off by the step's travel (in place),
                        // then paint the incoming into the strip that uncovered. The incoming is
                        // static at its final position, so only the new strip is drawn -- the same
                        // cheap-per-step shape as the capture reveal, without the second frame
                        let Some(mut c) = layer.frame_begin_over(display, now_us) else { return Step::Waiting };
                        let prev = self.page_move_travel;
                        if travel > prev {
                                let delta = travel - prev;
                                let (pw, ph) = layer.physical_size();
                                let (pw, ph) = (i32::from(pw), i32::from(ph));
                                //   the part of the buffer the outgoing still occupies at `prev`:
                                // everything the incoming has not reached, on the far side of the
                                // seam. shift_region leaves the near strip for the band paint below
                                let outgoing = if self.page_move_dx > 0 {
                                        Region::new(prev as u16, 0, (pw - 1) as u16, (ph - 1) as u16)
                                } else if self.page_move_dx < 0 {
                                        Region::new(0, 0, (pw - 1 - prev) as u16, (ph - 1) as u16)
                                } else if self.page_move_dy > 0 {
                                        Region::new(0, prev as u16, (pw - 1) as u16, (ph - 1) as u16)
                                } else {
                                        Region::new(0, 0, (pw - 1) as u16, (ph - 1 - prev) as u16)
                                };
                                c.shift_region(outgoing, self.page_move_dx * delta, self.page_move_dy * delta);
                                //   the incoming band, logical. The sign matches the setup's, so the
                                // band lands in the strip the shift just uncovered (region runs the
                                // open reveal only, so the sign is the descent axis's)
                                let vertical = self.page_move_descent.vertical();
                                let asign = self.page_move_descent.axis_sign();
                                let s = asign;
                                let (lw, lh) = layer.logical_size();
                                let (lw, lh) = (i32::from(lw), i32::from(lh));
                                let extent = if vertical { lh } else { lw };
                                let (b0, b1) = if s > 0 { (prev, travel - 1) } else { (extent - travel, extent - prev - 1) };
                                let band = if vertical { Rect::new(0, b0, lw - 1, b1) } else { Rect::new(b0, 0, b1, lh - 1) };
                                if let Some(root) = self.root {
                                        self.paint_clipped(&mut c, style, root, band);
                                }
                                c.clear_clip();
                                self.page_move_travel = travel;
                        }
                        drop(c);
                } else {
                        //   over, not cleared: the buffer holds the previous step's frame, and
                        // the incoming page is STATIC in this mode -- only the strip the
                        // outgoing image has uncovered since the last drawn step needs
                        // painting, then the capture is re-blitted at its new offset. A full
                        // tree paint here cost ~70 ms a step on the big panels: a 300 ms
                        // transition landed in three visible jumps, and every one starved the
                        // audio ring into a click
                        let Some(c) = layer.frame_begin_over(display, now_us) else { return Step::Waiting };
                        drop(c);
                        if let Some((front, captured)) = display.frame_and_capture() {
                                let mut c = layer.canvas(front);
                                //   the outgoing image's LOGICAL motion sign along the axis (== the
                                // setup's `sign`): new rows of the incoming appear at the low end
                                // when it moves positive, the high end otherwise. The vertical
                                // axis takes the descent's sign alone (reveal here is always the
                                // back half of a cover, so it does not turn on `back`); the
                                // horizontal one reverses with it.
                                let vertical = self.page_move_descent.vertical();
                                let asign = self.page_move_descent.axis_sign();
                                let s = if vertical { asign } else { asign * if self.page_move_back { -1 } else { 1 } };
                                let prev = self.page_move_travel;
                                if travel > prev {
                                        let (lw, lh) = layer.logical_size();
                                        let (lw, lh) = (i32::from(lw), i32::from(lh));
                                        let extent = if vertical { lh } else { lw };
                                        let (b0, b1) = if s > 0 { (prev, travel - 1) } else { (extent - travel, extent - prev - 1) };
                                        let band = if vertical {
                                                Rect::new(0, b0, lw - 1, b1)
                                        } else {
                                                Rect::new(b0, 0, b1, lh - 1)
                                        };
                                        if let Some(root) = self.root {
                                                self.paint_clipped(&mut c, style, root, band);
                                        }
                                        c.clear_clip();
                                }
                                c.blit_offset(captured, self.page_move_dx * travel, self.page_move_dy * travel);
                                self.page_move_travel = travel;
                        }
                }
                layer.invalidate_all();
                layer.frame_end(display);
                Step::Drew
        }
}
