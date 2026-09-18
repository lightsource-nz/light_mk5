use crate::{Ui, Theme};
use light_font::Font;

/// Which typographic role a piece of text plays. A [`Style`] binds a font per role, so a title
/// can be set in a different face or size from body text; today's UIs use one face for every
/// role ([`Fonts::uniform`]). Add a role here and every consumer keeps compiling -- [`Fonts`]
/// takes a font for each role, and `uniform` fills them all from one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontRole {
        /// Window titles and the title bar: subtitle and status indicator.
        Title,
        /// Everything else -- button labels, list rows, labels -- and the fallback role.
        Body,
}

impl FontRole {
        /// The number of roles: the width of a [`Style`]'s font set and the metric arrays.
        pub const COUNT: usize = 2;
        /// Every role, for iterating the set.
        pub const ALL: [FontRole; Self::COUNT] = [FontRole::Title, FontRole::Body];
}
/// The fonts a [`Style`] binds, one per [`FontRole`]. Borrowed, never owned: glyph data stays
/// transient -- handed to [`Ui::render`] each frame -- so a UI's font can be swapped freely and
/// the [`Ui`] carries no font lifetime (it keeps only the cell METRICS, taken at
/// [`Ui::set_style`]).
#[derive(Clone, Copy)]
pub struct Fonts<'f> {
        title: &'f Font<'f>,
        body: &'f Font<'f>,
}

impl<'f> Fonts<'f> {
        /// One face for every role: the single-font look every current UI uses.
        pub const fn uniform(font: &'f Font<'f>) -> Self {
                Self { title: font, body: font }
        }

        /// A distinct face -- or size -- per role.
        pub const fn new(title: &'f Font<'f>, body: &'f Font<'f>) -> Self {
                Self { title, body }
        }

        /// The font bound to `role`.
        pub const fn font(&self, role: FontRole) -> &'f Font<'f> {
                match role {
                        FontRole::Title => self.title,
                        FontRole::Body => self.body,
                }
        }
}
/// A complete look to render with: a [`Theme`] (colours and metrics) plus the [`Fonts`] for
/// every [`FontRole`]. Bound into a [`Ui`] with [`Ui::set_style`] and handed to
/// [`Ui::render`] each frame -- the same value for both, so the metrics a UI lays out with and
/// the glyphs it paints with can never come from different fonts.
#[derive(Clone, Copy)]
pub struct Style<'f> {
        pub theme: Theme,
        pub fonts: Fonts<'f>,
}

impl<'f> Style<'f> {
        pub const fn new(theme: Theme, fonts: Fonts<'f>) -> Self {
                Self { theme, fonts }
        }
}

impl<A: Copy, const N: usize> Ui<A, N> {
        /// The font's cell metrics, which layout and truncation need; fonts are fixed-pitch.
        /// Bind the look: the theme (its colours and metrics, via [`set_theme`](Self::set_theme))
        /// and each role's font cell metrics. The glyphs themselves stay transient -- pass the
        /// SAME [`Style`] to [`render`](Self::render). Re-lays-out, since a metric or the theme's
        /// radius may have moved.
        pub fn set_style(&mut self, style: &Style<'_>) {
                self.set_theme(style.theme);
                for role in FontRole::ALL {
                        let font = style.fonts.font(role);
                        self.cell_w[role as usize] = i32::from(font.cell_width());
                        self.cell_h[role as usize] = i32::from(font.cell_height());
                }
                self.relayout();
        }
        /// A role's font cell width, in logical pixels.
        pub(crate) fn role_cw(&self, role: FontRole) -> i32 {
                self.cell_w[role as usize]
        }
        /// A role's font cell height, in logical pixels.
        pub(crate) fn role_ch(&self, role: FontRole) -> i32 {
                self.cell_h[role as usize]
        }
}
