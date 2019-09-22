use crate::config::Config;
use crate::config::TextStyle;
use crate::font::{FontConfiguration, GlyphInfo};
use crate::mux::renderable::Renderable;
use crate::mux::tab::Tab;
use crate::mux::window::WindowId as MuxWindowId;
use crate::mux::Mux;
use ::window::bitmaps::atlas::{Atlas, Sprite, SpriteSlice};
use ::window::bitmaps::{Image, ImageTexture};
use ::window::*;
use failure::Fallible;
use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use term::color::ColorPalette;
use term::{CursorPosition, Line, Underline};
use termwiz::color::RgbColor;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GlyphKey {
    font_idx: usize,
    glyph_pos: u32,
    style: TextStyle,
}

/// Caches a rendered glyph.
/// The image data may be None for whitespace glyphs.
struct CachedGlyph {
    has_color: bool,
    x_offset: f64,
    y_offset: f64,
    bearing_x: f64,
    bearing_y: f64,
    texture: Option<Sprite<ImageTexture>>,
    scale: f64,
}

pub struct TermWindow {
    window: Option<Window>,
    fonts: Rc<FontConfiguration>,
    _config: Arc<Config>,
    cell_size: Size,
    mux_window_id: MuxWindowId,
    descender: f64,
    descender_row: isize,
    descender_plus_one: isize,
    descender_plus_two: isize,
    strike_row: isize,
    glyph_cache: RefCell<HashMap<GlyphKey, Rc<CachedGlyph>>>,
    atlas: RefCell<Atlas<ImageTexture>>,
}

impl WindowCallbacks for TermWindow {
    fn created(&mut self, window: &Window) {
        self.window.replace(window.clone());
    }

    fn can_close(&mut self) -> bool {
        // self.host.close_current_tab();
        true
    }

    fn destroy(&mut self) {
        Connection::get().unwrap().terminate_message_loop();
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }

    fn resize(&mut self, dimensions: Dimensions) {
        let mux = Mux::get().unwrap();
        if let Some(window) = mux.get_window(self.mux_window_id) {
            let size = portable_pty::PtySize {
                rows: dimensions.pixel_height as u16 / self.cell_size.height as u16,
                cols: dimensions.pixel_width as u16 / self.cell_size.width as u16,
                pixel_height: dimensions.pixel_height as u16,
                pixel_width: dimensions.pixel_width as u16,
            };
            for tab in window.iter() {
                tab.resize(size).ok();
            }
        };
    }

    fn paint(&mut self, ctx: &mut dyn PaintContext) {
        let mux = Mux::get().unwrap();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => {
                ctx.clear(Color::rgb(0, 0, 0));
                return;
            }
        };
        self.paint_tab(&tab, ctx);
    }
}

impl Drop for TermWindow {
    fn drop(&mut self) {
        if Mux::get().unwrap().is_empty() {
            Connection::get().unwrap().terminate_message_loop();
        }
    }
}

impl TermWindow {
    pub fn new_window(
        config: &Arc<Config>,
        fontconfig: &Rc<FontConfiguration>,
        tab: &Rc<dyn Tab>,
        mux_window_id: MuxWindowId,
    ) -> Fallible<()> {
        log::error!(
            "TermWindow::new_window called with mux_window_id {}",
            mux_window_id
        );
        let (physical_rows, physical_cols) = tab.renderer().physical_dimensions();

        let metrics = fontconfig.default_font_metrics()?;
        let (cell_height, cell_width) = (
            metrics.cell_height.ceil() as usize,
            metrics.cell_width.ceil() as usize,
        );

        let width = cell_width * physical_cols;
        let height = cell_height * physical_rows;

        let surface = Rc::new(ImageTexture::new(4096, 4096));
        let atlas = RefCell::new(Atlas::new(&surface)?);

        let descender_row = (cell_height as f64 + metrics.descender) as isize;
        let descender_plus_one = (1 + descender_row).min(cell_height as isize - 1);
        let descender_plus_two = (2 + descender_row).min(cell_height as isize - 1);
        let strike_row = descender_row / 2;

        let window = Window::new_window(
            "wezterm",
            "wezterm",
            width,
            height,
            Box::new(Self {
                window: None,
                cell_size: Size::new(cell_width as isize, cell_height as isize),
                mux_window_id,
                _config: Arc::clone(config),
                fonts: Rc::clone(fontconfig),
                descender: metrics.descender,
                descender_row,
                descender_plus_one,
                descender_plus_two,
                strike_row,
                glyph_cache: RefCell::new(HashMap::new()),
                atlas,
            }),
        )?;

        let cloned_window = window.clone();

        Connection::get().unwrap().schedule_timer(
            std::time::Duration::from_millis(35),
            move || {
                let mux = Mux::get().unwrap();
                if let Some(tab) = mux.get_active_tab_for_window(mux_window_id) {
                    if tab.renderer().has_dirty_lines() {
                        cloned_window.invalidate();
                    }
                } else {
                    // TODO: destroy the window here
                }
            },
        );

        window.show();
        Ok(())
    }

    fn paint_tab(&mut self, tab: &Rc<dyn Tab>, ctx: &mut dyn PaintContext) {
        let palette = tab.palette();

        let mut term = tab.renderer();
        let cursor = term.get_cursor_position();

        {
            let dirty_lines = term.get_dirty_lines();

            for (line_idx, line, selrange) in dirty_lines {
                self.render_screen_line(ctx, line_idx, &line, selrange, &cursor, &*term, &palette)
                    .ok();
            }
        }

        term.clean_dirty_lines();
    }

    fn render_screen_line(
        &self,
        ctx: &mut dyn PaintContext,
        line_idx: usize,
        line: &Line,
        selection: Range<usize>,
        cursor: &CursorPosition,
        terminal: &dyn Renderable,
        palette: &ColorPalette,
    ) -> Fallible<()> {
        let (_num_rows, num_cols) = terminal.physical_dimensions();
        let current_highlight = terminal.current_highlight();

        // Break the line into clusters of cells with the same attributes
        let cell_clusters = line.cluster();
        let mut last_cell_idx = 0;
        for cluster in cell_clusters {
            let attrs = &cluster.attrs;
            let is_highlited_hyperlink = match (&attrs.hyperlink, &current_highlight) {
                (&Some(ref this), &Some(ref highlight)) => this == highlight,
                _ => false,
            };
            let style = self.fonts.match_style(attrs);

            let bg_color = palette.resolve_bg(attrs.background);
            let fg_color = match attrs.foreground {
                term::color::ColorAttribute::Default => {
                    if let Some(fg) = style.foreground {
                        fg
                    } else {
                        palette.resolve_fg(attrs.foreground)
                    }
                }
                term::color::ColorAttribute::PaletteIndex(idx) if idx < 8 => {
                    // For compatibility purposes, switch to a brighter version
                    // of one of the standard ANSI colors when Bold is enabled.
                    // This lifts black to dark grey.
                    let idx = if attrs.intensity() == term::Intensity::Bold {
                        idx + 8
                    } else {
                        idx
                    };
                    palette.resolve_fg(term::color::ColorAttribute::PaletteIndex(idx))
                }
                _ => palette.resolve_fg(attrs.foreground),
            };

            let (fg_color, bg_color) = {
                let mut fg = fg_color;
                let mut bg = bg_color;

                if attrs.reverse() {
                    std::mem::swap(&mut fg, &mut bg);
                }

                (fg, bg)
            };

            let glyph_color = rgbcolor_to_window_color(fg_color);
            let bg_color = rgbcolor_to_window_color(bg_color);

            // Shape the printable text from this cluster
            let glyph_info = {
                let font = self.fonts.cached_font(style)?;
                let mut font = font.borrow_mut();
                font.shape(&cluster.text)?
            };

            for info in &glyph_info {
                let cell_idx = cluster.byte_to_cell_idx[info.cluster as usize];
                let glyph = self.cached_glyph(info, style)?;

                let left = (glyph.x_offset + glyph.bearing_x) as f32;
                let top = ((self.cell_size.height as f64 + self.descender)
                    - (glyph.y_offset + glyph.bearing_y)) as f32;

                // underline and strikethrough
                // Figure out what we're going to draw for the underline.
                // If the current cell is part of the current URL highlight
                // then we want to show the underline.
                let underline = match (is_highlited_hyperlink, attrs.underline()) {
                    (true, Underline::None) => Underline::Single,
                    (_, underline) => underline,
                };

                // Iterate each cell that comprises this glyph.  There is usually
                // a single cell per glyph but combining characters, ligatures
                // and emoji can be 2 or more cells wide.
                for glyph_idx in 0..info.num_cells as usize {
                    let cell_idx = cell_idx + glyph_idx;

                    if cell_idx >= num_cols {
                        // terminal line data is wider than the window.
                        // This happens for example while live resizing the window
                        // smaller than the terminal.
                        break;
                    }
                    last_cell_idx = cell_idx;

                    let (glyph_color, bg_color) = self.compute_cell_fg_bg(
                        line_idx,
                        cell_idx,
                        cursor,
                        &selection,
                        glyph_color,
                        bg_color,
                        palette,
                    );

                    let cell_rect = Rect::new(
                        Point::new(
                            cell_idx as isize * self.cell_size.width,
                            self.cell_size.height * line_idx as isize,
                        ),
                        self.cell_size,
                    );
                    ctx.clear_rect(cell_rect, bg_color);

                    match underline {
                        Underline::Single => {
                            ctx.draw_line(
                                Point::new(
                                    cell_rect.origin.x,
                                    cell_rect.origin.y + self.descender_plus_one,
                                ),
                                Point::new(
                                    cell_rect.origin.x + self.cell_size.width,
                                    cell_rect.origin.y + self.descender_plus_one,
                                ),
                                glyph_color,
                                Operator::Over,
                            );
                        }
                        Underline::Double => {
                            ctx.draw_line(
                                Point::new(
                                    cell_rect.origin.x,
                                    cell_rect.origin.y + self.descender_row,
                                ),
                                Point::new(
                                    cell_rect.origin.x + self.cell_size.width,
                                    cell_rect.origin.y + self.descender_row,
                                ),
                                glyph_color,
                                Operator::Over,
                            );
                            ctx.draw_line(
                                Point::new(
                                    cell_rect.origin.x,
                                    cell_rect.origin.y + self.descender_plus_two,
                                ),
                                Point::new(
                                    cell_rect.origin.x + self.cell_size.width,
                                    cell_rect.origin.y + self.descender_plus_two,
                                ),
                                glyph_color,
                                Operator::Over,
                            );
                        }
                        Underline::None => {}
                    }
                    if attrs.strikethrough() {
                        ctx.draw_line(
                            Point::new(cell_rect.origin.x, cell_rect.origin.y + self.strike_row),
                            Point::new(
                                cell_rect.origin.x + self.cell_size.width,
                                cell_rect.origin.y + self.strike_row,
                            ),
                            glyph_color,
                            Operator::Over,
                        );
                    }

                    if let Some(ref texture) = glyph.texture {
                        ctx.draw_image(
                            Point::new(
                                (cell_rect.origin.x as f32 + left) as isize,
                                (cell_rect.origin.y as f32 + top) as isize,
                            ),
                            Some(texture.coords),
                            &*texture.texture.image.borrow(),
                            if glyph.has_color {
                                Operator::Source
                            } else {
                                Operator::MultiplyThenOver(glyph_color)
                            },
                        );
                        /* TODO: SpriteSlice for double-width
                        let slice = SpriteSlice {
                            cell_idx: glyph_idx,
                            num_cells: info.num_cells as usize,
                            cell_width: self.cell_width.ceil() as usize,
                            scale: glyph.scale as f32,
                            left_offset: left,
                        };

                        // How much of the width of this glyph we can use here
                        let slice_width = texture.slice_width(&slice);

                        let left = if glyph_idx == 0 { left } else { 0.0 };
                        let right = (slice_width as f32 + left) - self.cell_width as f32;

                        let bottom = (texture.coords.height as f32 * glyph.scale as f32 + top)
                            - self.cell_height as f32;

                        vert[V_TOP_LEFT].tex = texture.top_left(&slice);
                        vert[V_TOP_LEFT].adjust = TexturePoint::new(left, top);

                        vert[V_TOP_RIGHT].tex = texture.top_right(&slice);
                        vert[V_TOP_RIGHT].adjust = TexturePoint::new(right, top);

                        vert[V_BOT_LEFT].tex = texture.bottom_left(&slice);
                        vert[V_BOT_LEFT].adjust = TexturePoint::new(left, bottom);

                        vert[V_BOT_RIGHT].tex = texture.bottom_right(&slice);
                        vert[V_BOT_RIGHT].adjust = TexturePoint::new(right, bottom);

                        let has_color = if glyph.has_color { 1.0 } else { 0.0 };
                        vert[V_TOP_LEFT].has_color = has_color;
                        vert[V_TOP_RIGHT].has_color = has_color;
                        vert[V_BOT_LEFT].has_color = has_color;
                        vert[V_BOT_RIGHT].has_color = has_color;
                        */
                    }
                }
            }
        }

        // Clear any remaining cells to the right of the clusters we
        // found above, otherwise we leave artifacts behind.  The easiest
        // reproduction for the artifacts is to maximize the window and
        // open a vim split horizontally.  Backgrounding vim would leave
        // the right pane with its prior contents instead of showing the
        // cleared lines from the shell in the main screen.

        for cell_idx in last_cell_idx + 1..num_cols {
            // Even though we don't have a cell for these, they still
            // hold the cursor or the selection so we need to compute
            // the colors in the usual way.
            let (_glyph_color, bg_color) = self.compute_cell_fg_bg(
                line_idx,
                cell_idx,
                cursor,
                &selection,
                rgbcolor_to_window_color(palette.foreground),
                rgbcolor_to_window_color(palette.background),
                palette,
            );

            let cell_rect = Rect::new(
                Point::new(
                    cell_idx as isize * self.cell_size.width,
                    self.cell_size.height * line_idx as isize,
                ),
                self.cell_size,
            );
            ctx.clear_rect(cell_rect, bg_color);
        }

        Ok(())
    }

    fn compute_cell_fg_bg(
        &self,
        line_idx: usize,
        cell_idx: usize,
        cursor: &CursorPosition,
        selection: &Range<usize>,
        fg_color: Color,
        bg_color: Color,
        palette: &ColorPalette,
    ) -> (Color, Color) {
        let selected = selection.contains(&cell_idx);
        let is_cursor = line_idx as i64 == cursor.y && cursor.x == cell_idx;

        let (fg_color, bg_color) = match (selected, is_cursor) {
            // Normally, render the cell as configured
            (false, false) => (fg_color, bg_color),
            // Cursor cell overrides colors
            (_, true) => (
                rgbcolor_to_window_color(palette.cursor_fg),
                rgbcolor_to_window_color(palette.cursor_bg),
            ),
            // Selected text overrides colors
            (true, false) => (
                rgbcolor_to_window_color(palette.selection_fg),
                rgbcolor_to_window_color(palette.selection_bg),
            ),
        };

        (fg_color, bg_color)
    }

    /// Resolve a glyph from the cache, rendering the glyph on-demand if
    /// the cache doesn't already hold the desired glyph.
    fn cached_glyph(&self, info: &GlyphInfo, style: &TextStyle) -> Fallible<Rc<CachedGlyph>> {
        let key = GlyphKey {
            font_idx: info.font_idx,
            glyph_pos: info.glyph_pos,
            style: style.clone(),
        };

        let mut cache = self.glyph_cache.borrow_mut();

        if let Some(entry) = cache.get(&key) {
            return Ok(Rc::clone(entry));
        }

        let glyph = self.load_glyph(info, style)?;
        cache.insert(key, Rc::clone(&glyph));
        Ok(glyph)
    }

    /// Perform the load and render of a glyph
    fn load_glyph(&self, info: &GlyphInfo, style: &TextStyle) -> Fallible<Rc<CachedGlyph>> {
        let (has_color, glyph, cell_width, cell_height) = {
            let font = self.fonts.cached_font(style)?;
            let mut font = font.borrow_mut();
            let metrics = font.get_fallback(0)?.metrics();
            let active_font = font.get_fallback(info.font_idx)?;
            let has_color = active_font.has_color();
            let glyph = active_font.rasterize_glyph(info.glyph_pos)?;
            (has_color, glyph, metrics.cell_width, metrics.cell_height)
        };

        let scale = if (info.x_advance / f64::from(info.num_cells)).floor() > cell_width {
            f64::from(info.num_cells) * (cell_width / info.x_advance)
        } else if glyph.height as f64 > cell_height {
            cell_height / glyph.height as f64
        } else {
            1.0f64
        };
        #[cfg_attr(feature = "cargo-clippy", allow(clippy::float_cmp))]
        let (x_offset, y_offset) = if scale != 1.0 {
            (info.x_offset * scale, info.y_offset * scale)
        } else {
            (info.x_offset, info.y_offset)
        };

        let glyph = if glyph.width == 0 || glyph.height == 0 {
            // a whitespace glyph
            CachedGlyph {
                texture: None,
                has_color,
                x_offset,
                y_offset,
                bearing_x: 0.0,
                bearing_y: 0.0,
                scale,
            }
        } else {
            let raw_im = Image::with_bgra32(
                glyph.width as usize,
                glyph.height as usize,
                4 * glyph.width as usize,
                &glyph.data,
            );

            let tex = self.atlas.borrow_mut().allocate(&raw_im)?;

            let bearing_x = glyph.bearing_x * scale;
            let bearing_y = glyph.bearing_y * scale;

            CachedGlyph {
                texture: Some(tex),
                has_color,
                x_offset,
                y_offset,
                bearing_x,
                bearing_y,
                scale,
            }
        };

        Ok(Rc::new(glyph))
    }
}

fn rgbcolor_to_window_color(color: RgbColor) -> Color {
    Color::rgba(color.red, color.green, color.blue, 0xff)
}