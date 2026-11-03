//! Renders the block tree with egui. Text is laid out into galleys that egui caches
//! between frames, so a static frame costs almost nothing.

use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use egui::emath::GuiRounding;
use egui::scroll_area::ScrollBarVisibility;
use egui::epaint::text::VariationCoords;
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{
    pos2, vec2, Align, Align2, Button, Color32, CornerRadius, CursorIcon, FontId, Galley, InnerResponse, Label, Layout,
    Margin, Pos2, Rangef, Rect, Response, RichText, Sense, Shape, Stroke, Ui, UiBuilder,
};
use egui::layers::ShapeIdx;
use pulldown_cmark::Alignment;

use crate::document::{Block, CodeBlock, Footnote, ImageRef, Inline, InlineItem, ListItem, QuoteKind, SpanStyle, Table};
use crate::fonts::FontSet;
use crate::highlight::Highlighter;
use crate::mermaid::{self, Diagram, Diagrams, Status};
use crate::search::SearchState;
use crate::theme::Palette;

pub enum Action {
    OpenLink(String),
    Status(String),
}

/// What the pointer is over this frame, for the right-click menu.
#[derive(Default)]
pub struct Hover {
    pub link: Option<String>,
    /// Resolved image URI (`file://…` or `http…`).
    pub image: Option<String>,
    pub code: Option<Arc<str>>,
}

pub struct RenderCtx<'a> {
    pub pal: &'a Palette,
    pub fonts: &'a FontSet,
    /// Body font size in points.
    pub size: f32,
    pub base_dir: Option<&'a Path>,
    pub gen: u64,
    pub hl: &'a mut Highlighter,
    pub diagrams: &'a mut Diagrams,
    pub search: Option<&'a SearchState>,
    pub hovered_link: Option<(usize, u32)>,
    pub new_hovered_link: Option<(usize, u32)>,
    pub actions: Vec<Action>,
    pub hover: Hover,
    /// Horizontal extent that content wider than the column may take, e.g. diagrams.
    pub wide: Rangef,
    list_depth: u8,
    text_color: Color32,
    scale: f32,
}

type LinkRanges = Vec<(Range<usize>, u32)>;
/// Backgrounds of inline code and search matches, relative to their galley.
type Backgrounds = Vec<(Rect, Color32)>;

/// Background of one character of a layout job: colour, margin, and how far its glyph is
/// drawn below where egui lays it out (negative: above).
#[derive(Clone, Copy, PartialEq)]
struct Fill {
    color: Color32,
    expand: f32,
    dy: f32,
}

impl Fill {
    const NONE: Fill = Fill { color: Color32::TRANSPARENT, expand: 0.0, dy: 0.0 };
}

impl<'a> RenderCtx<'a> {
    pub fn new(
        pal: &'a Palette,
        fonts: &'a FontSet,
        size: f32,
        base_dir: Option<&'a Path>,
        gen: u64,
        hl: &'a mut Highlighter,
        diagrams: &'a mut Diagrams,
        search: Option<&'a SearchState>,
        hovered_link: Option<(usize, u32)>,
    ) -> Self {
        Self {
            pal,
            fonts,
            size,
            base_dir,
            gen,
            hl,
            diagrams,
            search,
            hovered_link,
            new_hovered_link: None,
            actions: Vec::new(),
            hover: Hover::default(),
            wide: Rangef::NOTHING,
            list_depth: 0,
            text_color: pal.text,
            scale: 1.0,
        }
    }

    /// Vertical space between blocks.
    pub fn gap(&self) -> f32 {
        self.size * 0.9
    }

    fn fs(&self) -> f32 {
        self.size * self.scale
    }

    // ------------------------------------------------------------------
    // Inline text
    // ------------------------------------------------------------------

    fn format(&self, st: &SpanStyle, font_size: f32, color: Color32, lh: f32, hovered: bool, weight: Option<f32>) -> TextFormat {
        let code = st.code || st.math;
        // A static body font has no weight axis, so headings and table headers (which ask
        // for 600+) use the bold face instead.
        let strong = st.bold || (!self.fonts.variable_weight && weight.is_some_and(|w| w >= 600.0));
        let family = if code {
            self.fonts.inline_mono.clone()
        } else if st.italic {
            if strong {
                self.fonts.sans_bold_italic.clone().unwrap_or_else(|| self.fonts.sans_italic.clone())
            } else {
                self.fonts.sans_italic.clone()
            }
        } else if strong {
            self.fonts.sans_bold.clone().unwrap_or_else(|| self.fonts.sans.clone())
        } else {
            self.fonts.sans.clone()
        };
        let mut size = font_size;
        if code {
            size *= crate::fonts::CODE_SCALE;
        }
        if st.sup || st.sub || st.footnote {
            size *= 0.72;
        }
        let mut f = TextFormat {
            font_id: FontId::new(size, family),
            color,
            line_height: Some(font_size * lh),
            ..Default::default()
        };
        if st.dim {
            f.color = self.pal.text_dim;
        }
        if !code && self.fonts.variable_weight {
            let w = self.fonts.weight(weight, st.bold);
            if w != 400.0 {
                f.coords = VariationCoords::new([(b"wght", w)]);
            }
        }
        if st.italic && !code && self.fonts.sans_italic == self.fonts.sans {
            f.italics = true;
        }
        if st.code && !st.math {
            f.background = self.pal.inline_code_bg;
            f.expand_bg = 2.0;
            f.color = self.pal.inline_code_text;
        }
        if st.math {
            f.color = self.pal.math;
        }
        if st.strike {
            f.strikethrough = Stroke::new(1.0, f.color);
        }
        if st.link.is_some() {
            f.color = self.pal.link;
            if hovered {
                f.underline = Stroke::new(1.0, self.pal.link);
            }
        }
        if st.footnote {
            f.color = self.pal.link;
        }
        // egui centres text that has its own line height and ignores `valign` for it, so
        // raised and lowered text takes the line height of the text around it instead.
        if st.footnote || st.sup {
            f.line_height = None;
            f.valign = Align::TOP;
        } else if st.sub {
            f.line_height = None;
            f.valign = Align::BOTTOM;
        }
        f
    }

    #[allow(clippy::too_many_arguments)]
    fn layout_inline(
        &self,
        ui: &Ui,
        inline: &Inline,
        font_size: f32,
        color: Color32,
        wrap_width: f32,
        halign: Align,
        weight: Option<f32>,
        lh: f32,
    ) -> (Arc<Galley>, LinkRanges, Backgrounds) {
        let mut job = LayoutJob {
            wrap: TextWrapping { max_width: wrap_width, ..Default::default() },
            halign,
            break_on_newline: true,
            ..Default::default()
        };
        let mut links = Vec::new();
        let matches = self.search.and_then(|s| s.by_leaf.get(&inline.id));
        let current = self.search.map(|s| s.current);
        for span in &inline.spans {
            let hovered = span
                .style
                .link
                .is_some_and(|l| self.hovered_link == Some((inline.id, l)));
            let fmt = self.format(&span.style, font_size, color, lh, hovered, weight);
            let start = job.text.len();
            match matches {
                Some(ms) => append_highlighted(&mut job, &span.text, start, fmt, ms, current, self.pal),
                None => job.append(&span.text, 0.0, fmt),
            }
            if let Some(l) = span.style.link {
                links.push((start..job.text.len(), l));
            }
        }
        // egui draws a text background from the top of the text down by the whole line
        // height, which leaves it half a line low now that it centres text in that height,
        // so the backgrounds are taken out of the job and drawn around the text instead.
        // Inline code may be drawn lower or higher than egui lays it out (fonts.rs, Windows
        // only), and its background goes with it.
        let mut fills = Vec::new();
        if job.sections.iter().any(|s| s.format.background != Color32::TRANSPARENT) {
            for s in &mut job.sections {
                let dy = if self.fonts.inline_code_shift != 0.0 && s.format.font_id.family == self.fonts.inline_mono {
                    (s.format.font_id.size * self.fonts.inline_code_shift).round_ui()
                } else {
                    0.0
                };
                let fill = Fill { color: std::mem::replace(&mut s.format.background, Color32::TRANSPARENT), expand: s.format.expand_bg, dy };
                fills.extend(std::iter::repeat_n(fill, job.text[s.byte_range.start.0..s.byte_range.end.0].chars().count()));
            }
        }
        let galley = ui.painter().layout_job(job);
        let backgrounds = text_backgrounds(&galley, &fills);
        (galley, links, backgrounds)
    }

    fn show_inline(&mut self, ui: &mut Ui, inline: &Inline, font_size: f32, color: Color32, weight: Option<f32>, lh: f32) -> Response {
        let wrap = ui.available_width().max(10.0);
        let (galley, links, backgrounds) = self.layout_inline(ui, inline, font_size, color, wrap, Align::LEFT, weight, lh);
        let sense = if links.is_empty() { Sense::hover() } else { Sense::click() };
        let slot = (!backgrounds.is_empty()).then(|| ui.painter().add(Shape::Noop));
        let resp = ui.add(Label::new(galley.clone()).sense(sense).selectable(true));
        if let Some(slot) = slot {
            paint_backgrounds(ui, slot, resp.rect.min, &backgrounds);
        }
        if !links.is_empty() {
            self.handle_links(ui, &resp, &galley, &links, inline);
        }
        resp
    }

    fn handle_links(&mut self, ui: &Ui, resp: &Response, galley: &Arc<Galley>, links: &LinkRanges, inline: &Inline) {
        let Some(pos) = resp.hover_pos() else { return };
        let rel = pos - resp.rect.min;
        let rel_pos = pos2(rel.x, rel.y);
        if !galley.rows.iter().any(|r| r.rect().expand(1.0).contains(rel_pos)) {
            return;
        }
        let cursor = galley.cursor_from_pos(rel);
        let char_idx = cursor.index.0;
        let text = galley.text();
        let byte_idx = text.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(text.len());
        // A click at the very end of a link should still count; use the char before when at a boundary.
        let hit = links
            .iter()
            .find(|(r, _)| r.contains(&byte_idx))
            .or_else(|| links.iter().find(|(r, _)| byte_idx > 0 && r.contains(&(byte_idx - 1)) && byte_idx == r.end));
        if let Some((_, li)) = hit {
            let Some(link) = inline.links.get(*li as usize) else { return };
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            self.new_hovered_link = Some((inline.id, *li));
            self.hover.link = Some(link.url.clone());
            let tip = if link.title.is_empty() { link.url.clone() } else { format!("{}\n{}", link.title, link.url) };
            resp.clone().on_hover_text(tip);
            if resp.clicked() {
                self.actions.push(Action::OpenLink(link.url.clone()));
            }
        }
    }

    // ------------------------------------------------------------------
    // Blocks
    // ------------------------------------------------------------------

    pub fn show_blocks(&mut self, ui: &mut Ui, blocks: &[Block], tight: bool) {
        let gap = if tight { self.fs() * 0.25 } else { self.gap() * self.scale };
        for (i, b) in blocks.iter().enumerate() {
            if i > 0 {
                ui.add_space(gap);
            }
            self.show_block(ui, b);
        }
    }

    pub fn show_block(&mut self, ui: &mut Ui, block: &Block) {
        match block {
            Block::Heading { level, inline } => self.show_heading(ui, *level, inline),
            Block::Paragraph(items) => self.show_paragraph(ui, items),
            Block::Code(cb) => self.show_code(ui, cb),
            Block::Quote { kind, blocks } => self.show_quote(ui, *kind, blocks),
            Block::List { start, items } => self.show_list(ui, *start, items),
            Block::Table(t) => self.show_table(ui, t),
            Block::Rule => self.show_rule(ui),
            Block::Image(img) => self.show_image(ui, img, None),
            Block::Math(m) => self.show_math(ui, m),
            Block::Metadata(m) => self.show_metadata(ui, m),
            Block::DefList(entries) => self.show_deflist(ui, entries),
            Block::Footnotes(fns) => self.show_footnotes(ui, fns),
        }
    }

    fn show_heading(&mut self, ui: &mut Ui, level: u8, inline: &Inline) {
        let (scale, weight) = match level {
            1 => (2.0, 700.0),
            2 => (1.55, 650.0),
            3 => (1.28, 620.0),
            4 => (1.1, 600.0),
            5 => (1.0, 600.0),
            _ => (0.92, 600.0),
        };
        let size = self.fs() * scale;
        ui.add_space(self.fs() * if level <= 2 { 0.9 } else { 0.5 });
        let color = if level >= 6 { self.pal.text_dim } else { self.pal.heading };
        self.show_inline(ui, inline, size, color, Some(weight), 1.25);
        if level <= 2 {
            ui.add_space(self.fs() * 0.35);
            let r = ui.available_rect_before_wrap();
            ui.painter().hline(r.x_range(), r.top(), Stroke::new(1.0, self.pal.rule));
            ui.allocate_space(vec2(r.width(), 1.0));
        }
    }

    fn show_paragraph(&mut self, ui: &mut Ui, items: &[InlineItem]) {
        let fs = self.fs();
        if let [InlineItem::Text(t)] = items {
            self.show_inline(ui, t, fs, self.text_color, None, 1.55);
            return;
        }
        // Mixed text and inline images (badges etc.): flow them horizontally.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(fs * 0.35, fs * 0.35);
            for item in items {
                match item {
                    InlineItem::Text(t) => {
                        if t.text.trim().is_empty() {
                            continue;
                        }
                        self.show_inline(ui, t, fs, self.text_color, None, 1.55);
                    }
                    InlineItem::Image(img) => self.show_image(ui, img, Some(fs * 1.35)),
                }
            }
        });
    }

    fn resolve_image(&self, url: &str) -> Option<String> {
        let url = url.trim();
        if url.is_empty() || url.starts_with("data:") {
            return None;
        }
        if url.starts_with("http://") || url.starts_with("https://") {
            return Some(url.to_owned());
        }
        // Windows paths need their own URI form for egui_extras' file loader, and network
        // paths are refused.
        #[cfg(windows)]
        return crate::links::local_image_uri(url, self.base_dir);
        #[cfg(not(windows))]
        {
            if url.starts_with("file://") {
                return Some(url.to_owned());
            }
            let path_part = url.split(['#', '?']).next().unwrap_or(url);
            let path = crate::document::percent_decode(path_part);
            let p = Path::new(&path);
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                self.base_dir?.join(p)
            };
            Some(format!("file://{}", abs.display()))
        }
    }

    fn show_image(&mut self, ui: &mut Ui, img: &ImageRef, inline_max_h: Option<f32>) {
        let fs = self.fs();
        let alt = if img.alt.is_empty() { img.url.clone() } else { img.alt.clone() };
        let Some(uri) = self.resolve_image(&img.url) else {
            ui.label(RichText::new(format!("⊠ {alt}")).italics().color(self.pal.text_dim).size(fs * 0.9));
            return;
        };
        let avail = ui.available_width().max(16.0);
        let mut image = egui::Image::from_uri(uri.clone())
            .fit_to_original_size(1.0)
            .max_width(avail)
            .show_loading_spinner(false)
            .corner_radius(CornerRadius::same(4))
            .alt_text(alt.clone());
        if let Some(h) = inline_max_h {
            image = image.max_height(h);
        }
        match image.load_for_size(ui.ctx(), ui.available_size()) {
            Err(err) => {
                log::debug!("image {}: {err}", img.url);
                ui.label(RichText::new(format!("⊠ {alt}")).italics().color(self.pal.text_dim).size(fs * 0.9));
            }
            Ok(_) => {
                let resp = ui.add(image);
                if ui.rect_contains_pointer(resp.rect) {
                    self.hover.image = Some(uri);
                }
                if !img.title.is_empty() {
                    resp.on_hover_text(&img.title);
                }
            }
        }
    }

    fn show_code(&mut self, ui: &mut Ui, cb: &CodeBlock) {
        let mut diagram_error = None;
        if mermaid::is_mermaid(cb.lang.as_deref()) {
            match self.diagrams.status(self.gen, &cb.text, self.pal.dark) {
                Status::Ready(d) => return self.show_diagram(ui, cb, d),
                Status::Pending => {
                    let fs = self.fs();
                    ui.label(RichText::new("Drawing diagram…").italics().color(self.pal.text_dim).size(fs * 0.9));
                    return;
                }
                // Show the source, and why it is not a diagram.
                Status::Failed(err) => diagram_error = Some(err),
            }
        }
        let fs = self.fs();
        let font_size = fs * 0.86;
        let font = FontId::new(font_size, self.fonts.mono.clone());
        let hl = self.hl.get(self.gen, cb.id, self.pal.dark, &cb.text, cb.lang.as_deref());

        let base = TextFormat {
            font_id: font,
            color: self.pal.code_text,
            line_height: Some(font_size * 1.5),
            ..Default::default()
        };
        let mut sections: Vec<(Range<usize>, TextFormat)> = Vec::new();
        match hl {
            Some(h) if !h.spans.is_empty() => {
                let mut pos = 0;
                for s in &h.spans {
                    if s.start > pos {
                        sections.push((pos..s.start, base.clone()));
                    }
                    let mut f = base.clone();
                    f.color = s.color;
                    f.italics = s.italic;
                    sections.push((s.start..s.end, f));
                    pos = s.end;
                }
                if pos < cb.text.len() {
                    sections.push((pos..cb.text.len(), base.clone()));
                }
            }
            _ => sections.push((0..cb.text.len(), base.clone())),
        }
        if let Some(ms) = self.search.and_then(|s| s.by_leaf.get(&cb.id)) {
            let current = self.search.map(|s| s.current);
            sections = apply_search(sections, ms, current, self.pal);
        }
        let mut job = LayoutJob {
            wrap: TextWrapping { max_width: f32::INFINITY, ..Default::default() },
            break_on_newline: true,
            ..Default::default()
        };
        for (r, f) in sections {
            job.append(&cb.text[r], 0.0, f);
        }
        let galley = ui.painter().layout_job(job);

        let pad = (fs * 0.75).round();
        let has_lang = cb.lang.as_deref().is_some_and(|l| !l.is_empty());
        let band = if has_lang { (fs * 0.55).round() } else { 0.0 };
        let frame = egui::Frame::new()
            .fill(self.pal.code_bg)
            .stroke(Stroke::new(1.0, self.pal.code_border))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(Margin {
                left: pad as i8,
                right: pad as i8,
                top: (pad * 0.8 + band) as i8,
                bottom: (pad * 0.8) as i8,
            });
        let ir = frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::horizontal()
                .id_salt(("code", cb.id))
                .scroll_bar_visibility(ScrollBarVisibility::VisibleWhenNeeded)
                .show(ui, |ui| {
                    ui.add(Label::new(galley).selectable(true));
                });
        });
        let rect = ir.response.rect;
        let hovered = ui.rect_contains_pointer(rect);
        let corner = rect.right_top() + vec2(-pad * 0.6, pad * 0.35);
        if hovered {
            self.hover.code = Some(cb.text.clone());
            self.copy_button(ui, corner, &cb.text, "Copied code block to clipboard");
        } else if has_lang {
            ui.painter().text(
                corner,
                Align2::RIGHT_TOP,
                cb.lang.as_deref().unwrap_or(""),
                FontId::new(fs * 0.72, self.fonts.sans.clone()),
                self.pal.text_dim,
            );
        }
        if let Some(err) = diagram_error {
            ui.add_space(fs * 0.3);
            ui.label(RichText::new(format!("Diagram not drawn: {err}")).color(self.pal.warning).size(fs * 0.8));
        }
    }

    /// The "copy" button at the top right corner of a hovered code block or diagram.
    fn copy_button(&mut self, ui: &mut Ui, corner: Pos2, text: &Arc<str>, status: &str) {
        let fs = self.fs();
        let brect = Rect::from_min_max(corner + vec2(-fs * 3.2, 0.0), corner + vec2(0.0, fs * 1.25));
        let b = Button::new(RichText::new("copy").size(fs * 0.72)).small();
        // `place`, not `put`: `put` would move the layout cursor back up to the button,
        // so every block after a hovered code block would be laid out on top of it.
        if ui.place(brect, b).clicked() {
            ui.ctx().copy_text(text.to_string());
            self.actions.push(Action::Status(status.into()));
        }
    }

    /// A Mermaid block as its diagram, centred on the column.
    fn show_diagram(&mut self, ui: &mut Ui, cb: &CodeBlock, d: Diagram) {
        let fs = self.fs();
        let ppp = ui.ctx().pixels_per_point();
        let column = ui.available_rect_before_wrap().x_range();
        // A diagram wider than the column spreads into the margins beside it.
        let room = Rangef::new(self.wide.min.min(column.min), self.wide.max.max(column.max));
        // Mermaid's 16 px labels come out a little smaller than body text. A diagram too wide
        // for the room is shrunk to fit, down to 40% of that size; wider still, it scrolls.
        let natural = fs * 0.9 / 16.0;
        let scale = (room.span() / d.size.x).clamp(natural * 0.4, natural);
        let size = (d.size * scale * ppp).round() / ppp;
        let max_side = ui.ctx().input(|i| i.max_texture_side) as f32;
        let px = mermaid::texture_size(size * ppp, max_side);

        let (row, _) = ui.allocate_exact_size(vec2(column.span(), size.y), Sense::hover());
        let width = size.x.min(room.span());
        // Not `clamp`: rounding can put `room.max - width` a hair below `room.min`, and clamp panics.
        let left = (column.center() - width / 2.0).min(room.max - width).max(room.min);
        let frame = Rect::from_x_y_ranges(left..=left + width, row.y_range());
        let draw = |this: &mut Self, ui: &mut Ui| {
            let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
            let rect = Rect::from_min_size(rect.min.round_to_pixels(ppp), size);
            let near = ui.clip_rect().expand2(vec2(0.0, ui.clip_rect().height()));
            if let Some(texture) = this.diagrams.texture(d, near.intersects(rect).then_some(px)) {
                let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
                ui.painter().image(texture, rect, uv, Color32::WHITE);
            }
        };
        // A child that the column's layout does not grow to include. Its id must not depend
        // on how many blocks above were laid out, or the sideways scroll would be lost.
        let mut child = ui.new_child(
            UiBuilder::new().id_salt(("diagram", cb.id)).max_rect(frame).layout(Layout::top_down(Align::Min)),
        );
        if size.x > width + 0.5 {
            egui::ScrollArea::horizontal()
                .scroll_bar_visibility(ScrollBarVisibility::VisibleWhenNeeded)
                .show(&mut child, |ui| draw(self, ui));
        } else {
            draw(self, &mut child);
        }
        let block = row.union(frame);
        if ui.rect_contains_pointer(block) {
            self.hover.code = Some(cb.text.clone());
            let corner = block.right_top() + vec2(-fs * 0.45, fs * 0.25);
            self.copy_button(ui, corner, &cb.text, "Copied diagram source to clipboard");
        }
    }

    fn indented<R>(&mut self, ui: &mut Ui, indent: f32, f: impl FnOnce(&mut Self, &mut Ui) -> R) -> InnerResponse<R> {
        let avail = ui.available_rect_before_wrap();
        let rect = Rect::from_min_max(pos2(avail.left() + indent, avail.top()), pos2(avail.right(), avail.bottom().max(avail.top() + 10.0)));
        ui.scope_builder(UiBuilder::new().max_rect(rect).layout(Layout::top_down(Align::Min)), |ui| f(self, ui))
    }

    fn show_quote(&mut self, ui: &mut Ui, kind: Option<QuoteKind>, blocks: &[Block]) {
        let fs = self.fs();
        let bar_w = (fs * 0.25).max(3.0);
        let indent = fs * 1.3;
        let start = ui.cursor().min;
        let (bar_color, title) = match kind {
            None => (self.pal.quote_bar, None),
            Some(QuoteKind::Note) => (self.pal.note, Some("Note")),
            Some(QuoteKind::Tip) => (self.pal.tip, Some("Tip")),
            Some(QuoteKind::Important) => (self.pal.important, Some("Important")),
            Some(QuoteKind::Warning) => (self.pal.warning, Some("Warning")),
            Some(QuoteKind::Caution) => (self.pal.caution, Some("Caution")),
        };
        let old_color = self.text_color;
        if kind.is_none() {
            self.text_color = self.pal.quote_text;
        }
        let ir = self.indented(ui, indent, |s, ui| {
            if let Some(t) = title {
                let mut job = LayoutJob::default();
                let family = if s.fonts.variable_weight {
                    s.fonts.sans.clone()
                } else {
                    s.fonts.sans_bold.clone().unwrap_or_else(|| s.fonts.sans.clone())
                };
                let mut fmt = TextFormat { font_id: FontId::new(fs, family), color: bar_color, line_height: Some(fs * 1.5), ..Default::default() };
                if s.fonts.variable_weight {
                    fmt.coords = VariationCoords::new([(b"wght", s.fonts.weight(Some(650.0), false))]);
                }
                job.append(t, 0.0, fmt);
                ui.add(Label::new(job).selectable(false));
                ui.add_space(fs * 0.2);
            }
            s.show_blocks(ui, blocks, false);
        });
        self.text_color = old_color;
        let bottom = ir.response.rect.bottom();
        ui.painter().rect_filled(
            Rect::from_min_max(pos2(start.x, start.y), pos2(start.x + bar_w, bottom)),
            CornerRadius::same(2),
            bar_color,
        );
    }

    fn show_list(&mut self, ui: &mut Ui, start: Option<u64>, items: &[ListItem]) {
        let fs = self.fs();
        let max_num = start.map(|s| s + items.len() as u64).unwrap_or(0);
        let digits = if max_num >= 100 { 3.0 } else if max_num >= 10 { 2.0 } else { 1.0 };
        let marker_w = if start.is_some() { fs * (1.1 + 0.6 * digits) } else { fs * 1.6 };
        let lh = fs * 1.55;
        self.list_depth += 1;
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                ui.add_space(if item.tight { fs * 0.25 } else { fs * 0.6 });
            }
            let origin = ui.cursor().min;
            let cy = origin.y + lh * 0.5;
            let painter = ui.painter().clone();
            match (item.checked, start) {
                (Some(checked), _) => {
                    let sz = fs * 0.95;
                    let r = Rect::from_center_size(pos2(origin.x + marker_w - fs * 0.5 - sz * 0.5, cy), vec2(sz, sz));
                    if checked {
                        painter.rect_filled(r, CornerRadius::same(3), self.pal.accent);
                        let s = Stroke::new((sz * 0.14).max(1.5), self.pal.bg);
                        painter.line_segment([pos2(r.left() + sz * 0.22, cy + sz * 0.02), pos2(r.left() + sz * 0.42, cy + sz * 0.24)], s);
                        painter.line_segment([pos2(r.left() + sz * 0.42, cy + sz * 0.24), pos2(r.right() - sz * 0.2, cy - sz * 0.26)], s);
                    } else {
                        painter.rect_stroke(r, CornerRadius::same(3), Stroke::new(1.5, self.pal.text_dim), egui::StrokeKind::Inside);
                    }
                }
                (None, Some(s)) => {
                    let n = s + i as u64;
                    painter.text(
                        pos2(origin.x + marker_w - fs * 0.5, cy),
                        Align2::RIGHT_CENTER,
                        format!("{n}."),
                        FontId::new(fs, self.fonts.sans.clone()),
                        self.pal.text_dim,
                    );
                }
                (None, None) => {
                    let c = pos2(origin.x + marker_w - fs * 0.85, cy);
                    let r = fs * 0.17;
                    match self.list_depth {
                        1 => {
                            painter.circle_filled(c, r, self.pal.text_dim);
                        }
                        2 => {
                            painter.circle_stroke(c, r - 0.5, Stroke::new(1.3, self.pal.text_dim));
                        }
                        _ => {
                            painter.rect_filled(Rect::from_center_size(c, vec2(r * 1.7, r * 1.7)), CornerRadius::ZERO, self.pal.text_dim);
                        }
                    }
                }
            }
            self.indented(ui, marker_w, |s, ui| s.show_blocks(ui, &item.blocks, item.tight));
        }
        self.list_depth -= 1;
    }

    fn show_table(&mut self, ui: &mut Ui, t: &Table) {
        let fs = self.fs();
        let ncols = t
            .aligns
            .len()
            .max(t.head.len())
            .max(t.rows.iter().map(|r| r.len()).max().unwrap_or(0));
        if ncols == 0 {
            return;
        }
        let pad_x = fs * 0.75;
        let pad_y = fs * 0.4;
        let avail = ui.available_width();
        let empty = Inline::default();
        let color = self.text_color;
        let cell_align = |c: usize| match t.aligns.get(c) {
            Some(Alignment::Center) => Align::Center,
            Some(Alignment::Right) => Align::Max,
            _ => Align::Min,
        };

        // Natural (unwrapped) column widths.
        let mut nat = vec![0.0f32; ncols];
        for (ri, row) in std::iter::once(&t.head).chain(t.rows.iter()).enumerate() {
            let weight = if ri == 0 { Some(650.0) } else { None };
            for c in 0..ncols {
                let cell = row.get(c).unwrap_or(&empty);
                let (g, _, _) = self.layout_inline(ui, cell, fs, color, f32::INFINITY, Align::LEFT, weight, 1.4);
                nat[c] = nat[c].max(g.size().x.ceil() + 2.0 * pad_x + 2.0);
            }
        }
        let widths = distribute_widths(&nat, avail, fs * 4.0);
        let table_w: f32 = widths.iter().sum();
        let left = ui.cursor().left();
        let top = ui.cursor().top();
        let mut y = top;
        let border = Stroke::new(1.0, self.pal.table_border);
        let painter = ui.painter().clone();

        for (ri, row) in std::iter::once(&t.head).chain(t.rows.iter()).enumerate() {
            let is_head = ri == 0;
            if is_head && t.head.is_empty() {
                continue;
            }
            let mut galleys = Vec::with_capacity(ncols);
            let mut row_h = 0.0f32;
            for c in 0..ncols {
                let cell = row.get(c).unwrap_or(&empty);
                let (g, links, backgrounds) = self.layout_inline(
                    ui,
                    cell,
                    fs,
                    color,
                    (widths[c] - 2.0 * pad_x).max(8.0),
                    Align::LEFT,
                    if is_head { Some(650.0) } else { None },
                    1.4,
                );
                row_h = row_h.max(g.size().y);
                galleys.push((g, links, backgrounds));
            }
            let row_h = row_h + 2.0 * pad_y;
            let row_rect = Rect::from_min_size(pos2(left, y), vec2(table_w, row_h));
            if is_head {
                painter.rect_filled(row_rect, CornerRadius::ZERO, self.pal.table_head_bg);
            } else if ri % 2 == 0 {
                painter.rect_filled(row_rect, CornerRadius::ZERO, self.pal.table_stripe);
            }
            let mut x = left;
            for c in 0..ncols {
                let cell_rect = Rect::from_min_size(pos2(x + pad_x, y + pad_y), vec2((widths[c] - 2.0 * pad_x).max(8.0), row_h - 2.0 * pad_y));
                let (g, links, backgrounds) = &galleys[c];
                let sense = if links.is_empty() { Sense::hover() } else { Sense::click() };
                let g2 = g.clone();
                let slot = (!backgrounds.is_empty()).then(|| ui.painter().add(Shape::Noop));
                let resp = ui
                    .scope_builder(UiBuilder::new().max_rect(cell_rect).layout(Layout::top_down(cell_align(c))), |ui| {
                        ui.add(Label::new(g2).sense(sense).selectable(true))
                    })
                    .inner;
                if let Some(slot) = slot {
                    paint_backgrounds(ui, slot, resp.rect.min, backgrounds);
                }
                if !links.is_empty() {
                    if let Some(cell) = row.get(c) {
                        self.handle_links(ui, &resp, g, links, cell);
                    }
                }
                x += widths[c];
            }
            let line = if is_head { Stroke::new(2.0, self.pal.table_border) } else { border };
            painter.hline(left..=left + table_w, y + row_h, line);
            y += row_h;
        }
        painter.hline(left..=left + table_w, top, border);
        ui.allocate_rect(Rect::from_min_max(pos2(left, top), pos2(left + table_w, y + 1.0)), Sense::hover());
    }

    fn show_rule(&mut self, ui: &mut Ui) {
        let fs = self.fs();
        ui.add_space(fs * 0.4);
        let r = ui.available_rect_before_wrap();
        ui.painter().hline(r.x_range(), r.top() + 1.0, Stroke::new(2.0, self.pal.rule));
        ui.allocate_space(vec2(r.width(), 2.0));
        ui.add_space(fs * 0.4);
    }

    fn show_math(&mut self, ui: &mut Ui, m: &str) {
        let fs = self.fs();
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(m).font(FontId::new(fs * 0.95, self.fonts.mono.clone())).color(self.pal.math));
        });
    }

    fn show_metadata(&mut self, ui: &mut Ui, raw: &str) {
        let fs = self.fs();
        egui::CollapsingHeader::new(RichText::new("Front matter").size(fs * 0.8).color(self.pal.text_dim))
            .id_salt(("meta", raw.len()))
            .default_open(false)
            .show(ui, |ui| {
                ui.label(RichText::new(raw).font(FontId::new(fs * 0.82, self.fonts.mono.clone())).color(self.pal.text_dim));
            });
    }

    fn show_deflist(&mut self, ui: &mut Ui, entries: &[(Inline, Vec<Block>)]) {
        let fs = self.fs();
        for (i, (term, defs)) in entries.iter().enumerate() {
            if i > 0 {
                ui.add_space(fs * 0.5);
            }
            if !term.spans.is_empty() {
                self.show_inline(ui, term, fs, self.pal.heading, Some(650.0), 1.5);
                ui.add_space(fs * 0.15);
            }
            self.indented(ui, fs * 1.6, |s, ui| s.show_blocks(ui, defs, true));
        }
    }

    fn show_footnotes(&mut self, ui: &mut Ui, fns: &[Footnote]) {
        let fs = self.fs();
        ui.add_space(fs);
        let r = ui.available_rect_before_wrap();
        ui.painter().hline(r.left()..=r.left() + r.width() * 0.3, r.top(), Stroke::new(1.0, self.pal.rule));
        ui.allocate_space(vec2(r.width(), 1.0));
        ui.add_space(fs * 0.6);
        let old = self.scale;
        self.scale *= 0.88;
        let fs2 = self.fs();
        for (i, f) in fns.iter().enumerate() {
            if i > 0 {
                ui.add_space(fs2 * 0.4);
            }
            let origin = ui.cursor().min;
            ui.painter().text(
                pos2(origin.x + fs2 * 1.6, origin.y + fs2 * 1.55 * 0.5),
                Align2::RIGHT_CENTER,
                format!("{}.", f.number),
                FontId::new(fs2 * 0.9, self.fonts.sans.clone()),
                self.pal.link,
            );
            self.indented(ui, fs2 * 2.1, |s, ui| s.show_blocks(ui, &f.blocks, true));
        }
        self.scale = old;
    }
}

// ----------------------------------------------------------------------
// helpers
// ----------------------------------------------------------------------

/// One rect per run of glyphs with the same fill (one per character of the job), as tall
/// as the font around the text and moved with the glyphs.
fn text_backgrounds(galley: &Galley, fills: &[Fill]) -> Backgrounds {
    let mut out = Vec::new();
    if fills.is_empty() {
        return out;
    }
    // egui lays out one glyph per character, so the n-th glyph is the n-th character.
    let mut first_char = 0;
    for placed in &galley.rows {
        let mut run: Option<(Rect, Fill)> = None;
        for (i, g) in placed.row.glyphs.iter().enumerate() {
            let fill = fills.get(first_char + i).copied().unwrap_or(Fill::NONE);
            let rect = Rect::from_min_size(placed.pos + vec2(g.pos.x, g.pos.y - g.font_ascent + fill.dy), vec2(g.advance_width, g.font_height));
            match &mut run {
                Some((r, f)) if *f == fill && r.y_range() == rect.y_range() => r.max.x = rect.max.x,
                _ => {
                    out.extend(run.take().map(|(r, f)| (r.expand(f.expand), f.color)));
                    if fill.color != Color32::TRANSPARENT {
                        run = Some((rect, fill));
                    }
                }
            }
        }
        out.extend(run.map(|(r, f)| (r.expand(f.expand), f.color)));
        first_char += placed.char_count_including_newline().0;
    }
    out
}

/// Fills `slot`, a shape added before the galley at `origin` was, with its backgrounds.
fn paint_backgrounds(ui: &Ui, slot: ShapeIdx, origin: Pos2, backgrounds: &Backgrounds) {
    let ppp = ui.pixels_per_point();
    let shapes = backgrounds
        .iter()
        .map(|(r, c)| Shape::rect_filled(r.translate(origin.to_vec2()).round_to_pixels(ppp), CornerRadius::ZERO, *c))
        .collect();
    ui.painter().set(slot, Shape::Vec(shapes));
}

fn append_highlighted(
    job: &mut LayoutJob,
    text: &str,
    off: usize,
    fmt: TextFormat,
    ms: &[(usize, Range<usize>)],
    current: Option<usize>,
    pal: &Palette,
) {
    let end = off + text.len();
    let mut pos = off;
    for (mi, r) in ms {
        if r.end <= pos || r.start >= end {
            continue;
        }
        let s = r.start.max(pos);
        let e = r.end.min(end);
        if s > pos {
            job.append(&text[pos - off..s - off], 0.0, fmt.clone());
        }
        let mut f = fmt.clone();
        if Some(*mi) == current {
            f.background = pal.search_current_bg;
            f.color = Color32::BLACK;
        } else {
            f.background = pal.search_bg;
            if !pal.dark {
                f.color = Color32::BLACK;
            }
        }
        f.expand_bg = 1.0;
        job.append(&text[s - off..e - off], 0.0, f);
        pos = e;
    }
    if pos < end {
        job.append(&text[pos - off..], 0.0, fmt);
    }
}

fn apply_search(
    sections: Vec<(Range<usize>, TextFormat)>,
    ms: &[(usize, Range<usize>)],
    current: Option<usize>,
    pal: &Palette,
) -> Vec<(Range<usize>, TextFormat)> {
    let mut out = Vec::with_capacity(sections.len() + ms.len() * 2);
    for (r, fmt) in sections {
        let mut pos = r.start;
        for (mi, m) in ms {
            if m.end <= pos || m.start >= r.end {
                continue;
            }
            let s = m.start.max(pos);
            let e = m.end.min(r.end);
            if s > pos {
                out.push((pos..s, fmt.clone()));
            }
            let mut f = fmt.clone();
            if Some(*mi) == current {
                f.background = pal.search_current_bg;
                f.color = Color32::BLACK;
            } else {
                f.background = pal.search_bg;
                if !pal.dark {
                    f.color = Color32::BLACK;
                }
            }
            out.push((s..e, f));
            pos = e;
        }
        if pos < r.end {
            out.push((pos..r.end, fmt));
        }
    }
    out
}

/// Shrink columns to fit `avail`, letting narrow columns keep their natural width.
fn distribute_widths(nat: &[f32], avail: f32, min_w: f32) -> Vec<f32> {
    let total: f32 = nat.iter().sum();
    if total <= avail {
        return nat.to_vec();
    }
    let n = nat.len();
    let mut widths = nat.to_vec();
    let mut fixed = vec![false; n];
    loop {
        let fixed_sum: f32 = (0..n).filter(|&i| fixed[i]).map(|i| widths[i]).sum();
        let flex: Vec<usize> = (0..n).filter(|&i| !fixed[i]).collect();
        if flex.is_empty() {
            break;
        }
        let remaining = (avail - fixed_sum).max(0.0);
        let flex_nat: f32 = flex.iter().map(|&i| nat[i]).sum::<f32>().max(1.0);
        let mut changed = false;
        for &i in &flex {
            let share = remaining * nat[i] / flex_nat;
            if nat[i] <= share {
                widths[i] = nat[i];
                fixed[i] = true;
                changed = true;
            }
        }
        if !changed {
            for &i in &flex {
                widths[i] = (remaining * nat[i] / flex_nat).max(min_w.min(nat[i]));
            }
            break;
        }
    }
    widths
}
