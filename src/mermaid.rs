//! Mermaid diagrams (```` ```mermaid ```` code blocks). On a worker thread, merman lays a
//! diagram out as SVG and usvg turns that into a tree, which is then rasterised at the size
//! it is shown, so diagrams stay sharp at any zoom. Results are cached by source text and
//! theme, so a reload only redraws the diagrams that changed.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{vec2, ColorImage, TextureHandle, TextureId, TextureOptions, Vec2};
use merman::svg::{
    DeterministicTextMeasurer, MeasurementProfileId, RootBackgroundPostprocessor, SvgEnvironment, SvgPipeline,
    TextMeasurementPolicy, TextMeasurementProfile, TextMeasurementProfileIdentity, TextStyle,
};
use merman::{Engine, MermaidConfig, OperationControl, RenderError, RenderOutput, RenderRequest, Renderer, SvgRequest};
use resvg::tiny_skia;
use resvg::usvg::{self, fontdb};

/// Margin around a diagram, in diagram pixels, so glyphs that overhang the SVG's view box
/// (the actor names under a sequence diagram) are not cut off.
const PAD: f32 = 6.0;
/// Textures of diagrams that are not on screen are dropped above this total, and drawn
/// again when scrolled back to.
const TEXTURE_BUDGET: usize = 256 << 20;
/// Most pixels one diagram is drawn with: 128 MiB as RGBA, and the drawing is held twice
/// (tiny-skia's pixmap and egui's image) before it is uploaded. The budget above only drops
/// diagrams that are off screen, so it cannot bound a single one on screen.
const MAX_PIXELS: usize = 32 << 20;
/// A diagram that takes longer than this to lay out is given up on, so that the ones after
/// it are still drawn.
const LAYOUT_DEADLINE: Duration = Duration::from_secs(10);

/// Whether a fenced code block's info string asks for a Mermaid diagram.
pub fn is_mermaid(lang: Option<&str>) -> bool {
    lang.and_then(|l| l.split(|c: char| c.is_whitespace() || c == '{').next())
        .is_some_and(|l| l.eq_ignore_ascii_case("mermaid"))
}

/// The texture size for a diagram shown `px` physical pixels large. A diagram can be far
/// larger than the screen (a big graph, or one whose spacing is set very wide), and only
/// its visible part is ever on screen, so above the GPU's `max_side` or `MAX_PIXELS` in
/// all it is drawn smaller, keeping its shape, and stretched.
pub fn texture_size(px: Vec2, max_side: f32) -> [usize; 2] {
    let px = px.round();
    let fit = (max_side / px.max_elem()).min((MAX_PIXELS as f32 / (px.x * px.y)).sqrt());
    // Rounded down when shrunk, so the result never goes over either limit.
    let px = if fit < 1.0 { (px * fit).floor() } else { px };
    [px.x.max(1.0) as usize, px.y.max(1.0) as usize]
}

type Key = (u64, bool);

/// A laid-out diagram, for [`Diagrams::texture`].
#[derive(Clone, Copy)]
pub struct Diagram {
    key: Key,
    /// In diagram pixels, margin included.
    pub size: Vec2,
}

pub enum Status {
    Pending,
    Failed(String),
    Ready(Diagram),
}

enum State {
    Pending,
    Failed(String),
    Ready { tree: Arc<usvg::Tree>, texture: Option<TextureHandle>, rastering: bool },
}

struct Entry {
    state: State,
    /// Last document generation that showed it.
    gen: u64,
    /// Generation of the last layout job sent for it.
    sent: u64,
    /// Last frame it was drawn in.
    drawn: u64,
}

enum Job {
    /// `gen` is the document generation it was asked for in.
    Layout { key: Key, gen: u64, source: Arc<str> },
    Raster { key: Key, tree: Arc<usvg::Tree>, size: [usize; 2] },
}

enum Done {
    Layout { key: Key, result: Result<Arc<usvg::Tree>, String> },
    Raster { key: Key, image: ColorImage },
    /// A layout that was not run because another document was opened after it was asked
    /// for. The source comes back so that it can be sent again.
    Skipped { key: Key, gen: u64, source: Arc<str> },
}

/// What [`Diagrams::poll`] does with a skipped layout.
#[derive(Debug, PartialEq)]
enum OnSkip {
    /// The current document shows the diagram: lay it out after all.
    Resend,
    /// Only a document no longer shown did: forget it, so that it is not waited for.
    Remove,
    /// The entry has moved on (laid out, or asked for again since).
    Ignore,
}

/// The fate of a layout asked for in generation `skipped` and skipped, for an entry that is
/// `pending`, was last shown in generation `gen` and last sent in `sent`, while `current` is
/// the generation shown now.
fn on_skipped(pending: bool, gen: u64, sent: u64, skipped: u64, current: u64) -> OnSkip {
    if !pending || sent != skipped {
        OnSkip::Ignore
    } else if gen >= current {
        OnSkip::Resend
    } else {
        OnSkip::Remove
    }
}

pub struct Diagrams {
    ctx: egui::Context,
    tx: mpsc::Sender<Job>,
    rx: mpsc::Receiver<Done>,
    entries: HashMap<Key, Entry>,
    frame: u64,
    /// The document generation shown now, shared with the worker so that it can skip the
    /// layouts of documents no longer shown.
    current: Arc<AtomicU64>,
}

impl Diagrams {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, worker_rx) = mpsc::channel();
        let (worker_tx, rx) = mpsc::channel();
        let worker_ctx = ctx.clone();
        let current = Arc::new(AtomicU64::new(0));
        let worker_current = current.clone();
        std::thread::Builder::new()
            .name("mermaid".into())
            .spawn(move || worker(worker_rx, worker_tx, worker_ctx, worker_current))
            .expect("spawn mermaid thread");
        Self { ctx, tx, rx, entries: HashMap::new(), frame: 0, current }
    }

    /// Takes in finished work. Call once per frame, before drawing.
    pub fn poll(&mut self) {
        self.frame += 1;
        while let Ok(done) = self.rx.try_recv() {
            match done {
                Done::Skipped { key, gen, source } => {
                    let current = self.current.load(Ordering::Relaxed);
                    let Some(e) = self.entries.get_mut(&key) else { continue };
                    // A diagram the current document still shows (a reload that left it
                    // unchanged) is sent again here: status() only sends one it has no entry
                    // for, and it must still be laid out for the document's height to be known.
                    match on_skipped(matches!(e.state, State::Pending), e.gen, e.sent, gen, current) {
                        OnSkip::Resend => {
                            e.sent = current;
                            let _ = self.tx.send(Job::Layout { key, gen: current, source });
                        }
                        OnSkip::Remove => {
                            self.entries.remove(&key);
                        }
                        OnSkip::Ignore => {}
                    }
                }
                Done::Layout { key, result } => {
                    if let Some(e) = self.entries.get_mut(&key) {
                        e.state = match result {
                            Ok(tree) => State::Ready { tree, texture: None, rastering: false },
                            Err(err) => State::Failed(err),
                        };
                    }
                }
                Done::Raster { key, image } => {
                    if let Some(Entry { state: State::Ready { texture, rastering, .. }, .. }) = self.entries.get_mut(&key) {
                        *rastering = false;
                        match texture {
                            Some(t) => t.set(image, TextureOptions::LINEAR),
                            None => *texture = Some(self.ctx.load_texture("mermaid", image, TextureOptions::LINEAR)),
                        }
                    }
                }
            }
        }
        self.evict();
    }

    /// Drops the textures of diagrams not drawn in the last frame, least recently drawn
    /// first, while the total is over budget.
    fn evict(&mut self) {
        let bytes = |e: &Entry| match &e.state {
            State::Ready { texture: Some(t), .. } => t.byte_size(),
            _ => 0,
        };
        let mut total: usize = self.entries.values().map(bytes).sum();
        if total <= TEXTURE_BUDGET {
            return;
        }
        let mut idle: Vec<(u64, Key)> =
            self.entries.iter().filter(|(_, e)| e.drawn + 1 < self.frame && bytes(e) > 0).map(|(k, e)| (e.drawn, *k)).collect();
        idle.sort_unstable();
        for (_, key) in idle {
            if total <= TEXTURE_BUDGET {
                break;
            }
            if let Some(Entry { state: State::Ready { texture, .. }, .. }) = self.entries.get_mut(&key) {
                if let Some(t) = texture.take() {
                    total -= t.byte_size();
                }
            }
        }
    }

    /// The diagram for `source`; lays it out in the background the first time.
    pub fn status(&mut self, gen: u64, source: &Arc<str>, dark: bool) -> Status {
        let mut h = DefaultHasher::new();
        source.hash(&mut h);
        let key = (h.finish(), dark);
        let entry = self.entries.entry(key).or_insert_with(|| {
            let _ = self.tx.send(Job::Layout { key, gen, source: source.clone() });
            Entry { state: State::Pending, gen, sent: gen, drawn: 0 }
        });
        entry.gen = gen;
        match &entry.state {
            State::Pending => Status::Pending,
            State::Failed(err) => Status::Failed(err.clone()),
            State::Ready { tree, .. } => {
                let size = vec2(tree.size().width(), tree.size().height()) + Vec2::splat(2.0 * PAD);
                Status::Ready(Diagram { key, size })
            }
        }
    }

    /// The texture to draw `d` with. `px` is the size wanted in physical pixels; until that
    /// is ready the texture has another size (or there is none yet), so the caller draws it
    /// into its rect. `None` means the diagram is far from view and nothing new is drawn.
    pub fn texture(&mut self, d: Diagram, px: Option<[usize; 2]>) -> Option<TextureId> {
        let entry = self.entries.get_mut(&d.key)?;
        let State::Ready { tree, texture, rastering } = &mut entry.state else { return None };
        if let Some(px) = px {
            entry.drawn = self.frame;
            if !*rastering && texture.as_ref().is_none_or(|t| t.size() != px) {
                *rastering = true;
                let _ = self.tx.send(Job::Raster { key: d.key, tree: tree.clone(), size: px });
            }
        }
        texture.as_ref().map(TextureHandle::id)
    }

    /// Whether any diagram is still being laid out or drawn, so a screenshot would miss it.
    /// Diagrams that failed are done; those laid out but far from view are never drawn,
    /// so only the drawing asked for counts.
    pub fn busy(&self) -> bool {
        self.entries.values().any(|e| matches!(e.state, State::Pending | State::Ready { rastering: true, .. }))
    }

    /// Forgets diagrams that neither this document nor the one before it showed, so a
    /// reload keeps the ones that did not change, and tells the worker that layouts asked
    /// for before `gen` are no longer needed.
    pub fn retain_generation(&mut self, gen: u64) {
        self.entries.retain(|_, e| e.gen + 1 >= gen);
        self.current.store(gen, Ordering::Relaxed);
    }
}

/// A layout taken off the queue unrun: its key, generation and source.
type SkippedLayout = (Key, u64, Arc<str>);

/// Takes the job to run next off `queue`. Redraws come first: they are for diagrams on
/// screen, while layouts are requested for the whole document so that its height is known.
/// Layouts asked for before generation `current`, for a document no longer shown, are
/// taken off into `skipped` instead of being run: one diagram can take seconds, and those
/// of the document now shown would wait behind them.
fn next_job(queue: &mut Vec<Job>, current: u64, skipped: &mut Vec<SkippedLayout>) -> Option<Job> {
    for job in queue.extract_if(.., |j| matches!(j, Job::Layout { gen, .. } if *gen < current)) {
        if let Job::Layout { key, gen, source } = job {
            skipped.push((key, gen, source));
        }
    }
    let next = queue.iter().position(|j| matches!(j, Job::Raster { .. })).unwrap_or(0);
    (!queue.is_empty()).then(|| queue.remove(next))
}

fn worker(rx: mpsc::Receiver<Job>, tx: mpsc::Sender<Done>, ctx: egui::Context, current: Arc<AtomicU64>) {
    let mut renderers: [Option<Renderer>; 2] = [None, None];
    let mut fonts: Option<Fonts> = None;
    let mut queue: Vec<Job> = Vec::new();
    let mut skipped = Vec::new();
    loop {
        if queue.is_empty() {
            match rx.recv() {
                Ok(job) => queue.push(job),
                Err(_) => return,
            }
        }
        queue.extend(rx.try_iter());
        let next = next_job(&mut queue, current.load(Ordering::Relaxed), &mut skipped);
        if !skipped.is_empty() {
            log::debug!("skipped {} Mermaid layouts of a document no longer shown", skipped.len());
            for (key, gen, source) in skipped.drain(..) {
                if tx.send(Done::Skipped { key, gen, source }).is_err() {
                    return;
                }
            }
            ctx.request_repaint();
        }
        let Some(job) = next else { continue };
        let done = match job {
            Job::Layout { key, source, .. } => {
                let t0 = Instant::now();
                let dark = key.1;
                let renderer = renderers[dark as usize].get_or_insert_with(|| {
                    let theme = if dark { "dark" } else { "default" };
                    // Mermaid 12's "neo" look strokes shapes with a gradient, which leaves out
                    // straight lines such as the compartment lines of class diagrams.
                    let config = MermaidConfig::from_value(serde_json::json!({ "theme": theme, "look": "classic" }));
                    Renderer::new().with_engine(Engine::new().with_site_config(config))
                });
                let fonts = fonts.get_or_insert_with(Fonts::load);
                let result = match lay_out(renderer, &fonts.environment, &source) {
                    Ok(Some(svg)) => fonts.parse(&svg).map(Arc::new).map_err(|e| format!("cannot read the SVG: {e}")),
                    Ok(None) => Err("not a Mermaid diagram".into()),
                    Err(RenderError::Cancelled(_)) => Err("it took too long to lay out".into()),
                    Err(e) => Err(e.to_string().lines().next().unwrap_or_default().to_owned()),
                };
                log::debug!("laid out a Mermaid diagram in {:.1} ms", t0.elapsed().as_secs_f64() * 1000.0);
                Done::Layout { key, result }
            }
            Job::Raster { key, tree, size } => {
                let t0 = Instant::now();
                let image = rasterize(&tree, size);
                log::debug!("drew a {}x{} px Mermaid diagram in {:.1} ms", size[0], size[1], t0.elapsed().as_secs_f64() * 1000.0);
                Done::Raster { key, image }
            }
        };
        if tx.send(done).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

/// The diagram as SVG that usvg can draw: labels as SVG text rather than HTML, and no
/// background, so that it sits on the page in either theme.
fn lay_out(renderer: &Renderer, environment: &SvgEnvironment, source: &str) -> Result<Option<String>, RenderError> {
    let request = SvgRequest {
        environment: environment.clone(),
        pipeline: Some(SvgPipeline::resvg_safe().with_postprocessor(RootBackgroundPostprocessor::new("transparent"))),
        ..Default::default()
    };
    let control = OperationControl::new().with_deadline(LAYOUT_DEADLINE);
    match renderer.render(RenderRequest::svg(source, control, request))? {
        // Class, state and ER diagrams put each label in an <svg> of the width merman measured,
        // which can be less than the font draws, and that <svg> would cut the text off.
        RenderOutput::Svg(Some(svg)) => Ok(Some(svg.svg().replace(r#"style="overflow:hidden;"#, r#"style="overflow:visible;"#))),
        _ => Ok(None),
    }
}

fn rasterize(tree: &usvg::Tree, [w, h]: [usize; 2]) -> ColorImage {
    let (w, h) = (w.max(1), h.max(1));
    let Some(mut pixmap) = tiny_skia::Pixmap::new(w as u32, h as u32) else {
        return ColorImage::filled([w, h], egui::Color32::TRANSPARENT);
    };
    let sx = w as f32 / (tree.size().width() + 2.0 * PAD);
    let sy = h as f32 / (tree.size().height() + 2.0 * PAD);
    resvg::render(tree, tiny_skia::Transform::from_row(sx, 0.0, 0.0, sy, PAD * sx, PAD * sy), &mut pixmap.as_mut());
    ColorImage::from_rgba_premultiplied([w, h], pixmap.data())
}

/// Fonts for diagram text: Trebuchet MS, Mermaid's default font, where installed (Windows,
/// macOS, Linux with the Microsoft core fonts), else a sans-serif of similar width. usvg
/// only searches the few fonts loaded here, which keeps text layout fast (all system fonts
/// make it 50 times slower); a font for a character these lack is loaded the first time
/// one is needed.
struct Fonts {
    db: Arc<fontdb::Database>,
    /// Every installed font, of which only the names have been read.
    system: fontdb::Database,
    /// Characters that no installed font has.
    missing: Mutex<HashSet<char>>,
    /// Lays diagrams out with the widths these fonts draw the labels at.
    environment: SvgEnvironment,
}

impl Fonts {
    fn load() -> Self {
        let t0 = Instant::now();
        let mut system = fontdb::Database::new();
        system.load_system_fonts();
        let mut db = fontdb::Database::new();
        let sans = add_family(
            &system,
            &mut db,
            &["Trebuchet MS", "Arial", "Liberation Sans", "Helvetica", "Arimo", "Noto Sans", "DejaVu Sans"],
        );
        let mono = add_family(
            &system,
            &mut db,
            &["Courier New", "Liberation Mono", "Cousine", "Menlo", "Consolas", "DejaVu Sans Mono", "Noto Sans Mono"],
        );
        // Mermaid's stack ("trebuchet ms", verdana, arial, sans-serif) ends up at sans-serif,
        // as fontdb compares names case-sensitively; usvg falls back to serif after that.
        if let Some(sans) = &sans {
            db.set_sans_serif_family(sans.as_str());
            db.set_serif_family(sans.as_str());
        }
        if let Some(mono) = &mono {
            db.set_monospace_family(mono.as_str());
        }
        log::debug!(
            "diagram fonts {sans:?} and {mono:?} from {} system fonts in {:.1} ms",
            system.len(),
            t0.elapsed().as_secs_f64() * 1000.0
        );
        let widths = TextWidths::new(&db, sans.as_deref(), mono.as_deref());
        let measurer = DeterministicTextMeasurer::default().with_width_callback(move |text, style| widths.width(text, style));
        let identity = MeasurementProfileId::new("mdread.diagram-fonts")
            .and_then(|id| TextMeasurementProfileIdentity::new(id, env!("CARGO_PKG_VERSION")))
            .expect("profile name and version are not empty");
        let policy = TextMeasurementPolicy::uniform(TextMeasurementProfile::new(identity, measurer));
        let environment = SvgEnvironment::deterministic().with_text_measurement_policy(policy);
        Self { db: Arc::new(db), system, missing: Mutex::new(HashSet::new()), environment }
    }

    /// Loads a font for each character of `svg` that the loaded fonts lack. usvg asks the
    /// fallback for a text's first missing character only, so when that one cannot be
    /// drawn the fonts for the characters after it must be loaded already (see
    /// `load_fallback`). Loading them up front is no more work than loading them on demand.
    fn preload(&mut self, svg: &str) {
        let mut chars: Vec<char> = svg_chars(svg).filter(|c| !c.is_ascii()).collect();
        chars.sort_unstable();
        chars.dedup();
        for c in chars {
            if !self.db.faces().any(|f| has_char(&self.db, f.id, c)) {
                load_fallback(&self.system, &self.missing, c, &[], &mut self.db);
            }
        }
    }

    fn parse(&mut self, svg: &str) -> Result<usvg::Tree, usvg::Error> {
        self.preload(svg);
        let (system, missing) = (&self.system, &self.missing);
        let loaded = usvg::FontResolver::default_fallback_selector();
        let mut opt = usvg::Options { fontdb: self.db.clone(), ..Default::default() };
        // Diagrams are shapes and text. Never read an image an SVG names: a UNC path would
        // connect to that host.
        opt.image_href_resolver.resolve_string = Box::new(|_, _| None);
        opt.font_resolver.select_fallback =
            Box::new(move |c, used, db| loaded(c, used, db).or_else(|| load_fallback(system, missing, c, used, db)));
        let tree = usvg::Tree::from_str(svg, &opt)?;
        // Keep the fallback fonts loaded for this diagram for the next ones.
        self.db = tree.fontdb().clone();
        Ok(tree)
    }
}

/// A font file's data and the index of a face in it.
type FaceFile = (Arc<[u8]>, u32);

/// How wide usvg will draw a label. merman's own estimate is narrower than Trebuchet MS
/// draws most text, which leaves labels running past their boxes and edge labels into each
/// other.
struct TextWidths {
    /// Regular, bold, italic and bold italic faces of the sans-serif font, then of the
    /// monospace one.
    faces: [Option<FaceFile>; 8],
}

impl TextWidths {
    fn new(db: &fontdb::Database, sans: Option<&str>, mono: Option<&str>) -> Self {
        let mut files: Vec<(fontdb::ID, FaceFile)> = Vec::new();
        let faces = std::array::from_fn(|i| {
            let query = fontdb::Query {
                families: &[fontdb::Family::Name(if i < 4 { sans? } else { mono? })],
                weight: if i & 1 == 1 { fontdb::Weight::BOLD } else { fontdb::Weight::NORMAL },
                style: if i & 2 == 2 { fontdb::Style::Italic } else { fontdb::Style::Normal },
                ..Default::default()
            };
            let id = db.query(&query)?;
            if let Some((_, file)) = files.iter().find(|(loaded, _)| *loaded == id) {
                return Some(file.clone());
            }
            let file = db.with_face_data(id, |data, index| (Arc::from(data), index))?;
            files.push((id, file.clone()));
            Some(file)
        });
        Self { faces }
    }

    fn width(&self, text: &str, style: &TextStyle) -> f64 {
        let family = style.font_family.as_deref().unwrap_or_default().to_ascii_lowercase();
        let mono = ["mono", "courier", "consolas", "menlo"].iter().any(|m| family.contains(m));
        let bold = style.font_weight.as_deref().is_some_and(|w| w.starts_with("bold") || w.parse::<u16>().is_ok_and(|w| w >= 600));
        let italic = matches!(style.font_style.as_deref(), Some("italic" | "oblique"));
        let size = style.font_size;
        // Characters the font lacks are drawn with another one; an em for the wide ones.
        let guess = |c: char| {
            let wide = matches!(c, '\u{1100}'..='\u{115F}' | '\u{2E80}'..='\u{A4CF}' | '\u{AC00}'..='\u{D7A3}'
                | '\u{F900}'..='\u{FAFF}' | '\u{FF00}'..='\u{FF60}' | '\u{1F300}'..='\u{1FAFF}' | '\u{20000}'..);
            size * if wide { 1.0 } else { 0.6 }
        };
        let face = self.faces[mono as usize * 4 + italic as usize * 2 + bold as usize]
            .as_ref()
            .and_then(|(data, index)| rustybuzz::Face::from_slice(data, *index));
        let Some(face) = face else { return text.chars().map(guess).sum() };
        let mut buffer = rustybuzz::UnicodeBuffer::new();
        buffer.push_str(text);
        let shaped = rustybuzz::shape(&face, &[], buffer);
        let scale = size / f64::from(face.units_per_em());
        let glyphs = shaped.glyph_infos().iter().zip(shaped.glyph_positions());
        glyphs
            .map(|(glyph, pos)| match glyph.glyph_id {
                0 => text.get(glyph.cluster as usize..).and_then(|t| t.chars().next()).map_or(0.0, guess),
                _ => f64::from(pos.x_advance) * scale,
            })
            .sum()
    }
}

/// Loads the first of `names` that is installed into `db`, all its styles, and returns its name.
fn add_family(system: &fontdb::Database, db: &mut fontdb::Database, names: &[&str]) -> Option<String> {
    for name in names {
        let mut found = None;
        let mut files = HashSet::new();
        for face in system.faces() {
            let Some((family, _)) = face.families.iter().find(|(f, _)| f.eq_ignore_ascii_case(name)) else { continue };
            if let Some(path) = face_path(face) {
                if files.insert(path) && db.load_font_file(path).is_ok() {
                    found = Some(family.clone());
                }
            }
        }
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Finds an installed font that has `c`, preferring a regular face, and adds it to `db`
/// unless it is there already. `used` are the faces this text was shaped with so far.
fn load_fallback(
    system: &fontdb::Database,
    missing: &Mutex<HashSet<char>>,
    c: char,
    used: &[fontdb::ID],
    db: &mut Arc<fontdb::Database>,
) -> Option<fontdb::ID> {
    let mut missing = missing.lock().ok()?;
    if missing.contains(&c) {
        return None;
    }
    let regular = |f: &&fontdb::FaceInfo| f.style == fontdb::Style::Normal && f.weight == fontdb::Weight::NORMAL;
    let has = |f: &&fontdb::FaceInfo| has_char(system, f.id, c);
    let Some(face) = system.faces().filter(regular).find(has).or_else(|| system.faces().find(has)) else {
        missing.insert(c);
        return None;
    };
    let path = face_path(face)?;
    // usvg asks for the first character of a cluster as long as any glyph in it is missing,
    // so a mark that no font has after a letter ("a\u{20D7}", or the U+20E3 of a keycap
    // emoji) asks for the letter again and again. Loading the same file under a new ID each
    // time would never end, so a face that is already loaded is not loaded again. Once this
    // text was shaped with it, the letter's cluster cannot be completed, and None would end
    // the fallback for the whole text, leaving anything after the mark that needs another
    // font undrawn. So each loaded face not tried yet is offered instead (Fonts::preload has
    // loaded those the text needs), and None only once they all were. `c` is not missing.
    if db.faces().any(|f| f.index == face.index && face_path(f) == Some(path)) {
        let copy = db.faces().find(|f| f.index == face.index && face_path(f) == Some(path) && !used.contains(&f.id));
        return copy.map(|f| f.id).or_else(|| untried_face(db, used));
    }
    let db = Arc::make_mut(db);
    let before: HashSet<fontdb::ID> = db.faces().map(|f| f.id).collect();
    db.load_font_file(path).ok()?;
    log::debug!("diagram text: {c:?} from {}", face.post_script_name);
    db.faces().find(|f| !before.contains(&f.id) && f.index == face.index).map(|f| f.id)
}

/// Whether face `id` in `db` has a glyph for `c`.
fn has_char(db: &fontdb::Database, id: fontdb::ID, c: char) -> bool {
    let glyph = db.with_face_data(id, |data, index| ttf_parser::Face::parse(data, index).ok()?.glyph_index(c));
    matches!(glyph, Some(Some(_)))
}

/// A face in `db` that neither is one of `used` nor comes from the same file as one.
fn untried_face(db: &fontdb::Database, used: &[fontdb::ID]) -> Option<fontdb::ID> {
    let tried: Vec<_> = used.iter().filter_map(|id| db.face(*id)).map(|f| (face_path(f), f.index)).collect();
    db.faces().find(|f| !used.contains(&f.id) && !tried.contains(&(face_path(f), f.index))).map(|f| f.id)
}

/// The characters of `svg`, with numeric character references decoded.
fn svg_chars(svg: &str) -> impl Iterator<Item = char> + '_ {
    svg.split("&#").enumerate().flat_map(|(i, part)| {
        let reference = (i > 0).then(|| part.split_once(';')).flatten().and_then(|(num, rest)| {
            let code = match num.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse().ok()?,
            };
            Some((char::from_u32(code)?, rest))
        });
        match reference {
            Some((c, rest)) => Some(c).into_iter().chain(rest.chars()),
            None => None.into_iter().chain(part.chars()),
        }
    })
}

fn face_path(face: &fontdb::FaceInfo) -> Option<&Path> {
    match &face.source {
        fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => Some(path),
        fontdb::Source::Binary(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mermaid_info_strings() {
        assert!(is_mermaid(Some("mermaid")));
        assert!(is_mermaid(Some("Mermaid")));
        assert!(is_mermaid(Some("mermaid title=\"x\"")));
        assert!(!is_mermaid(Some("mermaidx")));
        assert!(!is_mermaid(Some("rust")));
        assert!(!is_mermaid(None));
    }

    #[test]
    fn texture_size_is_capped_by_side_and_area() {
        // Small enough: the size shown, rounded.
        assert_eq!(texture_size(vec2(1200.4, 800.6), 16384.0), [1200, 801]);
        assert_eq!(texture_size(vec2(0.2, 0.0), 16384.0), [1, 1]);
        // Too long for the GPU: shrunk to its limit, same shape.
        assert_eq!(texture_size(vec2(20000.0, 1000.0), 8192.0), [8192, 409]);
        // Under the side limit but too many pixels: shrunk to at most MAX_PIXELS, same shape.
        for (w, h) in [(30000.0, 30000.0), (32768.0, 2000.0), (12345.6, 9876.5), (200000.0, 200000.0), (5792.6, 5792.6)] {
            let [tw, th] = texture_size(vec2(w, h), 32768.0);
            assert!(tw * th <= MAX_PIXELS, "{w}x{h} -> {tw}x{th}");
            assert!(tw * th > MAX_PIXELS * 99 / 100, "{w}x{h} -> {tw}x{th}");
            assert!(tw <= 32768 && th <= 32768);
            assert!((tw as f32 / th as f32 - w / h).abs() < 0.01 * w / h, "{w}x{h} -> {tw}x{th}");
        }
    }

    fn layout(n: u64, gen: u64) -> Job {
        Job::Layout { key: (n, false), gen, source: Arc::from("graph TD; a-->b") }
    }

    fn raster(n: u64) -> Job {
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"/>"#;
        let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).unwrap();
        Job::Raster { key: (n, false), tree: Arc::new(tree), size: [1, 1] }
    }

    /// The jobs `next_job` hands out until the queue is empty, as (kind, key), and the
    /// skipped layouts as (key, gen).
    fn run(mut queue: Vec<Job>, current: u64) -> (Vec<(&'static str, u64)>, Vec<(u64, u64)>) {
        let (mut ran, mut skipped) = (Vec::new(), Vec::new());
        while let Some(job) = next_job(&mut queue, current, &mut skipped) {
            ran.push(match job {
                Job::Layout { key, .. } => ("layout", key.0),
                Job::Raster { key, .. } => ("raster", key.0),
            });
        }
        (ran, skipped.into_iter().map(|(key, gen, _)| (key.0, gen)).collect())
    }

    #[test]
    fn next_job_skips_layouts_of_documents_no_longer_shown() {
        let (ran, skipped) = run(vec![layout(1, 1), layout(2, 2), layout(3, 1), layout(4, 2)], 2);
        assert_eq!(ran, [("layout", 2), ("layout", 4)]);
        assert_eq!(skipped, [(1, 1), (3, 1)]);
        // Only layouts are skipped: a redraw is for a diagram on screen, whatever generation asked for it.
        let (ran, skipped) = run(vec![layout(1, 1), raster(2), layout(3, 2)], 2);
        assert_eq!(ran, [("raster", 2), ("layout", 3)]);
        assert_eq!(skipped, [(1, 1)]);
        // A queue of only stale layouts runs nothing.
        let (ran, skipped) = run(vec![layout(1, 1)], 2);
        assert!(ran.is_empty());
        assert_eq!(skipped, [(1, 1)]);
    }

    #[test]
    fn next_job_runs_redraws_first_then_layouts_in_order() {
        let (ran, skipped) = run(vec![layout(1, 1), layout(2, 1), raster(3), layout(4, 1), raster(5)], 1);
        assert_eq!(ran, [("raster", 3), ("raster", 5), ("layout", 1), ("layout", 2), ("layout", 4)]);
        assert!(skipped.is_empty());
    }

    #[test]
    fn skipped_layouts_are_sent_again_only_for_the_document_shown() {
        // Reloaded unchanged (shown in generation 3, current 3), last sent in 2: lay it out.
        assert_eq!(on_skipped(true, 3, 2, 2, 3), OnSkip::Resend);
        // Skipped before the new document's first frame: removed, and that frame asks again.
        assert_eq!(on_skipped(true, 2, 2, 2, 3), OnSkip::Remove);
        // Only the document before showed it.
        assert_eq!(on_skipped(true, 1, 1, 1, 3), OnSkip::Remove);
        // Laid out or failed meanwhile, or sent again since: a stale reply.
        assert_eq!(on_skipped(false, 3, 2, 2, 3), OnSkip::Ignore);
        assert_eq!(on_skipped(true, 3, 3, 2, 3), OnSkip::Ignore);
    }

    /// Fonts loaded, glyphs as (text, font name), and whether 'a' was taken for missing.
    type Shaped = (usize, Vec<(String, String)>, bool);

    /// Parses `text` as an SVG label with egui's Ubuntu Light as the sans-serif font and
    /// egui's `extra` fonts as the only other installed ones, so the result does not depend
    /// on the machine's fonts. Returns the fonts loaded, each glyph's text and the name of
    /// the font it was drawn with ("" for none), and whether 'a' was taken for missing.
    fn shape_label(test: &str, extra: &[&str], text: &str) -> Shaped {
        let dir = std::env::temp_dir().join(format!("mdread-fallback-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let defs = egui::FontDefinitions::default();
        for name in ["Ubuntu-Light"].iter().chain(extra) {
            std::fs::write(dir.join(format!("{name}.ttf")), (*defs.font_data[*name].font).as_ref()).unwrap();
        }
        let mut system = fontdb::Database::new();
        system.load_fonts_dir(&dir);
        let mut db = fontdb::Database::new();
        let sans = add_family(&system, &mut db, &["Ubuntu", "Ubuntu Light"]).expect("Ubuntu Light loads");
        db.set_sans_serif_family(sans.as_str());
        db.set_serif_family(sans.as_str());
        let mut fonts = Fonts { db: Arc::new(db), system, missing: Mutex::default(), environment: SvgEnvironment::deterministic() };
        let svg = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="40"><text x="10" y="30" font-family="&quot;trebuchet ms&quot;, sans-serif">{text}</text></svg>"#
        );
        // On a thread, so that a loop fails the test instead of hanging it.
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let tree = fonts.parse(&svg).expect("the SVG parses");
            let mut glyphs = Vec::new();
            for node in tree.root().children() {
                let usvg::Node::Text(t) = node else { continue };
                for g in t.layouted().iter().flat_map(|span| &span.positioned_glyphs) {
                    let font = match g.id.0 {
                        0 => String::new(),
                        _ => tree.fontdb().face(g.font).map(|f| f.families[0].0.clone()).unwrap_or_default(),
                    };
                    glyphs.push((g.text.clone(), font));
                }
            }
            let _ = tx.send((tree.fontdb().len(), glyphs, fonts.missing.lock().unwrap().contains(&'a')));
        });
        let result = rx.recv_timeout(Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&dir);
        result.expect("font fallback did not end")
    }

    #[test]
    fn fallback_for_a_mark_no_font_has_ends() {
        // Ubuntu Light has 'a' but not U+20D7, the combining arrow above, and no other font does.
        let (faces, _, missing) = shape_label("mark", &[], "a&#x20D7;");
        // The font it was drawn with is not loaded again, and 'a' is not taken for a character no font has.
        assert_eq!(faces, 1);
        assert!(!missing);
    }

    #[test]
    fn text_after_a_mark_no_font_has_still_falls_back() {
        // Noto Emoji has U+1F600 but not 'a'. usvg asks for 'a' while its arrow is missing,
        // and the emoji after it must still get its font.
        let (faces, glyphs, missing) = shape_label("after-mark", &["NotoEmoji-Regular"], "a&#x20D7; &#x1F600;");
        assert_eq!(faces, 2);
        assert!(!missing);
        let emoji = glyphs.iter().find(|(text, _)| text == "\u{1F600}").expect("the emoji is laid out");
        assert!(emoji.1.contains("Emoji"), "the emoji is drawn with {:?}", emoji.1);
    }
}
