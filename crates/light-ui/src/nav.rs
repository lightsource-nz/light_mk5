use crate::{Ui, Page, Descent, Axis, Layout, Error};

impl<A: Copy, const N: usize> Ui<A, N> {
        pub fn page(&self) -> Option<&'static Page<A>> {
                self.page
        }
        fn show_page(&mut self, page: &'static Page<A>, return_page: Option<&'static Page<A>>, back: bool) -> Result<(), Error> {
                //   nothing to slide before the first page exists. A rotation in progress already
                // owns the back buffer and the frame, so a transfer during one simply snaps -- a
                // correct change beats two animations fighting over the same pixels. The image
                // itself is captured at the first render step, which is before anything of the
                // new tree has been drawn
                if self.root.is_some() && !self.rotating {
                        self.page_moving = true;
                        self.page_move_started = false;
                        self.page_move_back = back;
                        //   the CHILD of the pair decides the descent -- the page being entered
                        // going forward, the one being left coming back -- so a page's arrival
                        // and departure run the same axis and mirror. Precedence: the page's own
                        // override, then the tree default, then the layout seed that reproduces
                        // the historical flow. The seed follows the page's EFFECTIVE axis -- a
                        // horizontal page (a Row, or a Linear tree set horizontal) enters from
                        // the bottom, a vertical one from the right
                        let child = if back { self.page } else { Some(page) };
                        let axis = self.layout_axis;
                        let seed = || {
                                let horizontal = match child.map(|p| p.content.layout) {
                                        Some(Layout::Row { .. }) => true,
                                        Some(Layout::Linear { .. }) => axis == Axis::Horizontal,
                                        _ => false,
                                };
                                if horizontal { Descent::FromBottom } else { Descent::FromRight }
                        };
                        self.page_move_descent = child.and_then(|p| p.descend).or(self.default_descent).unwrap_or_else(seed);
                }
                // the old tree goes before the new one is built: only one page's widgets exist
                if let Some(root) = self.root {
                        self.destroy(root);
                }
                self.page = Some(page);
                self.return_page = return_page;
                self.build(None, page.content)?;
                self.invalidate_all();
                Ok(())
        }
        /// Build `page` in place of whatever is showing. Back from here goes to its parent. Safe
        /// to call from wherever an activation is handled -- the activating widget is gone
        /// afterwards, which is why activation returns before anything navigates.
        pub fn navigate(&mut self, page: &'static Page<A>) -> Result<(), Error> {
                self.show_page(page, None, false)
        }
        /// Rebuild `page` in place with NO transition -- for reflecting an in-place data change
        /// (a design editor re-materialising the page it just edited, a live-updated list). Unlike
        /// [`navigate`](Self::navigate) it does not slide: the point is to show the same page,
        /// changed, without a page-move animation.
        pub fn reload(&mut self, page: &'static Page<A>) -> Result<(), Error> {
                if let Some(root) = self.root {
                        self.destroy(root);
                }
                self.page = Some(page);
                self.return_page = None;
                self.build(None, page.content)?;
                self.invalidate_all();
                Ok(())
        }
        /// The same, but back from `page` goes to `return_page` -- for a cross-tree jump that
        /// should return to where it was reached from. The override lasts exactly one page.
        pub fn navigate_returning(&mut self, page: &'static Page<A>, return_page: &'static Page<A>) -> Result<(), Error> {
                self.show_page(page, Some(return_page), false)
        }
        /// Navigate to an explicit `page` with the BACK-direction transition -- the mirror of
        /// [`navigate`](Self::navigate). For a caller that keeps its own history (so the target is
        /// known) rather than relying on the parent link that [`navigate_back`](Self::navigate_back)
        /// follows.
        pub fn navigate_back_to(&mut self, page: &'static Page<A>) -> Result<(), Error> {
                self.show_page(page, None, true)
        }
        /// Go to the current page's return address if one was set, otherwise its parent. `false`,
        /// changing nothing, when there is nowhere to go -- a top-level page, or a tree built
        /// without pages -- so a caller can leave the gesture meaning nothing there.
        pub fn navigate_back(&mut self) -> bool {
                let Some(page) = self.page else { return false };
                let Some(target) = self.return_page.or(page.parent) else { return false };
                self.show_page(target, None, true).is_ok()
        }
        /// Point the whole tree's navigation flow one way: every child page opens in this
        /// direction unless it pins its own with [`Page::descend`]. `None` restores the
        /// historical layout-derived flow (a `Row` page rises, everything else slides left).
        /// Takes effect on the next navigation; an app may change it live, e.g. from an
        /// orientation sensor. See [`Descent`].
        pub fn set_default_descent(&mut self, descend: Option<Descent>) {
                self.default_descent = descend;
                self.default_descent_explicit = true;
        }
        /// The tree-wide default set by [`set_default_descent`](Self::set_default_descent),
        /// or `None` when navigation follows the layout-derived flow.
        pub fn default_descent(&self) -> Option<Descent> {
                self.default_descent
        }
}
