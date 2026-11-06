//! Application state, input handling and the top-level layout
//! (outline sidebar · reading column · file sidebar · status bar).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use egui::scroll_area::{DragScroll, ScrollAreaOutput, ScrollSource};
use egui::text::{CCursor, CCursorRange};
use egui::text_selection::LabelSelectionState;
use egui::{
    pos2, vec2, Align, Align2, Button, Color32, Context, CornerRadius, Event, Frame, Id, Key, Label, LayerId, Layout,
    Margin, Modifiers, MouseWheelSource, Panel, PointerButton, Popup, PopupAnchor, PopupCloseBehavior, PopupKind, Pos2, Rect, RichText,
    ScrollArea, Sense, Shape, Stroke, TextEdit, Ui, UiBuilder, ViewportCommand,
};
use notify::Watcher;
use serde::{Deserialize, Serialize};

use crate::document::{Block, Document, InlineItem};
use crate::files;
use crate::fonts::{self, FontSet};
use crate::highlight::{self, Highlighter};
use crate::links::{self, Target};
use crate::mermaid::Diagrams;
use crate::render::{block_gap, Action, RenderCtx};
use crate::search::SearchState;
use crate::text;
use crate::theme::{self, Palette};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeChoice {
    System,
    Dark,
    Light,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub zoom: f32,
    pub base_size: f32,
    pub column_width: f32,
    pub full_width: bool,
    pub show_outline: bool,
    pub show_files: bool,
    pub theme: ThemeChoice,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            base_size: 16.5,
            column_width: 860.0,
            full_width: false,
            show_outline: true,
            show_files: true,
            theme: ThemeChoice::System,
        }
    }
}

/// Reading-column widths offered in the status bar, in points.
const WIDTH_PRESETS: [(&str, f32); 4] = [("Narrow", 720.0), ("Normal", 860.0), ("Wide", 1200.0), ("Extra wide", 1600.0)];

/// Bounds of `Settings::column_width`, in points.
const MIN_COLUMN: f32 = 400.0;
const MAX_COLUMN: f32 = 3000.0;

#[derive(Clone, Copy, Debug, PartialEq)]
enum WidthStep {
    Preset(f32),
    Full,
}

/// A small button that shows its frame only when hovered, for the status bar and the
/// sidebar header.
fn flat_button(text: RichText) -> Button<'static> {
    Button::new(text).frame_when_inactive(false).small()
}

/// An arrow for a `flat_button`, in the code font: the usual UI fonts (Ubuntu Sans,
/// egui's own) have no arrows, and code fonts do.
fn arrow(text: &str, size: f32, color: Color32) -> RichText {
    RichText::new(text).size(size).color(color).family(egui::FontFamily::Monospace)
}

/// The last part of `dir`'s path, or all of it for a root (`/`, `C:\`).
fn folder_name(dir: &Path) -> String {
    dir.file_name().map_or_else(|| links::display_path(dir), |n| n.to_string_lossy().into_owned())
}

/// Where the status bar's − or + button goes from a column `now` points wide: the next
/// narrower preset, or the next wider one that fits in `column_max` and then full width.
fn width_step(now: f32, full_width: bool, column_max: f32, wider: bool) -> Option<WidthStep> {
    let mut presets = WIDTH_PRESETS.iter().map(|p| p.1);
    if !wider {
        presets.rev().find(|&w| w < now - 1.0).map(WidthStep::Preset)
    } else if full_width {
        None
    } else {
        Some(presets.find(|&w| w > now + 1.0 && w < column_max - 1.0).map_or(WidthStep::Full, WidthStep::Preset))
    }
}

#[derive(Clone, Default)]
pub struct Args {
    pub files: Vec<PathBuf>,
    pub stdin: bool,
    pub theme: Option<ThemeChoice>,
    /// Debug aid: save a PNG of the window after `screenshot_delay_ms` and exit.
    pub screenshot: Option<PathBuf>,
    pub screenshot_delay_ms: u64,
    pub launched: Option<Instant>,
    /// `#anchor` given after the file name on the command line.
    pub anchor: Option<String>,
    /// `--find TERM`: open with the search box filled in.
    pub find: Option<String>,
}

enum WatchEvent {
    /// The open file changed.
    Changed,
    /// The file changed without a watcher event (Windows): notify silently stops watching
    /// after an error or a buffer overflow, so start a new watcher.
    #[cfg_attr(not(windows), allow(dead_code))]
    Missed,
}

/// Size and modification time of the open file.
type FileStamp = (u64, SystemTime);

fn file_stamp(path: &Path) -> Option<FileStamp> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.len(), m.modified().ok()?))
}

/// How often the open file's stamp is checked on Windows, as a fallback for the watcher.
#[cfg(windows)]
const STAMP_POLL: Duration = Duration::from_secs(2);

/// A file in the back/forward history, and how it was left.
#[derive(Clone)]
struct Visit {
    path: PathBuf,
    /// The scroll offset: the first frame's guess, and the place when `at` is `None` (the page top).
    offset: f32,
    /// The block, how far its top was below the view top, and the block's height, as
    /// `Anchor::place` gives them; `None` until the file is left, or when it was left at the top.
    at: Option<(usize, f32, f32)>,
    /// The folder the files sidebar showed; `None` until the file is left.
    files_dir: Option<PathBuf>,
}

#[derive(Clone, Copy, Default)]
struct Measure {
    height: f32,
    measured: bool,
    /// The block drew an image or diagram that was still loading, so `height` has its
    /// placeholder's size rather than its own.
    pending: bool,
}

/// The right-click menu over the reading column.
///
/// Its open state lives here rather than in egui's popup memory because the
/// right-press that opens it is swallowed in `raw_input_hook` before egui sees it:
/// egui collapses the label text selection on any press over a label, which would
/// leave nothing for "Copy" to copy.
struct ContextMenu {
    pos: Pos2,
    link: Option<String>,
    image: Option<String>,
    code: Option<Arc<str>>,
    /// Popup rect from the last frame; presses outside it dismiss the menu.
    rect: Rect,
    /// The enabled "Copy" item's rect; a press on it becomes an `Event::Copy`.
    copy_rect: Option<Rect>,
}

#[derive(Clone, Copy)]
enum MenuItem {
    OpenLink,
    CopyLink,
    OpenImage,
    CopyImage,
    CopyCode,
}

pub struct App {
    settings: Settings,
    fonts: FontSet,
    pal: Palette,
    system_dark: Option<bool>,

    doc: Option<Arc<Document>>,
    gen: u64,
    path: Option<PathBuf>,
    source: String,
    error: Option<String>,
    stdin_mode: bool,

    measures: Vec<Measure>,
    block_tops: Vec<f32>,
    layout_key: (i32, i32),
    measure_all: bool,

    scroll_offset: f32,
    viewport_h: f32,
    content_h: f32,
    /// The document generation `content_h` was measured for.
    content_gen: u64,
    pending_offset: Option<f32>,
    scroll_delta: f32,
    /// The reader's place as a block and an offset into it: jumps and restored positions are held
    /// on their block while content above it changes height.
    anchor: Anchor,
    pending_anchor: Option<String>,

    search: SearchState,
    hl: Highlighter,
    diagrams: Diagrams,
    hovered_link: Option<(usize, u32)>,

    /// Reading-column viewport from the last frame, in points.
    content_rect: Rect,
    /// Widest the reading column fits in the window, from the last frame, in points.
    column_max: f32,
    menu: Option<ContextMenu>,
    /// Right-press position waiting for the next frame's hover info.
    menu_request: Option<Pos2>,
    /// A press was swallowed in `raw_input_hook`; swallow its release too.
    swallow_release: Option<PointerButton>,

    history: Vec<Visit>,
    history_pos: usize,

    watcher: Option<notify::RecommendedWatcher>,
    watch_rx: Option<mpsc::Receiver<WatchEvent>>,
    /// Stops the stamp poller of the previous watcher.
    watch_alive: Arc<AtomicBool>,
    /// Stamp of the file as last read.
    seen: Arc<egui::mutex::Mutex<Option<FileStamp>>>,
    reload_at: Option<Instant>,
    reload_retries: u32,
    dialog_rx: Option<mpsc::Receiver<Option<PathBuf>>>,
    /// Errors from opening links in other apps.
    launch_tx: mpsc::Sender<String>,
    launch_rx: mpsc::Receiver<String>,

    files: Vec<PathBuf>,
    files_dir: Option<PathBuf>,
    files_rx: Option<mpsc::Receiver<Vec<PathBuf>>>,

    status: Option<(String, Instant)>,
    show_help: bool,
    current_heading: Option<usize>,
    outline_scroll_to: Option<usize>,
    started: Instant,
    frames: u64,
    /// Theme to persist: the CLI `--dark/--light` override is session-only.
    theme_override: Option<ThemeChoice>,
    saved_theme: ThemeChoice,
    screenshot: Option<PathBuf>,
    screenshot_delay_ms: u64,
    /// Where the screenshot lands once requested and taken.
    screenshot_taken: Option<Arc<egui::mutex::Mutex<Option<Arc<egui::ColorImage>>>>>,
    /// The screenshot was requested at the wait limit, before the page had finished drawing.
    screenshot_unsettled: bool,
    /// Whether the title bar was last made dark (Windows; None before the first frame).
    #[cfg(windows)]
    title_bar_dark: Option<bool>,
}

/// What a folder link opens, in this order.
const FOLDER_INDEX: &[&str] = &["README.md", "readme.md", "index.md", "README.markdown"];

/// Label for the primary shortcut modifier: egui's `Modifiers::COMMAND` is Cmd on macOS
/// and Ctrl everywhere else.
const CMD: &str = if cfg!(target_os = "macos") { "⌘" } else { "Ctrl" };

/// Shell command that prints the chosen file path on stdout, or nothing if cancelled.
/// macOS uses the native panel through `osascript`; elsewhere `zenity` (GTK). Windows
/// uses the native dialog through rfd instead.
#[cfg(not(windows))]
fn file_dialog_command(start_dir: Option<&Path>) -> std::process::Command {
    if cfg!(target_os = "macos") {
        let mut script = String::from(
            "POSIX path of (choose file with prompt \"Open Markdown file\" of type {\"md\", \"markdown\", \"mdown\", \"mkd\", \"mdx\", \"txt\", \"public.plain-text\"}",
        );
        if let Some(d) = start_dir {
            // AppleScript string literals: escape backslashes and double quotes.
            let d = d.display().to_string().replace('\\', "\\\\").replace('"', "\\\"");
            script.push_str(&format!(" default location POSIX file \"{d}\""));
        }
        script.push(')');
        let mut cmd = std::process::Command::new("osascript");
        cmd.arg("-e").arg(script);
        cmd
    } else {
        let mut cmd = std::process::Command::new("zenity");
        cmd.arg("--file-selection")
            .arg("--title=Open Markdown file")
            .arg("--file-filter=Markdown | *.md *.markdown *.mdown *.mkd *.mdx *.txt")
            .arg("--file-filter=All files | *");
        if let Some(d) = start_dir {
            cmd.arg(format!("--filename={}/", d.display()));
        }
        cmd
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Args) -> Self {
        let ctx = &cc.egui_ctx;
        let mut settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, eframe::APP_KEY))
            .unwrap_or_default();
        let saved_theme = settings.theme;
        if let Some(t) = args.theme {
            settings.theme = t;
        }
        #[cfg(windows)]
        crate::svg_loader::install(ctx);
        egui_extras::install_image_loaders(ctx);
        highlight::preload();
        let fonts = fonts::install(ctx);
        ctx.set_zoom_factor(settings.zoom.clamp(0.5, 3.0));

        let system_dark = ctx.system_theme().map(|t| t == egui::Theme::Dark);
        let dark = match settings.theme {
            ThemeChoice::Dark => true,
            ThemeChoice::Light => false,
            ThemeChoice::System => system_dark.unwrap_or(true),
        };
        let pal = if dark { Palette::dark() } else { Palette::light() };
        theme::apply(ctx, &pal, &fonts, settings.base_size);
        let (launch_tx, launch_rx) = mpsc::channel();

        let mut app = Self {
            settings,
            fonts,
            pal,
            system_dark,
            doc: None,
            gen: 0,
            path: None,
            source: String::new(),
            error: None,
            stdin_mode: false,
            measures: Vec::new(),
            block_tops: Vec::new(),
            layout_key: (0, 0),
            measure_all: false,
            scroll_offset: 0.0,
            viewport_h: 800.0,
            content_h: 0.0,
            content_gen: 0,
            pending_offset: None,
            scroll_delta: 0.0,
            anchor: Anchor::default(),
            pending_anchor: None,
            search: SearchState::default(),
            hl: Highlighter::new(ctx.clone()),
            diagrams: Diagrams::new(ctx.clone()),
            hovered_link: None,
            content_rect: Rect::NOTHING,
            column_max: f32::INFINITY,
            menu: None,
            menu_request: None,
            swallow_release: None,
            history: Vec::new(),
            history_pos: 0,
            watcher: None,
            watch_rx: None,
            watch_alive: Arc::new(AtomicBool::new(false)),
            seen: Arc::new(egui::mutex::Mutex::new(None)),
            reload_at: None,
            reload_retries: 0,
            dialog_rx: None,
            launch_tx,
            launch_rx,
            files: Vec::new(),
            files_dir: None,
            files_rx: None,
            status: None,
            show_help: false,
            current_heading: None,
            outline_scroll_to: None,
            started: args.launched.unwrap_or_else(Instant::now),
            frames: 0,
            theme_override: args.theme,
            saved_theme,
            screenshot: args.screenshot.clone(),
            screenshot_delay_ms: args.screenshot_delay_ms,
            screenshot_taken: None,
            screenshot_unsettled: false,
            #[cfg(windows)]
            title_bar_dark: None,
        };

        if args.stdin {
            use std::io::Read;
            app.stdin_mode = true;
            let mut bytes = Vec::new();
            match std::io::stdin().read_to_end(&mut bytes) {
                Ok(n) if n > 0 => {
                    let decoded = text::decode(bytes);
                    app.set_document(decoded.text, None, ctx);
                    if let Some(enc) = decoded.encoding {
                        app.set_status(format!("Decoded as {enc}"));
                    }
                }
                Ok(_) => app.error = Some("No input on stdin".into()),
                Err(e) => app.error = Some(format!("Cannot read stdin: {e}")),
            }
        } else if let Some(p) = args.files.first() {
            if app.load_path(p.clone(), true, ctx) {
                app.pending_anchor = args.anchor.clone();
            }
        }
        if let Some(q) = &args.find {
            app.search.open = true;
            app.search.query = q.clone();
            app.search.focus_pending = true;
        }
        app
    }

    // ------------------------------------------------------------------
    // Loading
    // ------------------------------------------------------------------

    fn set_document(&mut self, text: String, path: Option<PathBuf>, ctx: &Context) {
        let t0 = Instant::now();
        fonts::add_script_fallbacks(ctx, &self.fonts, &text);
        if let Some(p) = &path {
            fonts::add_script_fallbacks(ctx, &self.fonts, &p.to_string_lossy());
        }
        let doc = Document::parse(&text);
        log::debug!("parsed {} bytes into {} blocks in {:.2} ms", text.len(), doc.blocks.len(), t0.elapsed().as_secs_f64() * 1000.0);
        self.gen += 1;
        self.hl.retain_generation(self.gen);
        self.diagrams.retain_generation(self.gen);
        self.measures = vec![Measure::default(); doc.blocks.len()];
        self.block_tops = vec![0.0; doc.blocks.len()];
        self.measure_all = doc.blocks.len() <= 600;
        self.anchor.reset();
        self.error = None;
        self.source = text;
        let title = match (&doc.title, &path) {
            (_, Some(p)) => format!("{} — mdread", p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()),
            (Some(t), None) => format!("{t} — mdread"),
            (None, None) => "mdread".to_owned(),
        };
        ctx.send_viewport_cmd(ViewportCommand::Title(title));
        self.doc = Some(Arc::new(doc));
        self.path = path;
        self.hovered_link = None;
        if self.search.open {
            self.search.last_gen = 0; // force re-run
        }
    }

    /// Returns whether the file was read; on failure the error screen shows instead.
    fn load_path(&mut self, path: PathBuf, push_history: bool, ctx: &Context) -> bool {
        self.load_path_in(path, push_history, None, ctx)
    }

    /// Open `path` with the files sidebar showing `files_dir`, or a folder chosen by
    /// `files::sidebar_root` when that is `None`. Returns whether the file was read.
    fn load_path_in(&mut self, path: PathBuf, push_history: bool, files_dir: Option<PathBuf>, ctx: &Context) -> bool {
        // dunce leaves out the `\\?\` prefix Windows puts on canonical paths. Some virtual
        // drives cannot be canonicalized; an absolute path still gives the folder links
        // and the sidebar start from.
        let path = dunce::canonicalize(&path).or_else(|_| std::path::absolute(&path)).unwrap_or(path);
        let stamp = file_stamp(&path);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let decoded = text::decode(bytes);
                if push_history {
                    self.save_visit();
                    if !self.history.is_empty() {
                        self.history.truncate(self.history_pos + 1);
                    }
                    self.history.push(Visit { path: path.clone(), offset: 0.0, at: None, files_dir: None });
                    self.history_pos = self.history.len() - 1;
                }
                self.stdin_mode = false;
                self.set_document(decoded.text, Some(path.clone()), ctx);
                if let Some(enc) = decoded.encoding {
                    self.set_status(format!("Decoded as {enc}"));
                }
                self.pending_offset = Some(0.0);
                self.reload_retries = 0;
                self.setup_watcher(&path, stamp, ctx);
                let home = fonts::home_dir();
                let dir = files_dir.or_else(|| files::sidebar_root(self.files_dir.as_deref(), &path, home.as_deref()));
                self.set_files_dir(dir, ctx);
                true
            }
            Err(e) => {
                self.error = Some(format!("Cannot read {}: {e}", links::display_path(&path)));
                self.doc = None;
                self.path = Some(path);
                ctx.send_viewport_cmd(ViewportCommand::Title("mdread".into()));
                // An anchor waiting for this document has nothing to jump in, and a later reload of
                // this path must not hold a block of the previous document (its scroll offset still
                // carries over).
                self.pending_anchor = None;
                self.anchor.reset();
                false
            }
        }
    }

    fn reload(&mut self, ctx: &Context, announce: bool) {
        let Some(path) = self.path.clone() else { return };
        let stamp = file_stamp(&path);
        match std::fs::read(&path) {
            Ok(bytes) => {
                self.reload_retries = 0;
                *self.seen.lock() = stamp;
                let text = text::decode(bytes).text;
                if text == self.source {
                    return;
                }
                // The place as a block, so it survives blocks above it that are not laid out yet;
                // an edited file can have fewer blocks, and `restore` drops an index past the end.
                let at = self.anchor.place(&self.measures);
                let offset = self.scroll_offset + self.anchor.shift;
                self.set_document(text, Some(path), ctx);
                self.pending_offset = Some(offset);
                let n = self.measures.len();
                self.anchor.restore(at, n);
                if announce {
                    self.set_status("Reloaded");
                }
            }
            Err(e) if is_transient_read_error(&e) && self.reload_retries < 4 => {
                self.reload_retries += 1;
                let delay = Duration::from_millis(50 << self.reload_retries);
                log::debug!("reload {}: {e}; retrying in {delay:?}", path.display());
                self.reload_at = Some(Instant::now() + delay);
                wake_after(ctx, delay + Duration::from_millis(5));
            }
            Err(e) => {
                self.reload_retries = 0;
                self.set_status(format!("Reload failed: {e}"));
            }
        }
    }

    /// Watches `path`, last read when it had `stamp`.
    fn setup_watcher(&mut self, path: &Path, stamp: Option<FileStamp>, ctx: &Context) {
        self.watcher = None;
        self.watch_rx = None;
        self.watch_alive.store(false, Ordering::Relaxed);
        // A new cell per watcher: the stopped poller may be past its `alive` check and
        // about to store the old file's stamp.
        self.seen = Arc::new(egui::mutex::Mutex::new(stamp));
        let Some(dir) = path.parent() else { return };
        let (tx, rx) = mpsc::channel();
        self.watch_rx = Some(rx);
        #[cfg(windows)]
        self.spawn_stamp_poller(path, tx.clone(), ctx);
        let target = path.to_path_buf();
        let ctx = ctx.clone();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
            Ok(ev) => {
                let relevant = ev.paths.is_empty() || ev.paths.iter().any(|p| is_watched_file(p, &target));
                if relevant && tx.send(WatchEvent::Changed).is_ok() {
                    ctx.request_repaint();
                }
            }
            Err(e) if cfg!(windows) => {
                log::warn!("file watcher: {e}");
                if tx.send(WatchEvent::Missed).is_ok() {
                    ctx.request_repaint();
                }
            }
            Err(_) => {}
        });
        match watcher {
            Ok(mut w) => {
                if let Err(e) = w.watch(dir, notify::RecursiveMode::NonRecursive) {
                    log::warn!("watch {}: {e}", dir.display());
                } else {
                    self.watcher = Some(w);
                }
            }
            Err(e) => log::warn!("file watcher unavailable: {e}"),
        }
    }

    /// Checks the file's size and modification time every few seconds. Catches what
    /// ReadDirectoryChangesW misses: notify stops watching without a word after an error
    /// or a buffer overflow, and changes made on network shares and in WSL often never
    /// arrive. Only stat calls; nothing is read or repainted unless the file changed.
    #[cfg(windows)]
    fn spawn_stamp_poller(&mut self, path: &Path, tx: mpsc::Sender<WatchEvent>, ctx: &Context) {
        let alive = Arc::new(AtomicBool::new(true));
        self.watch_alive = alive.clone();
        let seen = self.seen.clone();
        let path = path.to_path_buf();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("file-poll".into())
            .spawn(move || loop {
                std::thread::sleep(STAMP_POLL);
                if !alive.load(Ordering::Relaxed) {
                    return;
                }
                let now = file_stamp(&path);
                let mut seen = seen.lock();
                if *seen != now {
                    *seen = now;
                    if tx.send(WatchEvent::Missed).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                }
            })
            .ok();
    }

    fn open_dialog(&mut self, ctx: &Context, frame: &eframe::Frame) {
        if self.dialog_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let start_dir = self.path.as_deref().and_then(Path::parent).map(Path::to_path_buf).or_else(|| self.files_dir.clone());
        // The native Open dialog, owned by the window so it stays on top of it and keeps it
        // disabled while open. It runs on the helper thread: its modal loop would stall
        // the event loop here.
        #[cfg(windows)]
        let dialog = {
            let dialog = rfd::FileDialog::new()
                .set_title("Open Markdown file")
                .add_filter("Markdown", &["md", "markdown", "mdown", "mkd", "mdx", "txt"])
                .add_filter("All files", &["*"])
                .set_parent(frame);
            match &start_dir {
                Some(d) => dialog.set_directory(dunce::simplified(d)),
                None => dialog,
            }
        };
        #[cfg(not(windows))]
        let _ = frame;
        std::thread::Builder::new()
            .name("file-dialog".into())
            .spawn(move || {
                #[cfg(windows)]
                let path = dialog.pick_file();
                #[cfg(not(windows))]
                let path = file_dialog_command(start_dir.as_deref())
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from);
                let _ = tx.send(path);
                ctx.request_repaint();
            })
            .ok();
        self.dialog_rx = Some(rx);
    }

    /// Opens a file dropped on the window. On Windows a dropped shortcut opens its target,
    /// and a dropped folder its README or first Markdown file: reading a folder as a file
    /// fails there with a puzzling "Access is denied".
    fn open_dropped(&mut self, path: PathBuf, ctx: &Context) {
        #[cfg(windows)]
        let path = {
            let mut path = path;
            if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk")) {
                let _com = crate::win::ComApartment::init();
                match crate::win::shortcut_target(&path) {
                    Some(target) => path = target,
                    None => {
                        self.set_status(format!("{} does not point to a file", links::display_path(&path)));
                        return;
                    }
                }
            }
            if path.is_dir() {
                match markdown_in_folder(&path) {
                    Some(file) => path = file,
                    None => {
                        self.set_status(format!("No Markdown files in {}", links::display_path(&path)));
                        return;
                    }
                }
            }
            path
        };
        self.load_path(path, true, ctx);
    }

    fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some((s.into(), Instant::now()));
    }

    /// Fonts for East Asian scripts and symbols in the UI's own short texts (messages and
    /// the search box), which can need them before any document does.
    fn add_ui_script_fallbacks(&self, ctx: &Context) {
        let status = self.status.as_ref().map(|(s, _)| s.as_str());
        let query = self.search.open.then_some(self.search.query.as_str());
        for text in [status, self.error.as_deref(), query].into_iter().flatten() {
            fonts::add_script_fallbacks(ctx, &self.fonts, text);
        }
    }

    // ------------------------------------------------------------------
    // Navigation
    // ------------------------------------------------------------------

    /// Holds `block` a gap below the view top until the reader scrolls. When last frame showed this
    /// document, its offset is placed this frame; after a load, the load's own offset is kept and the
    /// hold corrects from it in the same pass. Link actions run after the scroll area, hence the
    /// repaint.
    fn jump_to_block(&mut self, block: usize, ctx: &Context) {
        if self.doc.is_none() {
            // An error page: search keys still reach here, with the previous document's matches
            // and tops. A hold made from them would be restored into the next load of this path.
            return;
        }
        let gap = block_gap(self.settings.base_size);
        if let Some(off) = self.anchor.jump(block, gap, &self.block_tops, self.viewport_h, self.content_rect.top(), self.content_gen == self.gen) {
            self.pending_offset = Some(off);
        }
        ctx.request_repaint();
    }

    fn go_history(&mut self, delta: i32, ctx: &Context) {
        let Some(visit) = self.history_pos.checked_add_signed(delta as isize).and_then(|i| self.history.get(i)).cloned() else {
            return;
        };
        self.save_visit();
        self.history_pos = (self.history_pos as isize + delta as isize) as usize;
        if self.load_path_in(visit.path, false, visit.files_dir, ctx) {
            self.pending_offset = Some(visit.offset);
            let n = self.measures.len();
            self.anchor.restore(visit.at, n);
        }
    }

    /// Note where the current file was left, for coming back to it.
    fn save_visit(&mut self) {
        if let Some(cur) = self.history.get_mut(self.history_pos) {
            cur.offset = self.scroll_offset + self.anchor.shift;
            cur.at = self.anchor.place(&self.measures);
            cur.files_dir = self.files_dir.clone();
        }
    }

    /// The file `delta` steps back (-1) or forward (1) in the history.
    fn history_neighbour(&self, delta: isize) -> Option<&Path> {
        let i = self.history_pos.checked_add_signed(delta)?;
        self.history.get(i).map(|v| v.path.as_path())
    }

    /// Show `dir` in the files sidebar, scanning it if it changed.
    fn set_files_dir(&mut self, dir: Option<PathBuf>, ctx: &Context) {
        if dir == self.files_dir {
            return;
        }
        self.files_dir = dir.clone();
        self.files.clear();
        self.files_rx = dir.map(|d| files::spawn_scan(d, ctx.clone()));
    }

    fn open_link(&mut self, url: &str, ctx: &Context) {
        let base = self.link_base();
        match links::classify_link(url, base.as_deref()) {
            None => {}
            Some(Target::Anchor(anchor)) => {
                let target = self.doc.as_ref().and_then(|d| d.find_slug(&anchor));
                match target {
                    Some(b) => self.jump_to_block(b, ctx),
                    None => self.set_status(format!("Anchor not found: #{anchor}")),
                }
            }
            Some(Target::External(url)) => {
                self.launch(url.clone(), false, ctx);
                self.set_status(format!("Opened {url}"));
            }
            Some(Target::Blocked(msg)) => self.set_status(msg),
            Some(Target::Local(abs, frag)) => self.open_local(abs, frag, ctx),
        }
    }

    /// The folder relative links start from: the document's, or with stdin the current
    /// folder, which a terminal set (stdin is only ever piped from one).
    fn link_base(&self) -> Option<PathBuf> {
        if self.stdin_mode {
            return std::env::current_dir().ok();
        }
        self.path.as_ref().and_then(|p| p.parent()).map(Path::to_path_buf)
    }

    fn open_local(&mut self, abs: PathBuf, frag: Option<String>, ctx: &Context) {
        // Resolve `..` and links first, so the type check below sees the real file.
        #[cfg(windows)]
        let abs = dunce::canonicalize(&abs).unwrap_or(abs);
        if abs.is_dir() {
            for name in FOLDER_INDEX {
                let p = abs.join(name);
                if p.is_file() {
                    if self.load_path(p, true, ctx) {
                        self.pending_anchor = frag;
                    }
                    return;
                }
            }
            self.launch(abs, false, ctx);
            return;
        }
        if abs.is_file() {
            if files::is_markdown(&abs) || abs.extension().is_none() {
                if self.load_path(abs, true, ctx) {
                    self.pending_anchor = frag;
                }
            } else if links::opens_in_default_app(&abs) {
                self.set_status(format!("Opened {}", links::display_path(&abs)));
                self.launch(abs, false, ctx);
            } else {
                // Could be a program or script: show it instead of running it.
                let name = abs.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.set_status(format!("Showing {name} in Explorer"));
                self.launch(abs, true, ctx);
            }
        } else {
            self.set_status(format!("Not found: {}", links::display_path(&abs)));
        }
    }

    /// The link as it should read outside the viewer: web links and anchors as
    /// written, relative file links as absolute paths.
    fn link_for_clipboard(&self, url: &str) -> String {
        match links::classify_link(url, self.link_base().as_deref()) {
            Some(Target::Local(abs, frag)) => {
                let abs = links::display_path(&abs);
                match frag {
                    Some(f) if !f.is_empty() => format!("{abs}#{f}"),
                    _ => abs,
                }
            }
            _ => url.trim().to_owned(),
        }
    }

    // ------------------------------------------------------------------
    // Context menu
    // ------------------------------------------------------------------

    /// Returns false for events egui should not see; see `raw_input_hook`.
    fn keep_event(&mut self, e: &Event, copy: &mut bool) -> bool {
        match e {
            Event::PointerButton { pos, button, pressed: true, .. } => {
                let (pos, button) = (*pos, *button);
                if let Some(menu) = self.menu.take() {
                    if menu.rect.contains(pos) {
                        if menu.copy_rect.is_some_and(|r| r.contains(pos)) {
                            *copy = true;
                            self.swallow_release = Some(button);
                            return false;
                        }
                        self.menu = Some(menu);
                        return true;
                    }
                    if button != PointerButton::Secondary {
                        // A click outside only dismisses the menu.
                        self.swallow_release = Some(button);
                        return false;
                    }
                }
                if button == PointerButton::Secondary && !self.show_help && self.content_rect.contains(pos) {
                    log::trace!("context menu requested at {pos:?}");
                    self.menu_request = Some(pos);
                    self.swallow_release = Some(button);
                    return false;
                }
                true
            }
            Event::PointerButton { button, pressed: false, .. } => {
                if self.swallow_release == Some(*button) {
                    self.swallow_release = None;
                    return false;
                }
                true
            }
            Event::MouseWheel { .. } => {
                self.menu = None;
                true
            }
            _ => true,
        }
    }

    fn show_context_menu(&mut self, ctx: &Context) {
        let Some(mut menu) = self.menu.take() else { return };
        let has_selection = ctx.plugin::<LabelSelectionState>().lock().has_selection();
        let base = self.settings.base_size;
        let mut picked: Option<MenuItem> = None;
        let mut copy_rect = None;
        let out = Popup::new(Id::unique("context-menu"), ctx.clone(), PopupAnchor::Position(menu.pos), LayerId::background())
            .kind(PopupKind::Menu)
            .open(true)
            .close_behavior(PopupCloseBehavior::IgnoreClicks)
            .layout(Layout::top_down_justified(Align::Min))
            .style(egui::containers::menu::menu_style)
            .show(|ui| {
                ui.set_min_width(base * 9.0);
                // A press on "Copy" is handled in `raw_input_hook`, see `ContextMenu::copy_rect`.
                let copy = ui.add_enabled(has_selection, Button::new("Copy"));
                if has_selection {
                    copy_rect = Some(copy.rect);
                }
                if menu.link.is_some() {
                    ui.separator();
                    if ui.button("Open link").clicked() {
                        picked = Some(MenuItem::OpenLink);
                    }
                    if ui.button("Copy link").clicked() {
                        picked = Some(MenuItem::CopyLink);
                    }
                }
                if menu.image.is_some() {
                    ui.separator();
                    if ui.button("Open image").clicked() {
                        picked = Some(MenuItem::OpenImage);
                    }
                    if ui.button("Copy image address").clicked() {
                        picked = Some(MenuItem::CopyImage);
                    }
                }
                if menu.code.is_some() {
                    ui.separator();
                    if ui.button("Copy code block").clicked() {
                        picked = Some(MenuItem::CopyCode);
                    }
                }
            });
        menu.rect = out.map(|o| o.response.rect).unwrap_or(Rect::NOTHING);
        menu.copy_rect = copy_rect;
        match picked {
            None => self.menu = Some(menu),
            Some(item) => self.menu_action(item, &menu, ctx),
        }
    }

    fn menu_action(&mut self, item: MenuItem, menu: &ContextMenu, ctx: &Context) {
        match item {
            MenuItem::OpenLink => {
                if let Some(url) = &menu.link {
                    self.open_link(url, ctx);
                }
            }
            MenuItem::CopyLink => {
                if let Some(url) = &menu.link {
                    ctx.copy_text(self.link_for_clipboard(url));
                    self.set_status("Copied link to clipboard");
                }
            }
            MenuItem::OpenImage => {
                if let Some(uri) = &menu.image {
                    // Windows hands the file to the shell, so it gets the checks a click on a
                    // link gets: only safe types open, the rest is revealed in Explorer.
                    #[cfg(windows)]
                    match links::file_uri_to_path(uri) {
                        Some(path) => self.open_local(path, None, ctx),
                        None => self.open_link(uri, ctx),
                    }
                    #[cfg(not(windows))]
                    {
                        let target = local_path(uri);
                        self.launch(target, false, ctx);
                        self.set_status(format!("Opened {target}"));
                    }
                }
            }
            MenuItem::CopyImage => {
                if let Some(uri) = &menu.image {
                    ctx.copy_text(image_address(uri));
                    self.set_status("Copied image address to clipboard");
                }
            }
            MenuItem::CopyCode => {
                if let Some(code) = &menu.code {
                    ctx.copy_text(code.to_string());
                    self.set_status("Copied code block to clipboard");
                }
            }
        }
    }

    /// Opens a URL or path with the OS (or, with `reveal`, shows the file in Explorer) on a
    /// helper thread, as the shell can block. Failures come back to the status bar.
    fn launch(&self, target: impl Into<OsString>, reveal: bool, ctx: &Context) {
        let target: OsString = target.into();
        let tx = self.launch_tx.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("open".into())
            .spawn(move || {
                #[cfg(windows)]
                let result = {
                    let _com = crate::win::ComApartment::init();
                    if reveal {
                        crate::win::reveal_in_explorer(Path::new(&target))
                    } else {
                        open::that_detached(&target)
                    }
                };
                #[cfg(not(windows))]
                let result = {
                    let _ = reveal;
                    open::that_detached(&target)
                };
                if let Err(e) = result {
                    let label = links::display_path(Path::new(&target));
                    log::warn!("open {label}: {e}");
                    if tx.send(format!("Could not open {label}: {e}")).is_ok() {
                        ctx.request_repaint();
                    }
                }
            })
            .ok();
    }

    // ------------------------------------------------------------------
    // Appearance
    // ------------------------------------------------------------------

    fn apply_theme(&mut self, ctx: &Context) {
        let dark = match self.settings.theme {
            ThemeChoice::Dark => true,
            ThemeChoice::Light => false,
            ThemeChoice::System => self.system_dark.unwrap_or(true),
        };
        self.pal = if dark { Palette::dark() } else { Palette::light() };
        theme::apply(ctx, &self.pal, &self.fonts, self.settings.base_size);
    }

    fn toggle_theme(&mut self, ctx: &Context) {
        self.settings.theme = if self.pal.dark { ThemeChoice::Light } else { ThemeChoice::Dark };
        // An explicit toggle is a real preference; a CLI override was not.
        self.theme_override = None;
        self.apply_theme(ctx);
    }

    /// Gives the Windows title bar the app's theme, which can differ from the system's.
    #[cfg(windows)]
    fn sync_title_bar(&mut self, frame: &eframe::Frame) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if self.title_bar_dark == Some(self.pal.dark) {
            return;
        }
        if let Ok(RawWindowHandle::Win32(w)) = frame.window_handle().map(|h| h.as_raw()) {
            crate::win::set_title_bar_dark(w.hwnd.get(), self.pal.dark);
            self.title_bar_dark = Some(self.pal.dark);
        }
    }

    fn zoom_to(&mut self, ctx: &Context, zoom: f32) {
        let zoom = zoom.clamp(0.5, 3.0);
        ctx.set_zoom_factor(zoom);
        self.settings.zoom = zoom;
    }

    /// The reading column's width as drawn, in points.
    fn column_now(&self) -> f32 {
        if self.settings.full_width {
            self.column_max
        } else {
            self.settings.column_width.min(self.column_max)
        }
    }

    fn set_column_width(&mut self, w: f32) {
        self.settings.full_width = false;
        self.settings.column_width = w.clamp(MIN_COLUMN, MAX_COLUMN);
    }

    fn width_step(&self, wider: bool) -> Option<WidthStep> {
        width_step(self.column_now(), self.settings.full_width, self.column_max, wider)
    }

    fn apply_width_step(&mut self, step: WidthStep) {
        match step {
            WidthStep::Preset(w) => self.set_column_width(w),
            WidthStep::Full => self.settings.full_width = true,
        }
    }

    // ------------------------------------------------------------------
    // Input
    // ------------------------------------------------------------------

    fn handle_input(&mut self, ctx: &Context, frame: &eframe::Frame) {
        // Dropped files
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if let Some(p) = dropped.into_iter().next() {
            self.open_dropped(p, ctx);
        }

        let menu_open = self.menu.is_some();
        if menu_open && ctx.input(|i| i.events.iter().any(|e| matches!(e, Event::Key { pressed: true, .. }))) {
            self.menu = None;
        }

        let typing = ctx.memory(|m| m.focused().is_some());
        let line = self.settings.base_size * 1.55 * 3.0;
        let page = (self.viewport_h * 0.88).max(line);
        let mut delta = 0.0;
        let mut zoom: Option<f32> = None;
        let cur_zoom = ctx.zoom_factor();

        let mut cmds: Vec<Cmd> = Vec::new();
        ctx.input_mut(|i| {
            let zd = i.zoom_delta();
            if (zd - 1.0).abs() > 1e-4 {
                zoom = Some(cur_zoom * zd);
            }
            cmds = key_commands(i, typing);
            // The side buttons on the mouse, as in file managers and browsers.
            if i.pointer.button_pressed(egui::PointerButton::Extra1) {
                cmds.push(Cmd::Back);
            }
            if i.pointer.button_pressed(egui::PointerButton::Extra2) {
                cmds.push(Cmd::Forward);
            }
        });

        for c in &cmds {
            match *c {
                Cmd::Lines(n) => delta += line * n,
                Cmd::Pages(n) => delta += page * n,
                Cmd::ZoomIn => zoom = Some(cur_zoom * 1.1),
                Cmd::ZoomOut => zoom = Some(cur_zoom / 1.1),
                _ => {}
            }
        }
        if delta != 0.0 {
            self.scroll_delta += delta;
        }
        if let Some(z) = zoom {
            self.zoom_to(ctx, z);
        }
        for c in cmds {
            match c {
                Cmd::Lines(_) | Cmd::Pages(_) | Cmd::ZoomIn | Cmd::ZoomOut => {}
                Cmd::Open => self.open_dialog(ctx, frame),
                Cmd::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
                Cmd::Search => {
                    self.search.open = true;
                    self.search.focus_pending = true;
                }
                Cmd::Reload => self.reload(ctx, true),
                Cmd::Back => self.go_history(-1, ctx),
                Cmd::Forward => self.go_history(1, ctx),
                Cmd::Escape => {
                    if menu_open {
                        // Already closed above.
                    } else if self.show_help {
                        self.show_help = false;
                    } else if self.search.open {
                        self.search.clear();
                    }
                }
                Cmd::Top => {
                    self.pending_offset = Some(0.0);
                    self.anchor.clear_hold();
                }
                Cmd::Bottom => {
                    self.pending_offset = Some(f32::MAX);
                    self.anchor.clear_hold();
                }
                Cmd::Next => {
                    if self.search.open {
                        self.search.next();
                        if let Some(b) = self.search.current_block() {
                            self.jump_to_block(b, ctx);
                        }
                    }
                }
                Cmd::Prev => {
                    if self.search.open {
                        self.search.prev();
                        if let Some(b) = self.search.current_block() {
                            self.jump_to_block(b, ctx);
                        }
                    }
                }
                Cmd::Outline => self.settings.show_outline = !self.settings.show_outline,
                Cmd::Files => self.settings.show_files = !self.settings.show_files,
                Cmd::Theme => self.toggle_theme(ctx),
                Cmd::Width => self.settings.full_width = !self.settings.full_width,
                // From the width as drawn, so the first press out of full width (or a
                // column the window caps) narrows it.
                Cmd::Narrower => self.set_column_width(self.column_now() - 60.0),
                Cmd::Wider => {
                    if !self.settings.full_width {
                        self.set_column_width(self.settings.column_width + 60.0);
                    }
                }
                Cmd::Help => self.show_help = !self.show_help,
                Cmd::ZoomReset => self.zoom_to(ctx, 1.0),
            }
        }
    }

    fn poll_channels(&mut self, ctx: &Context) {
        if let Some(rx) = &self.watch_rx {
            let (mut changed, mut missed) = (false, false);
            while let Ok(ev) = rx.try_recv() {
                match ev {
                    WatchEvent::Changed => changed = true,
                    WatchEvent::Missed => missed = true,
                }
            }
            if missed {
                if let Some(path) = self.path.clone() {
                    log::debug!("{} changed without a watcher event; watching again", path.display());
                    let stamp = *self.seen.lock();
                    self.setup_watcher(&path, stamp, ctx);
                }
            }
            if changed || missed {
                let at = Instant::now() + Duration::from_millis(60);
                self.reload_at = Some(at);
                wake_after(ctx, Duration::from_millis(70));
            }
        }
        if let Some(t) = self.reload_at {
            if Instant::now() >= t {
                self.reload_at = None;
                self.reload(ctx, true);
            } else {
                wake_after(ctx, t - Instant::now() + Duration::from_millis(5));
            }
        }
        if self.hl.poll() {
            ctx.request_repaint();
        }
        self.diagrams.poll();
        while let Ok(msg) = self.launch_rx.try_recv() {
            self.set_status(msg);
        }
        if let Some(rx) = &self.dialog_rx {
            match rx.try_recv() {
                Ok(Some(p)) => {
                    self.dialog_rx = None;
                    self.load_path(p, true, ctx);
                }
                Ok(None) | Err(mpsc::TryRecvError::Disconnected) => self.dialog_rx = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.files_rx {
            match rx.try_recv() {
                Ok(files) => {
                    for f in &files {
                        fonts::add_script_fallbacks(ctx, &self.fonts, &f.to_string_lossy());
                    }
                    self.files = files;
                    self.files_rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.files_rx = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.settings.theme == ThemeChoice::System {
            let sys = ctx.system_theme().map(|t| t == egui::Theme::Dark);
            if sys.is_some() && sys != self.system_dark {
                self.system_dark = sys;
                self.apply_theme(ctx);
            }
        }
        // Keep zoom in sync when egui handled Ctrl+/- itself.
        let z = ctx.zoom_factor();
        if (z - self.settings.zoom).abs() > 1e-4 {
            self.settings.zoom = z;
        }
        if let Some((_, t)) = &self.status {
            if t.elapsed() > Duration::from_secs(4) {
                self.status = None;
            } else {
                ctx.request_repaint_after(Duration::from_millis(250));
                wake_after(ctx, Duration::from_millis(300));
            }
        }
    }

    // ------------------------------------------------------------------
    // Panels
    // ------------------------------------------------------------------

    fn panel_frame(&self) -> Frame {
        Frame::new().fill(self.pal.panel_bg).inner_margin(Margin::symmetric(12, 12))
    }

    fn show_outline(&mut self, ui: &mut Ui) {
        let Some(doc) = self.doc.clone() else { return };
        let headings: Vec<(usize, &crate::document::HeadingRef)> =
            doc.headings.iter().enumerate().filter(|(_, h)| !h.nested).collect();
        if headings.is_empty() {
            return;
        }
        let min_level = headings.iter().map(|(_, h)| h.level).min().unwrap_or(1);
        let base = self.settings.base_size;
        let pal = self.pal;
        Panel::left("outline")
            .resizable(true)
            .default_size(280.0)
            .size_range(160.0..=700.0)
            .frame(self.panel_frame())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("OUTLINE").size(base * 0.72).color(pal.text_dim));
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(format!("{}", headings.len())).size(base * 0.72).color(pal.text_dim));
                    });
                });
                ui.add_space(6.0);
                let mut clicked: Option<usize> = None;
                ScrollArea::vertical().id_salt("outline-scroll").auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    for (hi, h) in &headings {
                        let depth = h.level.saturating_sub(min_level) as f32;
                        let indent = depth * base * 0.85;
                        let is_current = self.current_heading == Some(*hi);
                        let size = match h.level.saturating_sub(min_level) {
                            0 => base * 0.95,
                            1 => base * 0.9,
                            _ => base * 0.85,
                        };
                        let color = if is_current {
                            pal.heading
                        } else if depth <= 1.0 {
                            pal.text
                        } else {
                            pal.text_dim
                        };
                        let mut text = RichText::new(&h.text).size(size).color(color);
                        if is_current || depth == 0.0 {
                            text = text.strong();
                        }
                        let bg_idx = ui.painter().add(Shape::Noop);
                        let avail = ui.available_rect_before_wrap();
                        let row = ui
                            .scope_builder(
                                UiBuilder::new().max_rect(Rect::from_min_max(
                                    pos2(avail.left() + indent + 8.0, avail.top() + 3.0),
                                    pos2(avail.right() - 4.0, avail.bottom()),
                                )),
                                |ui| ui.add(Label::new(text).sense(Sense::click()).selectable(false).truncate()),
                            )
                            .inner;
                        let row_rect = Rect::from_min_max(pos2(avail.left(), row.rect.top() - 3.0), pos2(avail.right(), row.rect.bottom() + 3.0));
                        ui.allocate_rect(Rect::from_min_max(pos2(avail.left(), avail.top()), pos2(avail.right(), row.rect.bottom() + 3.0)), Sense::hover());
                        if is_current {
                            ui.painter().set(bg_idx, Shape::rect_filled(row_rect, CornerRadius::same(4), pal.sidebar_current));
                            ui.painter().rect_filled(Rect::from_min_size(row_rect.left_top(), vec2(3.0, row_rect.height())), CornerRadius::same(2), pal.accent);
                        } else if row.hovered() {
                            ui.painter().set(bg_idx, Shape::rect_filled(row_rect, CornerRadius::same(4), pal.table_stripe));
                        }
                        if row.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if row.clicked() {
                            clicked = Some(h.block);
                        }
                        if self.outline_scroll_to == Some(*hi) {
                            row.scroll_to_me(Some(Align::Center));
                        }
                    }
                });
                self.outline_scroll_to = None;
                if let Some(b) = clicked {
                    self.jump_to_block(b, ui.ctx());
                }
            });
    }

    fn show_files_panel(&mut self, ui: &mut Ui, ctx: &Context) {
        let Some(dir) = self.files_dir.clone() else { return };
        let open_dir = self.path.as_deref().and_then(Path::parent).map(Path::to_path_buf);
        // A lone file has no use for the sidebar, unless a link or the sidebar led
        // somewhere and it shows the way back.
        if self.files.len() < 2 && self.history.len() < 2 && open_dir.as_ref() == Some(&dir) {
            return;
        }
        let base = self.settings.base_size;
        let pal = self.pal;
        let current = self.path.clone();
        let mut clicked: Option<PathBuf> = None;
        let mut show_dir: Option<PathBuf> = None;
        Panel::right("files")
            .resizable(true)
            .default_size(260.0)
            .size_range(160.0..=700.0)
            .frame(self.panel_frame())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("FILES").size(base * 0.72).color(pal.text_dim));
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let up = dir.parent();
                        let up_hint = up.map(|u| format!("Show the folder above: {}", folder_name(u))).unwrap_or_default();
                        if ui.add_enabled(up.is_some(), flat_button(arrow("↑", base * 0.78, pal.text_dim))).on_hover_text(up_hint).clicked() {
                            show_dir = up.map(Path::to_path_buf);
                        }
                        // The folder shown, with a menu of the open file's folder and those
                        // above it, to go back down after going up.
                        let resp = ui
                            .add(flat_button(RichText::new(folder_name(&dir)).size(base * 0.72).color(pal.text_dim)).truncate())
                            .on_hover_text(format!("{}\nClick to show another folder", links::display_path(&dir)));
                        Popup::menu(&resp).show(|ui| {
                            for d in open_dir.iter().flat_map(|d| d.ancestors()) {
                                let here = Some(d) == open_dir.as_deref();
                                let item = Button::selectable(d == dir, folder_name(d)).right_text(if here { "open file" } else { "" });
                                if ui.add(item).on_hover_text(links::display_path(d)).clicked() {
                                    show_dir = Some(d.to_path_buf());
                                }
                            }
                        });
                    });
                });
                ui.add_space(6.0);
                ScrollArea::vertical().id_salt("files-scroll").auto_shrink([false, false]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    for f in &self.files {
                        let rel = f.strip_prefix(&dir).unwrap_or(f);
                        let is_current = current.as_ref() == Some(f);
                        let depth = rel.components().count().saturating_sub(1) as f32;
                        let label = rel.to_string_lossy().to_string();
                        let color = if is_current { pal.heading } else if depth == 0.0 { pal.text } else { pal.text_dim };
                        let mut text = RichText::new(label).size(base * 0.88).color(color);
                        if is_current {
                            text = text.strong();
                        }
                        let bg_idx = ui.painter().add(Shape::Noop);
                        let avail = ui.available_rect_before_wrap();
                        let row = ui
                            .scope_builder(
                                UiBuilder::new().max_rect(Rect::from_min_max(pos2(avail.left() + 8.0, avail.top() + 3.0), pos2(avail.right() - 4.0, avail.bottom()))),
                                |ui| ui.add(Label::new(text).sense(Sense::click()).selectable(false).truncate()),
                            )
                            .inner;
                        let row_rect = Rect::from_min_max(pos2(avail.left(), row.rect.top() - 3.0), pos2(avail.right(), row.rect.bottom() + 3.0));
                        ui.allocate_rect(Rect::from_min_max(pos2(avail.left(), avail.top()), pos2(avail.right(), row.rect.bottom() + 3.0)), Sense::hover());
                        if is_current {
                            ui.painter().set(bg_idx, Shape::rect_filled(row_rect, CornerRadius::same(4), pal.sidebar_current));
                        } else if row.hovered() {
                            ui.painter().set(bg_idx, Shape::rect_filled(row_rect, CornerRadius::same(4), pal.table_stripe));
                        }
                        if row.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        if row.clicked() && !is_current {
                            clicked = Some(f.clone());
                        }
                    }
                });
            });
        if let Some(d) = show_dir {
            self.set_files_dir(Some(d), ctx);
        }
        if let Some(p) = clicked {
            self.load_path(p, true, ctx);
        }
    }

    fn show_search_bar(&mut self, ui: &mut Ui, ctx: &Context) {
        let base = self.settings.base_size;
        let pal = self.pal;
        let mut do_next = false;
        let mut do_prev = false;
        let mut close = false;
        Panel::top("search")
            .resizable(false)
            .frame(Frame::new().fill(pal.panel_bg).inner_margin(Margin::symmetric(14, 8)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Find").color(pal.text_dim));
                    let out = TextEdit::singleline(&mut self.search.query)
                        .hint_text("Search this document…")
                        .desired_width((base * 24.0).min(ui.available_width() * 0.5))
                        .show(ui);
                    let resp = out.response;
                    if self.search.focus_pending {
                        resp.request_focus();
                        // Select the previous query so that typing replaces it.
                        let mut state = out.state;
                        let end = CCursor::new(self.search.query.chars().count());
                        state.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(0), end)));
                        state.store(ctx, resp.id);
                        self.search.focus_pending = false;
                    }
                    if resp.lost_focus() && ctx.input(|i| i.key_pressed(Key::Enter)) {
                        if ctx.input(|i| i.modifiers.shift) {
                            do_prev = true;
                        } else {
                            do_next = true;
                        }
                        resp.request_focus();
                    }
                    let n = self.search.matches.len();
                    let info = if self.search.query.trim().is_empty() {
                        String::new()
                    } else if n == 0 {
                        "No matches".to_owned()
                    } else {
                        format!("{} of {}", self.search.current + 1, n)
                    };
                    ui.label(RichText::new(info).color(pal.text_dim));
                    if ui.add_enabled(n > 0, egui::Button::new("Prev")).on_hover_text("Shift+Enter or N").clicked() {
                        do_prev = true;
                    }
                    if ui.add_enabled(n > 0, egui::Button::new("Next")).on_hover_text("Enter or n").clicked() {
                        do_next = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Close").on_hover_text("Esc").clicked() {
                            close = true;
                        }
                    });
                });
            });
        if close {
            self.search.clear();
        }
        if do_next {
            self.search.next();
            if let Some(b) = self.search.current_block() {
                self.jump_to_block(b, ctx);
            }
        }
        if do_prev {
            self.search.prev();
            if let Some(b) = self.search.current_block() {
                self.jump_to_block(b, ctx);
            }
        }
    }

    fn show_status_bar(&mut self, ui: &mut Ui) {
        let base = self.settings.base_size;
        let pal = self.pal;
        let small = base * 0.78;
        let mut nav = None;
        Panel::bottom("status")
            .resizable(false)
            .show_separator_line(true)
            .frame(Frame::new().fill(pal.panel_bg).inner_margin(Margin::symmetric(12, 5)))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.history.len() > 1 {
                        nav = self.history_buttons(ui, small);
                        ui.add_space(6.0);
                    }
                    let path_text = if self.stdin_mode {
                        "stdin".to_owned()
                    } else {
                        self.path.as_deref().map(links::display_path).unwrap_or_default()
                    };
                    ui.add(Label::new(RichText::new(path_text).size(small).color(pal.text_dim)).truncate());
                    if let Some((msg, _)) = &self.status {
                        ui.add_space(12.0);
                        ui.label(RichText::new(msg).size(small).color(pal.accent));
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        let zoom = (self.settings.zoom * 100.0).round() as i32;
                        // The true offset and height: content drawn shifted is not yet in egui's offset.
                        let shift = self.anchor.shift;
                        let pct = if self.content_h + shift > self.viewport_h + 1.0 {
                            ((self.scroll_offset + shift) / (self.content_h + shift - self.viewport_h) * 100.0).clamp(0.0, 100.0).round() as i32
                        } else {
                            100
                        };
                        ui.label(RichText::new(format!("{zoom}%")).size(small).color(pal.text_dim)).on_hover_text(format!("Zoom ({CMD} +/−/0, {CMD}+scroll)"));
                        ui.separator();
                        ui.label(RichText::new(format!("{pct}%")).size(small).color(pal.text_dim)).on_hover_text("Scroll position");
                        if let Some(doc) = &self.doc {
                            ui.separator();
                            let words = doc.word_count;
                            let mins = (words as f32 / 230.0).ceil().max(1.0) as usize;
                            ui.label(RichText::new(format!("{words} words · {mins} min")).size(small).color(pal.text_dim));
                        }
                        ui.separator();
                        self.width_controls(ui, small);
                        ui.separator();
                        ui.label(RichText::new("? help").size(small).color(pal.text_dim));
                    });
                });
            });
        if let Some(delta) = nav {
            let ctx = ui.ctx().clone();
            self.go_history(delta, &ctx);
        }
    }

    /// ← → in the status bar, for going back to the file a link came from. Returns the
    /// step clicked.
    fn history_buttons(&self, ui: &mut Ui, size: f32) -> Option<i32> {
        let mut nav = None;
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            for (delta, glyph, verb, keys) in [(-1, "←", "Back", "Alt+Left, Backspace or the mouse back button"), (1, "→", "Forward", "Alt+Right")] {
                let to = self.history_neighbour(delta);
                let resp = ui.add_enabled(to.is_some(), flat_button(arrow(glyph, size, self.pal.text_dim)));
                if let Some(to) = to {
                    // The folder too: links often lead to another README.md.
                    let shown = match to.parent() {
                        Some(dir) => format!("{}/{}", folder_name(dir), to.file_name().unwrap_or_default().to_string_lossy()),
                        None => links::display_path(to),
                    };
                    if resp.on_hover_text(format!("{verb} to {shown} ({keys})")).clicked() {
                        nav = Some(delta as i32);
                    }
                }
            }
        });
        nav
    }

    /// `− 860pt +` in the status bar, which runs right to left: the buttons step through
    /// the width presets, and the width opens a menu of them.
    fn width_controls(&mut self, ui: &mut Ui, size: f32) {
        let pal = self.pal;
        let flat = |text: &str| flat_button(RichText::new(text).size(size).color(pal.text_dim));
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let wider = self.width_step(true);
            if ui.add_enabled(wider.is_some(), flat("+")).on_hover_text("Wider (] in small steps)").clicked() {
                if let Some(step) = wider {
                    self.apply_width_step(step);
                }
            }
            let label = if self.settings.full_width { "full width".to_owned() } else { format!("{:.0}pt", self.column_now()) };
            // Wide enough for "full width", so the − button stays put while stepping.
            let full_w = ui.painter().layout_no_wrap("full width".to_owned(), egui::FontId::proportional(size), pal.text_dim).size().x;
            let min_w = full_w + 2.0 * ui.spacing().button_padding.x;
            let resp = ui
                .add(flat(&label).min_size(vec2(min_w, 0.0)))
                .on_hover_text("Reading column width: click for presets (w toggles full width)");
            Popup::menu(&resp).show(|ui| {
                for (name, w) in WIDTH_PRESETS {
                    let on = !self.settings.full_width && (self.settings.column_width - w).abs() < 0.5;
                    let fits = w <= self.column_max + 1.0;
                    let item = Button::selectable(on, name).right_text(format!("{w:.0}pt"));
                    let resp = ui.add_enabled(fits || on, item).on_disabled_hover_text("Wider than the window");
                    if resp.clicked() {
                        self.set_column_width(w);
                    }
                }
                ui.separator();
                if ui.add(Button::selectable(self.settings.full_width, "Full width").right_text("w")).clicked() {
                    self.settings.full_width = true;
                }
            });
            let narrower = self.width_step(false);
            if ui.add_enabled(narrower.is_some(), flat("−")).on_hover_text("Narrower ([ in small steps)").clicked() {
                if let Some(step) = narrower {
                    self.apply_width_step(step);
                }
            }
        });
    }

    fn show_help(&mut self, ctx: &Context) {
        if !self.show_help {
            return;
        }
        let pal = self.pal;
        let base = self.settings.base_size;
        let mut open = self.show_help;
        egui::Window::new(RichText::new("Keyboard shortcuts").size(base * 1.1))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .frame(Frame::window(&ctx.global_style()).fill(pal.panel_bg).inner_margin(Margin::same(18)))
            .show(ctx, |ui| {
                let open_key = format!("{CMD}+O");
                let find_key = format!("{CMD}+F  /");
                let zoom_key = format!("{CMD} + −  0   {CMD}+scroll");
                let quit_key = format!("q  {CMD}+Q");
                let rows: Vec<(&str, &str)> = vec![
                    (&open_key, "Open file"),
                    (&find_key, "Find in document"),
                    ("Enter  n  N", "Next / previous match"),
                    ("j k  ↓ ↑", "Scroll by line"),
                    ("Space  PgDn / Shift+Space  PgUp", "Scroll by page"),
                    ("d  u", "Scroll half a page"),
                    ("g  G  Home  End", "Top / bottom"),
                    ("o", "Toggle outline sidebar"),
                    ("f", "Toggle files sidebar"),
                    ("t", "Toggle dark / light theme"),
                    ("w   [  ]", "Full-width column / narrower / wider"),
                    (&zoom_key, "Zoom"),
                    ("Alt+←  Backspace  /  Alt+→", "Back / forward (also the arrows in the status bar)"),
                    ("Mouse side buttons", "Back / forward"),
                    ("r  F5", "Reload (files also reload automatically)"),
                    ("Click link", "Open in browser, or follow to another .md"),
                    ("Hover code block", "Copy button"),
                    ("Right-click", "Copy selection, open or copy link"),
                    ("Click width in status bar", "Column width presets (− + step through them)"),
                    ("↑ or folder name in Files", "Show the folder above, or pick one"),
                    (&quit_key, "Quit"),
                ];
                egui::Grid::new("help-grid").num_columns(2).spacing([24.0, 6.0]).show(ui, |ui| {
                    for (k, v) in &rows {
                        ui.label(RichText::new(*k).monospace().color(pal.accent));
                        ui.label(RichText::new(*v).color(pal.text));
                        ui.end_row();
                    }
                });
                ui.add_space(10.0);
                ui.label(
                    RichText::new(format!("Fonts: {} · {}", self.fonts.sans_name, self.fonts.mono_name))
                        .size(base * 0.8)
                        .color(pal.text_dim),
                );
            });
        self.show_help = open;
    }

    // ------------------------------------------------------------------
    // Reading column
    // ------------------------------------------------------------------

    fn show_content(&mut self, ui: &mut Ui, ctx: &Context) {
        let pal = self.pal;
        let base = self.settings.base_size;

        let Some(doc) = self.doc.clone() else {
            self.content_rect = Rect::NOTHING;
            self.menu_request = None;
            self.show_welcome(ui);
            return;
        };

        // Search bookkeeping
        if self.search.needs_run(self.gen) {
            self.search.run(&doc, self.gen);
            if let Some(b) = self.search.current_block() {
                self.jump_to_block(b, ctx);
            }
        }
        if let Some(anchor) = self.pending_anchor.take() {
            if let Some(b) = doc.find_slug(&anchor) {
                self.jump_to_block(b, ctx);
            }
        }

        let avail_w = ui.available_width();
        let margin = base * 2.0;
        let col_w = if self.settings.full_width {
            (avail_w - 2.0 * margin).max(200.0)
        } else {
            (avail_w - 2.0 * margin).min(self.settings.column_width).max(200.0)
        };
        self.column_max = (avail_w - 2.0 * margin).max(200.0);
        let left_pad = ((avail_w - col_w) / 2.0).max(margin);
        let key = ((col_w * 2.0).round() as i32, (base * 100.0) as i32);
        let key_changed = key != self.layout_key;

        let sa = ScrollArea::vertical()
            .id_salt("content")
            .auto_shrink([false, false])
            .scroll_source(ScrollSource { scroll_bar: true, drag: DragScroll::OnTouch, mouse_wheel: true });
        // Ahead of the layout key branch: a relayout reads which blocks were measured before it.
        let fin = FrameIn {
            scroll_offset: self.scroll_offset,
            content_h: self.content_h,
            viewport_h: self.viewport_h,
            same_doc: self.content_gen == self.gen,
            n: doc.blocks.len(),
            key_changed,
            avail_w,
            base,
        };
        let sa = self.anchor.before(ui, sa, &mut self.pending_offset, fin, &self.measures);
        if key_changed {
            self.layout_key = key;
            for m in &mut self.measures {
                m.measured = false;
            }
            self.measure_all = doc.blocks.len() <= 600;
        }

        let scroll_delta = std::mem::take(&mut self.scroll_delta);
        let measure_all = std::mem::take(&mut self.measure_all);
        let base_dir = self.path.as_ref().and_then(|p| p.parent().map(Path::to_path_buf));
        let search = if self.search.open && !self.search.matches.is_empty() { Some(&self.search) } else { None };
        let hovered_link = self.hovered_link;

        let mut rc = RenderCtx::new(
            &pal,
            &self.fonts,
            base,
            base_dir.as_deref(),
            self.gen,
            &mut self.hl,
            &mut self.diagrams,
            search,
            hovered_link,
        );
        let measures = &mut self.measures;
        let block_tops = &mut self.block_tops;
        let gap = rc.gap();
        let anchor = &mut self.anchor;

        let out = sa.show_viewport(ui, |ui, viewport| {
            if scroll_delta != 0.0 {
                ui.scroll_with_delta(vec2(0.0, -scroll_delta));
            }
            let visible = ui.clip_rect().expand2(vec2(0.0, ui.clip_rect().height().max(400.0)));
            rc.wide = egui::Rangef::new(ui.max_rect().left() + margin, ui.max_rect().right() - margin);
            let col_x = ui.max_rect().left() + left_pad;
            anchor.column(ui, viewport, col_x, col_w, base, gap, |ui, anchor| {
                let n = doc.blocks.len();
                for (i, block) in doc.blocks.iter().enumerate() {
                    anchor.at_block(ui, i);
                    let top = ui.cursor().top();
                    block_tops[i] = top;
                    let m = measures[i];
                    let est = if m.measured { m.height } else { estimate_height(block, col_w, base) + gap };
                    let in_view = top <= visible.max.y && top + est >= visible.min.y;
                    if lay_out_block(m, in_view, measure_all, anchor.holds(i), i + 1 == n) {
                        rc.pending = false;
                        rc.show_block(ui, block);
                        if i + 1 < n {
                            ui.add_space(gap);
                        }
                        let h = ui.cursor().top() - top;
                        measures[i] = Measure { height: h, measured: true, pending: rc.pending };
                    } else {
                        ui.allocate_space(vec2(col_w, est));
                    }
                }
            });
        });
        self.anchor.after(ctx, &out, scroll_delta, &mut self.block_tops, &self.measures, base);

        let actions = std::mem::take(&mut rc.actions);
        let new_hovered = rc.new_hovered_link;
        let hover = std::mem::take(&mut rc.hover);
        drop(rc);

        self.content_rect = out.inner_rect;
        if let Some(pos) = self.menu_request.take() {
            self.menu = Some(ContextMenu { pos, link: hover.link, image: hover.image, code: hover.code, rect: Rect::NOTHING, copy_rect: None });
            // The swallowed press would have taken focus from the search box.
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
            ctx.request_repaint();
        }

        if new_hovered != self.hovered_link {
            self.hovered_link = new_hovered;
            ctx.request_repaint();
        }
        self.scroll_offset = out.state.clamped_offset().y;
        self.viewport_h = out.inner_rect.height();
        self.content_h = out.content_size.y;
        self.content_gen = self.gen;

        // Current heading for the outline
        let top_y = out.inner_rect.top() + base * 1.5;
        let mut current = None;
        for (hi, h) in doc.headings.iter().enumerate() {
            if h.nested {
                continue;
            }
            if let Some(&t) = self.block_tops.get(h.block) {
                if t <= top_y {
                    current = Some(hi);
                } else {
                    break;
                }
            }
        }
        if current.is_none() && doc.headings.iter().any(|h| !h.nested) {
            current = doc.headings.iter().position(|h| !h.nested);
        }
        if current != self.current_heading {
            self.current_heading = current;
            self.outline_scroll_to = current;
        }

        for a in actions {
            match a {
                Action::OpenLink(url) => self.open_link(&url, ctx),
                Action::Status(s) => self.set_status(s),
            }
        }
    }

    fn handle_screenshot(&mut self, ctx: &Context) {
        let Some(path) = self.screenshot.clone() else { return };
        let shot = self.screenshot_taken.as_ref().and_then(|taken| taken.lock().take());
        if let Some(img) = shot {
            let [w, h] = img.size;
            crate::cli_console(true);
            if self.screenshot_unsettled {
                eprintln!("screenshot taken before the page had finished drawing ({} s past the delay)", SCREENSHOT_WAIT_MAX.as_secs());
            }
            match image::save_buffer(&path, img.as_raw(), w as u32, h as u32, image::ColorType::Rgba8) {
                Ok(()) => eprintln!("screenshot saved to {} ({w}x{h})", path.display()),
                Err(e) => eprintln!("screenshot failed: {e}"),
            }
            ctx.send_viewport_cmd(ViewportCommand::Close);
        } else {
            // Diagrams are laid out and drawn, and code highlighted, on worker threads, and
            // images read on egui's loader thread (a block that drew one still loading is
            // re-measured every frame until it arrives, so its flag clears then). A jump or a
            // restored place is held until the reader scrolls, so the hold never ends here, but
            // a correction it made is drawn shifted until it moves into the scroll offset: the
            // page is settled once it has. eframe captures the frame painted after the request,
            // and this runs at the end of the frame, after show_content queued this frame's
            // work, so one settled frame is enough. Without a document nothing is loading.
            // A block's pending flag covers its diagrams as well as its images.
            let blocks_loading = self.doc.is_some() && self.measures.iter().any(|m| m.pending);
            let settled = !self.diagrams.busy() && !self.hl.busy() && !blocks_loading && self.anchor.shift == 0.0 && self.pending_offset.is_none();
            if self.screenshot_taken.is_none() && screenshot_due(self.started.elapsed(), Duration::from_millis(self.screenshot_delay_ms), settled) {
                if !settled {
                    log::warn!("taking the screenshot before the page has finished drawing");
                    self.screenshot_unsettled = true;
                }
                let taken = Arc::new(egui::mutex::Mutex::new(None));
                self.screenshot_taken = Some(taken.clone());
                log::debug!("requesting screenshot at frame {}", self.frames);
                ctx.request_screenshot(move |image| *taken.lock() = Some(image));
            }
            // Keep frames coming so the screenshot is picked up once taken.
            ctx.request_repaint();
        }
    }

    fn show_welcome(&mut self, ui: &mut Ui) {
        let pal = self.pal;
        let base = self.settings.base_size;
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            ui.label(RichText::new("mdread").size(base * 2.4).color(pal.heading).strong());
            ui.add_space(base * 0.6);
            if let Some(err) = &self.error {
                ui.label(RichText::new(err).color(pal.caution).size(base));
                ui.add_space(base);
            }
            ui.label(RichText::new(format!("Drop a Markdown file here, or press {CMD}+O to open one.")).color(pal.text_dim).size(base * 1.05));
            ui.add_space(base * 0.3);
            ui.label(RichText::new("mdread README.md    ·    cat notes.md | mdread -").monospace().color(pal.text_dim).size(base * 0.9));
            ui.add_space(base * 1.5);
            ui.label(RichText::new("Press ? for keyboard shortcuts").color(pal.text_dim).size(base * 0.9));
        });
    }
}

/// How long `--screenshot` waits past its delay for diagrams, images and highlighting. It
/// must not wait for ever: the 10 s layout limit is per diagram and the worker lays them
/// out one at a time, so a page with several slow ones takes a multiple of it, a worker
/// that died leaves its diagrams pending for good, and a remote image can be slow.
const SCREENSHOT_WAIT_MAX: Duration = Duration::from_secs(20);

/// Whether to take the `--screenshot` now, `elapsed` after launch: once the `delay` has
/// passed and the page is `settled`, or `SCREENSHOT_WAIT_MAX` after the delay regardless.
fn screenshot_due(elapsed: Duration, delay: Duration, settled: bool) -> bool {
    elapsed >= delay && (settled || elapsed >= delay + SCREENSHOT_WAIT_MAX)
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.frames += 1;
        if self.frames == 1 {
            log::info!("first frame {:.1} ms after launch", self.started.elapsed().as_secs_f64() * 1000.0);
        }
        let frame_t0 = Instant::now();
        self.poll_channels(&ctx);
        self.handle_input(&ctx, frame);
        self.add_ui_script_fallbacks(&ctx);
        #[cfg(windows)]
        self.sync_title_bar(frame);

        self.show_status_bar(ui);
        if self.search.open {
            self.show_search_bar(ui, &ctx);
        }
        if self.settings.show_outline {
            self.show_outline(ui);
        }
        if self.settings.show_files {
            self.show_files_panel(ui, &ctx);
        }
        let pal = self.pal;
        egui::CentralPanel::default()
            .frame(Frame::new().fill(pal.bg))
            .show(ui, |ui| {
                self.show_content(ui, &ctx);
            });
        self.show_context_menu(&ctx);
        self.show_help(&ctx);
        // Last, so that it sees the diagram drawing and highlighting show_content asked for.
        self.handle_screenshot(&ctx);
        log::trace!(
            "frame {} at {:.0} ms took {:.2} ms",
            self.frames,
            self.started.elapsed().as_secs_f64() * 1000.0,
            frame_t0.elapsed().as_secs_f64() * 1000.0
        );
    }

    /// Runs before egui sees the frame's input. Right-presses over the reading column
    /// are swallowed and turned into a context-menu request, because egui collapses
    /// the label text selection on any press over a label. A press on the menu's
    /// "Copy" item becomes an `Event::Copy`, which is how egui copies that selection.
    fn raw_input_hook(&mut self, _ctx: &Context, raw: &mut egui::RawInput) {
        let mut copy = false;
        raw.events.retain(|e| self.keep_event(e, &mut copy));
        if copy {
            raw.events.push(Event::Copy);
            self.set_status("Copied selection to clipboard");
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let mut s = self.settings.clone();
        if self.theme_override.is_some() {
            s.theme = self.saved_theme;
        }
        eframe::set_value(storage, eframe::APP_KEY, &s);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.pal.bg.to_normalized_gamma_f32()
    }
}

/// What the keyboard (or a mouse button) asks for.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Cmd {
    Open,
    Quit,
    Search,
    Reload,
    Back,
    Forward,
    Escape,
    Top,
    Bottom,
    Next,
    Prev,
    Outline,
    Files,
    Theme,
    Width,
    Narrower,
    Wider,
    Help,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    /// Scroll by this many lines (negative: up).
    Lines(f32),
    /// Scroll by this many pages.
    Pages(f32),
}

/// Whether the held modifiers fit a shortcut. As in egui, an extra Shift is fine, since
/// some layouts need it to type '?', '/' or '+'; so a binding with Shift must be checked
/// before the same key without it. On Windows an extra Alt is not: Alt+letter is a menu
/// chord there, and Ctrl+Alt stands in for AltGr (Ctrl+Alt+Q types '@' on a German
/// keyboard), so neither may reach the plain or Ctrl shortcuts. Real AltGr presses come
/// without Ctrl and Alt: winit clears them.
fn modifiers_fit(held: Modifiers, shortcut: Modifiers) -> bool {
    held.matches_logically(shortcut) && (!cfg!(windows) || held.alt == shortcut.alt)
}

/// `InputState::consume_key`, with [`modifiers_fit`] deciding which presses count.
fn consume(i: &mut egui::InputState, shortcut: Modifiers, key: Key) -> bool {
    let before = i.events.len();
    i.events.retain(|e| {
        !matches!(e, egui::Event::Key { key: k, pressed: true, modifiers, .. } if *k == key && modifiers_fit(*modifiers, shortcut))
    });
    i.events.len() != before
}

/// `InputState::key_pressed` for the navigation keys, which work with any of Ctrl and
/// Shift held, but on Windows not with Alt.
fn pressed(i: &egui::InputState, key: Key) -> bool {
    i.events.iter().any(|e| matches!(e, egui::Event::Key { key: k, pressed: true, modifiers, .. } if *k == key && !(cfg!(windows) && modifiers.alt)))
}

/// Reads the shortcuts pressed this frame. Single-key shortcuts are off while a text
/// field has focus (`typing`).
fn key_commands(i: &mut egui::InputState, typing: bool) -> Vec<Cmd> {
    let mut cmds = Vec::new();
    let cmd = Modifiers::COMMAND;
    let none = Modifiers::NONE;
    let shift = Modifiers::SHIFT;
    if consume(i, cmd, Key::O) {
        cmds.push(Cmd::Open);
    }
    if consume(i, cmd, Key::Q) || consume(i, cmd, Key::W) {
        cmds.push(Cmd::Quit);
    }
    if consume(i, cmd, Key::F) {
        cmds.push(Cmd::Search);
    }
    if consume(i, cmd, Key::R) || consume(i, none, Key::F5) {
        cmds.push(Cmd::Reload);
    }
    if consume(i, Modifiers::ALT, Key::ArrowLeft) {
        cmds.push(Cmd::Back);
    }
    if consume(i, Modifiers::ALT, Key::ArrowRight) {
        cmds.push(Cmd::Forward);
    }
    // Shifted bindings go first everywhere: the plain one would also take Shift+key.
    if consume(i, shift, Key::F3) {
        cmds.push(Cmd::Prev);
    }
    if consume(i, none, Key::F3) {
        cmds.push(Cmd::Next);
    }
    if consume(i, none, Key::F1) {
        cmds.push(Cmd::Help);
    }
    if i.key_pressed(Key::Escape) {
        cmds.push(Cmd::Escape);
    }
    if typing {
        return cmds;
    }
    if consume(i, none, Key::J) || pressed(i, Key::ArrowDown) {
        cmds.push(Cmd::Lines(1.0));
    }
    if consume(i, none, Key::K) || pressed(i, Key::ArrowUp) {
        cmds.push(Cmd::Lines(-1.0));
    }
    if consume(i, shift, Key::Space) || pressed(i, Key::PageUp) {
        cmds.push(Cmd::Pages(-1.0));
    }
    if consume(i, none, Key::Space) || pressed(i, Key::PageDown) {
        cmds.push(Cmd::Pages(1.0));
    }
    if consume(i, none, Key::D) {
        cmds.push(Cmd::Pages(0.5));
    }
    if consume(i, none, Key::U) {
        cmds.push(Cmd::Pages(-0.5));
    }
    if consume(i, shift, Key::G) || pressed(i, Key::End) {
        cmds.push(Cmd::Bottom);
    }
    if consume(i, none, Key::G) || pressed(i, Key::Home) {
        cmds.push(Cmd::Top);
    }
    if consume(i, none, Key::Slash) {
        cmds.push(Cmd::Search);
    }
    if consume(i, shift, Key::N) {
        cmds.push(Cmd::Prev);
    }
    if consume(i, none, Key::N) {
        cmds.push(Cmd::Next);
    }
    if consume(i, none, Key::O) {
        cmds.push(Cmd::Outline);
    }
    if consume(i, none, Key::F) {
        cmds.push(Cmd::Files);
    }
    if consume(i, none, Key::T) {
        cmds.push(Cmd::Theme);
    }
    if consume(i, none, Key::W) {
        cmds.push(Cmd::Width);
    }
    if consume(i, none, Key::OpenBracket) {
        cmds.push(Cmd::Narrower);
    }
    if consume(i, none, Key::CloseBracket) {
        cmds.push(Cmd::Wider);
    }
    if consume(i, none, Key::Q) {
        cmds.push(Cmd::Quit);
    }
    if consume(i, none, Key::R) {
        cmds.push(Cmd::Reload);
    }
    if consume(i, shift, Key::Questionmark) || consume(i, none, Key::Questionmark) {
        cmds.push(Cmd::Help);
    }
    if consume(i, none, Key::Backspace) {
        cmds.push(Cmd::Back);
    }
    if consume(i, none, Key::Plus) || consume(i, none, Key::Equals) || consume(i, shift, Key::Equals) || consume(i, shift, Key::Plus) {
        cmds.push(Cmd::ZoomIn);
    }
    if consume(i, none, Key::Minus) {
        cmds.push(Cmd::ZoomOut);
    }
    if consume(i, none, Key::Num0) {
        cmds.push(Cmd::ZoomReset);
    }
    cmds
}

/// The file a dropped folder opens: its README or index, else the first Markdown file
/// by name. Only this folder is looked at, as the drop is handled on the UI thread.
#[cfg(windows)]
fn markdown_in_folder(dir: &Path) -> Option<PathBuf> {
    if let Some(p) = FOLDER_INDEX.iter().map(|n| dir.join(n)).find(|p| p.is_file()) {
        return Some(p);
    }
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| !t.is_dir()) && files::is_markdown(&e.path()))
        .map(|e| e.path())
        .min_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
}

/// Rough height guess for blocks that have not been laid out yet.
fn estimate_height(block: &Block, col_w: f32, base: f32) -> f32 {
    let lh = base * 1.55;
    let chars_per_line = (col_w / (base * 0.5)).max(10.0);
    let para = |chars: usize| ((chars as f32 / chars_per_line).ceil().max(1.0)) * lh;
    match block {
        Block::Heading { level, inline, .. } => {
            let scale = match level {
                1 => 2.0,
                2 => 1.55,
                3 => 1.28,
                _ => 1.1,
            };
            base * 1.4 + para(inline.text.len()) * scale * 0.85
        }
        Block::Paragraph(items) => items
            .iter()
            .map(|it| match it {
                InlineItem::Text(t) => para(t.text.len()),
                InlineItem::Image(_) => lh * 1.2,
            })
            .sum(),
        Block::Code(c) => c.text.lines().count().max(1) as f32 * base * 0.86 * 1.5 + base * 1.3,
        Block::Quote { blocks, .. } => blocks.iter().map(|b| estimate_height(b, col_w - base * 1.3, base) + base * 0.9).sum(),
        Block::List { items, .. } => items
            .iter()
            .map(|it| it.blocks.iter().map(|b| estimate_height(b, col_w - base * 1.6, base) + base * 0.3).sum::<f32>())
            .sum(),
        Block::Table(t) => (t.rows.len() + 1) as f32 * (lh + base * 0.8),
        Block::Rule => base * 1.0,
        Block::Image(_) => base * 12.0,
        Block::Math(_) => lh * 1.5,
        Block::Metadata(_) => lh * 1.2,
        Block::DefList(e) => e.len() as f32 * lh * 2.5,
        Block::Footnotes(f) => f.len() as f32 * lh * 1.5 + base * 2.0,
    }
}

/// Ask egui to repaint after `delay` from a helper thread (timer-based repaints are coarse on some backends).
fn wake_after(ctx: &Context, delay: Duration) {
    let ctx = ctx.clone();
    std::thread::Builder::new()
        .name("wake".into())
        .spawn(move || {
            std::thread::sleep(delay);
            ctx.request_repaint();
        })
        .ok();
}

/// A `file://` URI as a plain path; anything else unchanged.
fn local_path(uri: &str) -> &str {
    uri.strip_prefix("file://").unwrap_or(uri)
}

/// An image URI as an address worth copying: a plain path for a `file://` image.
fn image_address(uri: &str) -> String {
    #[cfg(windows)]
    if let Some(p) = links::file_uri_to_path(uri) {
        return links::display_path(&p);
    }
    local_path(uri).to_owned()
}

/// Whether a watcher event is about the open file. On Windows the name can come in the
/// case the writer used, or as an 8.3 short name (`README~1.MD`), which only counts if
/// it still leads to the open file.
fn is_watched_file(p: &Path, target: &Path) -> bool {
    if p == target {
        return true;
    }
    if !cfg!(windows) || p.parent() != target.parent() {
        return false;
    }
    let lower = |p: &Path| p.as_os_str().to_string_lossy().to_lowercase();
    match p.file_name().map(|n| n.to_string_lossy()) {
        Some(name) if is_short_name(&name) => dunce::canonicalize(p).is_ok_and(|long| lower(&long) == lower(target)),
        Some(_) => lower(p) == lower(target),
        None => false,
    }
}

/// An 8.3 short name: up to eight characters ending in `~` and digits, then an optional
/// extension of up to three (`README~1.MD`). Not `notes.md~`, `~$x.docx` or `.~lock.x#`.
fn is_short_name(name: &str) -> bool {
    let (stem, ext) = name.split_once('.').unwrap_or((name, ""));
    let Some((head, num)) = stem.rsplit_once('~') else { return false };
    !head.is_empty() && stem.len() <= 8 && !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) && ext.len() <= 3 && !ext.contains('.')
}

/// The offset to give the content scroll area for a requested one. `content_h` is the
/// content height from the last frame, and `measured_this_doc` says whether that frame showed
/// the document now loaded.
///
/// The clamp keeps End's `f32::MAX` out of the layout. It only applies to the page it was
/// measured on: on the frame after a document change, `content_h` is the previous
/// document's, and clamping Back's saved offset to it would land a long page near the top.
/// Other offsets go through as they are, and the scroll area clamps them to the new document
/// at the end of the frame. End before the new page has been measured (a reload or the Open
/// dialog can change the document earlier in the same frame) gets `None`: try next frame.
fn scroll_target(requested: f32, content_h: f32, measured_this_doc: bool) -> Option<f32> {
    if measured_this_doc {
        Some(requested.min(content_h.max(0.0)))
    } else if requested == f32::MAX {
        None
    } else {
        Some(requested)
    }
}

/// Whether a block is laid out this frame rather than given its last measured (or estimated)
/// height: it is near the view, every block is being measured, it is held (a jump or a
/// restored place keeps it in the view), it is the last block and has never been measured,
/// or an image or diagram in it was still loading when it was. Every block is measured on a
/// document's first frame only, when its images are still being read on egui's loader
/// thread; without the pending case, one off screen would keep its 24 px placeholder height,
/// and history, reload, the percentage and the scrollbar would all work from it.
fn lay_out_block(m: Measure, in_view: bool, measure_all: bool, is_target: bool, last: bool) -> bool {
    in_view || measure_all || is_target || m.pending || !m.measured && last
}

/// Puts the content scroll area at `y` this frame, with the rubber band off when
/// `rubber_band_off`: last frame's bounds do not fit this content.
///
/// That is so on the first frame of a new document, when the scroll area still has the previous
/// one's bounds, and when the content was drawn shifted last frame. With rubber-banding on, an
/// offset past those bounds springs back and then coasts on upward, so Back to a page left
/// further down than the previous page is tall lands far too high. Off for this frame, the
/// offset stays put and the scroll area clamps it to the new content.
fn set_scroll(ui: &mut Ui, sa: ScrollArea, y: f32, rubber_band_off: bool) -> ScrollArea {
    if rubber_band_off {
        ui.spacing_mut().scroll.kinetic.rubber_band = false;
    }
    sa.vertical_scroll_offset(y)
}

/// The block containing screen y `y`, and where its top is relative to `y` (`tops[i] - y`:
/// negative when the block starts above `y`). `None` when `tops` is empty or `y` is above the
/// first block (the top padding, as at every page top). A `y` exactly on a boundary names the
/// block starting there.
fn block_at(tops: &[f32], y: f32) -> Option<(usize, f32)> {
    let i = tops.partition_point(|t| *t <= y);
    (i > 0).then(|| (i - 1, tops[i - 1] - y))
}

/// How far to move the content up (positive) or down (negative) so a block whose top is at `top`
/// lands at `wanted`. The caller passes `top` on the unrounded layout (the drawn top less
/// `AnchorPass::round`): drawn tops sit on egui's pixel-rounded content origin while its offset is
/// unrounded, so a drawn top would add the rounding to every correction and from there to the
/// offset. Under 1 pt it is 0: nothing worth moving the content for. The limit `-view_y` keeps the
/// true offset from going below 0 (it can leave a sub-1 pt correction at the very top, which is
/// harmless).
fn anchor_correction(top: f32, wanted: f32, view_y: f32) -> f32 {
    let d = top - wanted;
    if d.abs() < 1.0 {
        return 0.0;
    }
    d.max(-view_y)
}

/// Where a jump puts its target's top below the view top: `gap`, or higher up the screen only as
/// far as the page end allows. A target in the last screen cannot reach the top, so it goes as
/// high as `to_end` (the distance from the target's top to the page end) leaves room for.
fn jump_rel(gap: f32, view_h: f32, to_end: f32) -> f32 {
    gap.max(view_h - to_end)
}

/// Brings a place saved deeper than its block (`rel < -h`: the top of the block more than `h`
/// above the view) back inside the block, about a line (`base`) from its bottom, so the view
/// stays inside it. A place already inside the block is left alone, including one in the last
/// `base` pt: `h` includes the block's trailing gap, and `block_at` names the block while the
/// view top is in that gap, so a floor for every negative `rel` would nudge such a view down.
/// A jump (`rel >= 0`) is never changed.
fn clamp_rel(rel: f32, h: f32, base: f32) -> f32 {
    if rel < -h { -(h - base).max(0.0) } else { rel }
}

/// A block that changed height from `h_old` to `h_new` (a relayout the reader started): the same
/// proportion of the block above the view top, less `moved` (what the pass already moved), kept
/// inside the block and no higher than `need`. `None` when either height is 0 or less, or they
/// differ by under 0.5 pt (nothing worth correcting).
fn rescale_rel(rel: f32, h_old: f32, h_new: f32, moved: f32, base: f32, need: f32) -> Option<f32> {
    if h_old <= 0.0 || h_new <= 0.0 || (h_new - h_old).abs() < 0.5 {
        return None;
    }
    Some((rel * h_new / h_old - moved).clamp(-(h_new - base).max(0.0), 0.0).max(need))
}

/// Whether egui's end-of-frame clamp, not the reader, moved the offset from `painted` to `after`:
/// past the end (`max`) it lands on `max`, above 0 on 0. egui clamps to 0 when the page is shorter
/// than the view (`max < 0`), hence `max.max(0.0)`.
fn end_clamp(painted: f32, after: f32, max: f32) -> bool {
    (painted > max && (after - max.max(0.0)).abs() < 0.01) || (painted < 0.0 && after.abs() < 0.01)
}

/// Whether the offset moved by more than rounding (0.5 pt) and not by egui's end clamp: a wheel
/// turn, a scrollbar drag or a key scroll, as opposed to egui easing towards a target of its own.
fn moved_by_user(painted: f32, after: f32, max: f32) -> bool {
    (after - painted).abs() > 0.5 && !end_clamp(painted, after, max)
}

/// Where a hold on `hold = (block, rel)` goes after a pass, given the `shown` rel it was drawn at.
/// Kept as it is while the block sits no lower than `gap + 0.5` below the view top and no higher
/// than the page end allows (`need`). Otherwise re-based on whatever is at the view top, or at
/// `page_end - view_h` when the hold wanted the view past the end, so growth above the held block
/// is not shown as a strip and the move-in clamp does not fight egui's bottom clamp. A `y` above
/// block 0 (the top padding, a page shorter than the view) is `(0, tops[0] - y)`. `None` for a
/// block index past `tops` (a stale hold after a reload).
fn rebase_hold(hold: (usize, f32), shown: f32, tops: &[f32], page_end: f32, view_h: f32, view_top: f32, gap: f32) -> Option<(usize, f32)> {
    let (b, _) = hold;
    if b >= tops.len() {
        return None;
    }
    let need = view_h - (page_end - tops[b]);
    if need <= shown && shown <= gap + 0.5 {
        return Some(hold);
    }
    let y = if shown < need { page_end - view_h } else { view_top };
    block_at(tops, y).or(Some((0, tops[0] - y)))
}

/// Whether the scroll offset is still animating, so a shift must not move into it yet, and the
/// updated start time of an episode where only egui's own movement (no key) kept the gate closed.
/// A live key target is never capped: held on auto-repeat it keeps egui's absolute target alive,
/// and moving in then would drag the view back. egui's own movement is capped at `cap` s, in case
/// an egui version has a mover that never ends; once capped it stays so until that movement
/// stops or a key press takes over.
fn gate(now: f64, key_anim_end: f64, egui_moving: bool, egui_moving_since: Option<f64>, cap: f64) -> (bool, Option<f64>) {
    if now < key_anim_end {
        (true, None)
    } else if egui_moving {
        let since = egui_moving_since.unwrap_or(now);
        (now - since <= cap, Some(since))
    } else {
        (false, None)
    }
}

/// What `App` hands `Anchor::before` each frame, by value: last frame's scroll state and the few
/// facts about this frame it cannot get itself.
struct FrameIn {
    /// Last frame's scroll offset, viewport height and content height.
    scroll_offset: f32,
    content_h: f32,
    viewport_h: f32,
    /// Last frame showed this document (`content_gen == gen`).
    same_doc: bool,
    /// `doc.blocks.len()`.
    n: usize,
    /// The layout key changed this frame, computed before the `layout_key` branch clears `measured`.
    key_changed: bool,
    /// This frame's `ui.available_width()`.
    avail_w: f32,
    /// `settings.base_size`, for `clamp_rel`.
    base: f32,
}

/// What one pass hands from `Anchor::before` and the column to `Anchor::after`. `before` resets it at
/// the start of every pass.
struct AnchorPass {
    /// The entry this pass keeps, with the `rel` it is drawn at (`clamp_rel` applied).
    held: Option<(usize, f32)>,
    /// `(block, rel, old height)` of the held place inside a block above the view, noted in `before` on a relayout.
    rescale: Option<(usize, f32, f32)>,
    /// The content Ui's clip top, read first in the column.
    view_top: f32,
    /// The column's own clip, restored at the held block.
    clip: Rect,
    /// The gap between blocks (`block_gap`), which a jump aims for below the view top.
    gap: f32,
    /// The correction applied at the held block: cursor before minus cursor after (positive: moved up).
    d: f32,
    /// This pass's page end.
    page_end: f32,
    /// How far egui's pixel rounding put the content origin from the unrounded one (`view_top -
    /// painted`). Drawn tops sit on egui's pixel-rounded origin while egui's offset is unrounded. A
    /// place taken from drawn tops and then measured from again would carry the rounding forward
    /// each time, so `at_block`, the re-base and `view` compare on the unrounded layout.
    round: f32,
}

/// Written by hand: emath's `Rect` has no `Default`. `column` sets `clip` before `at_block` reads it.
impl Default for AnchorPass {
    fn default() -> Self {
        Self { held: None, rescale: None, view_top: 0.0, clip: Rect::NOTHING, gap: 0.0, d: 0.0, page_end: 0.0, round: 0.0 }
    }
}

// egui internals this relies on (recheck after an egui upgrade): the content origin is fixed from the
// offset before physics and the key-scroll target step (no test here pins it); `vertical_scroll_offset`
// leaves the key-scroll target alone (no test pins it: if egui cleared the target, the gate would only
// wait longer than needed); that target lives at most `duration.max`
// (a_hold_survives_blocks_above_shrinking_by_more_than_a_screen fails if it lives longer); `content_size`
// is the content Ui's `min_size` (no test here pins it). With the rubber band off, `end()` clamps an
// out-of-range offset with this frame's bounds: back_lands_where_it_left_even_from_a_scrollable_page and
// a_restored_place_deeper_than_its_block (i) fail if it does not. The content Ui's `max_rect().top()`,
// read in `column`, is egui's pixel-rounded content origin, which holds while the scroll area's
// `content_margin` is zero (a top margin would move jump holds by the margin), and the content clip top
// is the scroll area's `inner_rect` top. The correction does not depend on that clip top (it cancels out
// through `round`), but the clip of the blocks above and `view` do.
/// The reader's place, kept as a block and how far its top is below the top of the view, so content
/// that changes height above the view (a diagram or image finishing, a relayout) does not move what
/// is being read. A jump or a restored position becomes a hold; the column draws the held block where
/// the hold says by moving the content before it in the same pass, and the difference is carried as
/// `shift` until no egui animation can be running.
///
/// Each frame `show_content` calls `before` (the offset to place, the shift to move in, the place to
/// keep this pass), then `column` inside `show_viewport`, whose body calls `at_block` at the top of
/// every block (the correction at the kept block), then `after` (the shift, the hold released or
/// re-based, the gate, `view`, and a relayout's rescale). A pass egui discards runs all of them again.
#[derive(Default)]
struct Anchor {
    /// Keep block `b`'s top `rel` points below the top of the view (negative: the block starts above
    /// it) until the reader scrolls. Set by every jump, by Back/Forward and by reload.
    hold: Option<(usize, f32)>,
    /// For a hold restored by Back/Forward or reload with a negative `rel`, the held block's
    /// height when the place was saved; `None` for every other hold, and once it has been compared
    /// with the block's settled height.
    hold_h: Option<f32>,
    /// Near the page end: `(block, need)` for a restored hold still waiting whose block is
    /// pending; the next pass draws that hold no higher than `need`.
    end_floor: Option<(usize, f32)>,
    /// How much higher than the scroll offset the content is drawn: a correction not yet moved into
    /// egui's offset. The true offset is `scroll_offset + shift`.
    shift: f32,
    /// The block at the top of the view and its `rel`, as of the end of the last frame, adjusted for
    /// the movement egui applied. `place` reads it.
    view: Option<(usize, f32)>,
    /// Screen y of the page end as drawn this frame (the content Ui's cursor top after the closing
    /// space, which is the end egui clamps to), kept for `jump`.
    page_end: f32,
    /// This frame's `viewport.min.y`, kept for `jump`'s guess.
    painted: f32,
    /// The gate for moving `shift` into the offset: when a key-scroll target stops being alive.
    key_anim_end: f64,
    /// Whether the offset may still be animating, so `shift` must not move in yet.
    offset_animating: bool,
    /// When egui's own movement (no key) started keeping the gate closed, for the cap.
    egui_moving_since: Option<f64>,
    /// Whether the cap's `log::debug!` was written in the current episode.
    cap_logged: bool,
    /// Last frame's `ui.available_width()`, to tell a relayout.
    avail_w: f32,
    /// Values one pass hands from `before` and the column to `after`.
    pass: AnchorPass,
}

impl Anchor {
    /// Before `show_viewport`. Places `*pending_offset` (Home, End, a load, Back's guess, a jump's
    /// guess), or puts it back with a repaint when `scroll_target` says "next frame"; otherwise moves
    /// the shift into the offset once no egui animation can be running. Then decides, once for this
    /// pass, which place the column keeps (`pass.held`), and on a relayout notes the place inside
    /// its block for `after` to rescale (`pass.rescale`).
    fn before(&mut self, ui: &mut Ui, mut sa: ScrollArea, pending_offset: &mut Option<f32>, f: FrameIn, measures: &[Measure]) -> ScrollArea {
        self.pass = AnchorPass::default();
        if let Some(off) = pending_offset.take() {
            // The true height is `content_h + shift`, so End is not cut short. While the content was
            // drawn shifted last frame, egui's bounds are stale too: no rubber band, or it springs back.
            match scroll_target(off, f.content_h + self.shift, f.same_doc) {
                Some(y) => {
                    sa = set_scroll(ui, sa, y, !f.same_doc || self.shift != 0.0);
                    self.shift = 0.0;
                }
                None => {
                    *pending_offset = Some(off);
                    ui.ctx().request_repaint();
                }
            }
        } else if self.shift != 0.0 && !self.offset_animating {
            // egui's bounds still come from the shifted content: with the rubber band on, it would
            // spring the new offset back. Off, `end()` clamps it with this frame's bounds.
            let true_max = (f.content_h + self.shift - f.viewport_h).max(0.0);
            sa = set_scroll(ui, sa, (f.scroll_offset + self.shift).clamp(0.0, true_max), true);
            self.shift = 0.0;
        }
        // A relayout: the column width or zoom changed the layout key, or the available width
        // changed (a window resize, a sidebar, any zoom), which can rewrap a wide diagram with no
        // key change. Read before the caller's `layout_key` branch clears `measured`.
        let relayout = f.key_changed || f.avail_w != self.avail_w;
        self.avail_w = f.avail_w;
        let entry = self.hold.filter(|&(b, _)| b < f.n);
        if let Some((b, rel)) = entry {
            // Note the place inside a block that starts above the view, with the height it had, so
            // `after` can keep it in proportion once the block is laid out at the new width. Only a
            // block measured before the change has a height to scale from (an unmeasured one has 0
            // or a height from an earlier width). A restored place still waiting for its block to
            // settle refers to the height it was saved with, not this one, so it is left to that.
            let m = measures.get(b).copied().unwrap_or_default();
            if relayout && rel < 0.0 && m.measured && m.height > 0.0 && self.hold_h.is_none() {
                self.pass.rescale = Some((b, rel, m.height));
            }
            // A place deeper than its block is now (a restore onto a diagram back at its
            // placeholder, or onto a block a width change shortened) is drawn about a line inside
            // the block; the stored `rel` stays for when the block regains its height. A block not
            // measured yet is drawn as stored, and `after` discards the pass if that was wrong.
            let mut drawn = if m.measured { clamp_rel(rel, m.height, f.base) } else { rel };
            // Near the page end, while the clamp is in effect, no higher than the end allows.
            if let Some((fb, need)) = self.end_floor {
                if fb == b && drawn != rel {
                    drawn = drawn.max(need);
                }
            }
            self.pass.held = Some((b, drawn));
        }
        sa
    }

    /// Inside `show_viewport`'s closure: the top padding, the column (`body` draws the blocks into
    /// it, calling `at_block` at the top of each) and the bottom padding, up to the page end.
    ///
    /// The column is a child Ui started `shift` higher than the cursor, not an allocated scope: a
    /// rect allocated from the shifted top would grow the content Ui's `min_rect` upward and so
    /// egui's `content_size`. Only `cursor .. col_end` is allocated, so `content_size` is exactly
    /// the shown layout's length. `new_child` takes its id from the parent's counter as
    /// `scope_builder` does, so widget ids (text selection, code-block scroll state) are unchanged.
    #[allow(clippy::too_many_arguments)]
    fn column(&mut self, ui: &mut Ui, viewport: Rect, col_x: f32, col_w: f32, base: f32, gap: f32, body: impl FnOnce(&mut Ui, &mut Anchor)) {
        // egui fixes the content origin from this offset before touch drag, physics and the
        // key-scroll target step, so it is what this pass shows.
        self.painted = viewport.min.y;
        // The content Ui's clip top is this pass's view top, before any clip change. Not last
        // frame's content rect: the search bar opening, or a resize, moves it.
        self.pass.view_top = ui.clip_rect().top();
        self.pass.gap = gap;
        // The content Ui's top is `view_top - painted`, rounded to pixels by egui.
        self.pass.round = ui.max_rect().top() - (self.pass.view_top - self.painted);
        ui.add_space(base * 1.2);
        let cursor = ui.cursor().top();
        let col_top = cursor - self.shift;
        // The content Ui's max_rect is the view's size, not infinite: with blocks above shrunk by
        // more than a screen, its bottom can be above `col_top`.
        let col_rect = Rect::from_min_max(pos2(col_x, col_top), pos2(col_x + col_w, ui.max_rect().bottom().max(col_top + 100.0)));
        let mut col = ui.new_child(UiBuilder::new().max_rect(col_rect).layout(Layout::top_down(Align::Min)));
        col.set_width(col_w);
        col.spacing_mut().item_spacing.y = 0.0;
        self.pass.clip = col.clip_rect();
        if let Some((_, rel)) = self.pass.held {
            // Blocks before the held one are laid out where they were before any growth, which may
            // be inside the view: clipped, they cannot paint there or be hovered there.
            col.set_clip_rect(self.pass.clip.intersect(Rect::everything_above(self.pass.view_top + (rel - gap).max(0.0))));
        }
        body(&mut col, self);
        // The cursor, not `min_rect().bottom()`: a top-down cursor only ever expands `min_rect`, so
        // after a correction upward it still reaches the held block's uncorrected top.
        let col_end = col.cursor().top();
        drop(col);
        ui.advance_cursor_after_rect(Rect::from_min_max(pos2(col_x, cursor), pos2(col_x + col_w, col_end.max(cursor))));
        ui.add_space(base * 6.0);
        // After the item spacing `advance_cursor_after_rect` adds: the end egui clamps to.
        self.pass.page_end = ui.cursor().top();
    }

    /// Called by the column's `body` at the top of block `i`, before its top is read. At the held
    /// block, ends the clip above it and moves the cursor so the block lands where the hold says:
    /// that block and everything after it are drawn right in this same pass.
    fn at_block(&mut self, ui: &mut Ui, i: usize) {
        let Some((b, rel)) = self.pass.held else { return };
        if i != b {
            return;
        }
        ui.set_clip_rect(self.pass.clip);
        let before = ui.cursor().top();
        let d = anchor_correction(before - self.pass.round, self.pass.view_top + rel, self.painted + self.shift);
        if d != 0.0 {
            ui.add_space(-d);
            // As applied, after add_space's rounding: positive when content above grew.
            self.pass.d = before - ui.cursor().top();
        }
    }

    /// After `show_viewport`, before `App` stores the scroll state. Takes this pass's correction into
    /// `shift` and `block_tops`, releases the hold if the reader scrolled, re-bases it, updates the
    /// move-in gate and notes the block at the view top. After a relayout that changed the height of
    /// the block the place is inside, keeps the place in proportion and discards the pass.
    fn after(&mut self, ctx: &Context, out: &ScrollAreaOutput<()>, scroll_delta: f32, block_tops: &mut [f32], measures: &[Measure], base: f32) {
        let painted = self.painted;
        let after = out.state.unclamped_offset().y;
        let moved = after - painted;
        let inner = out.inner_rect;
        let max = out.content_size.y - inner.height();
        let page_end_now = self.pass.page_end;
        if self.pass.d != 0.0 {
            self.shift += self.pass.d;
            if let Some((b, _)) = self.pass.held {
                let b = b.min(block_tops.len());
                for t in &mut block_tops[..b] {
                    *t -= self.pass.d;
                }
            }
        }
        let (wheel, primary_down, now) = ctx.input(|i| {
            // Not Ctrl/Cmd+wheel (egui's zoom modifier), and not a trackpad fling's momentum tail,
            // which may have started before the hold was set.
            let wheel = i.events.iter().any(|e| matches!(e, Event::MouseWheel { modifiers, source, .. } if !modifiers.command && *source != MouseWheelSource::Momentum));
            (wheel, i.pointer.primary_down(), i.time)
        });
        let user = scroll_delta != 0.0 || ((wheel || primary_down) && moved_by_user(painted, after, max));
        // egui's key-scroll target lives at most `duration.max` after the press that made it.
        let anim = ctx.global_style().scroll_animation.duration.max as f64;
        let cap = 2.0 * anim + 0.5;
        if scroll_delta != 0.0 {
            self.key_anim_end = now + anim;
        }
        let egui_moving = moved.abs() > 0.5 && !user && !end_clamp(painted, after, max);
        (self.offset_animating, self.egui_moving_since) = gate(now, self.key_anim_end, egui_moving, self.egui_moving_since, cap);
        if self.egui_moving_since.is_none() {
            self.cap_logged = false;
        } else if !self.offset_animating && !self.cap_logged {
            log::debug!("scroll anchoring: egui moved the offset for over {cap} s; moving the shift in");
            self.cap_logged = true;
        }
        if user {
            self.hold = None;
            self.hold_h = None;
        }
        let shown = match (self.pass.held, self.hold) {
            (Some((pb, prel)), Some((b, _))) if pb == b => prel,
            (_, hold) => hold.map_or(0.0, |(_, rel)| rel),
        };
        let mut new = self.hold.and_then(|h| rebase_hold(h, shown, block_tops, page_end_now, inner.height(), inner.top() + moved + self.pass.round, self.pass.gap));
        // A restored place on a placeholder in the last screen: the clamped `rel` can ask for a view
        // past the end, and a re-base there would lose the stored place for good. Keep it while
        // the block is pending, and have the next pass draw it no higher than the end allows.
        self.end_floor = None;
        if let (Some((b, _)), Some(_)) = (self.hold, self.hold_h) {
            if let (Some(&top), Some(mb)) = (block_tops.get(b), measures.get(b)) {
                if mb.pending {
                    let need = inner.height() - (page_end_now - top);
                    if shown < need {
                        new = self.hold;
                    }
                    self.end_floor = Some((b, need));
                }
            }
        }
        // Only a kept hold (the same one back) keeps the height it was restored with.
        if new != self.hold {
            self.hold_h = None;
        }
        self.hold = new;
        self.view = block_at(block_tops, inner.top() + moved + self.pass.round);
        // egui's end clamp is not a movement to take off a place: the end is handled by `need`.
        let m = if end_clamp(painted, after, max) { 0.0 } else { moved };
        // A relayout changed the height of the block the place is inside: the stored `rel` alone
        // would put a reader 800 pt into a 1,200 pt diagram that halves 200 pt below it. Keep the
        // same proportion of block `b` above the view: store it in place of what the re-base just
        // stored (after a halve, a later block), and discard this pass so the shown one is drawn
        // there. Not after the reader scrolled: the hold is gone and `view` follows them.
        let mut rescaled = false;
        if let Some((b, rel, h_old)) = self.pass.rescale.filter(|_| !user) {
            if let (Some(&top), Some(mb)) = (block_tops.get(b), measures.get(b)) {
                let need = inner.height() - (page_end_now - top);
                if let Some(r) = rescale_rel(rel, h_old, mb.height, m, base, need) {
                    self.hold = Some((b, r));
                    self.hold_h = None;
                    rescaled = true;
                    ctx.request_discard("relayout: keep the place in the top block");
                }
            }
        }
        // A restored place inside a block above the view.
        if let Some((b, rel)) = self.hold.filter(|&(_, rel)| !rescaled && rel < 0.0) {
            if let (Some(&top), Some(mb)) = (block_tops.get(b), measures.get(b).filter(|mb| mb.measured)) {
                let need = inner.height() - (page_end_now - top);
                // Once the block has settled (laid out, not pending), compare its height once with the
                // one the place was saved with: a width or zoom change while away keeps the place in
                // proportion, as a relayout does.
                let mut r = None;
                if let Some(h0) = self.hold_h.filter(|_| !mb.pending) {
                    self.hold_h = None;
                    r = rescale_rel(rel, h0, mb.height, m, base, need);
                }
                if let Some(r) = r {
                    self.hold = Some((b, r));
                    ctx.request_discard("restore: keep the place in a block of another height");
                } else if let Some((_, prel)) = self.pass.held.filter(|&(pb, _)| pb == b) {
                    // Where `before` will draw it next pass. Unlike this pass's: in the first pass after
                    // a load (the block unmeasured, drawn as stored), when the block's height changed (it
                    // regained its height, or its placeholder rewrapped), or when a re-base near the end
                    // moved the hold to another `rel` on it. Discard, so the shown pass has it right.
                    let mut want = clamp_rel(rel, mb.height, base);
                    if let Some((fb, fneed)) = self.end_floor {
                        if fb == b && want != rel {
                            want = want.max(fneed);
                        }
                    }
                    if (want - prel).abs() >= 0.5 {
                        ctx.request_discard("restore: the held place is drawn elsewhere next pass");
                    }
                }
            }
        }
        self.page_end = page_end_now;
        if self.shift != 0.0 {
            ctx.request_repaint();
        }
    }

    /// `lay_out_block`'s `is_target` for block `i`: this pass keeps it.
    fn holds(&self, i: usize) -> bool {
        self.pass.held.is_some_and(|(b, _)| b == i)
    }

    /// `jump_to_block`'s part: holds block `b` and returns the offset to place this frame, or `None`
    /// to leave `pending_offset` alone. `tops` is `block_tops`, `content_top` the content rect's top.
    /// A jump is not a restored place: `hold_h` and `end_floor` go, or `place` would save a stale
    /// height for `b`. When last frame showed this document the hold is `(b, rel)` with `rel` raised
    /// only as far as the page end allows, and the offset is guessed from last frame's layout
    /// (`painted`, the reference `view` uses, plus `shift`); otherwise, after a load, the hold is
    /// `(b, gap)` and corrects from the load's own offset in the same pass.
    fn jump(&mut self, b: usize, gap: f32, tops: &[f32], viewport_h: f32, content_top: f32, same_doc: bool) -> Option<f32> {
        self.hold_h = None;
        self.end_floor = None;
        match tops.get(b).filter(|_| same_doc) {
            Some(&top) => {
                let rel = jump_rel(gap, viewport_h, self.page_end - top);
                self.hold = Some((b, rel));
                Some((self.painted + self.shift + top - content_top - rel).max(0.0))
            }
            None => {
                self.hold = Some((b, gap));
                None
            }
        }
    }

    /// Back/Forward and reload: holds the saved place unless its block is past the new end (`b >=
    /// n`: an edited file can have fewer blocks). `hold_h` is kept only for a negative `rel` and a
    /// known height: the settle compare acts on nothing else, and with `Some` nothing would clear it.
    fn restore(&mut self, at: Option<(usize, f32, f32)>, n: usize) {
        let at = at.filter(|&(b, _, _)| b < n);
        self.hold = at.map(|(b, rel, _)| (b, rel));
        self.hold_h = at.filter(|&(_, rel, _)| rel < 0.0).and_then(|(_, _, h)| (h > 0.0).then_some(h));
        self.end_floor = None;
    }

    /// Home and End: no hold. `hold_h` and `end_floor` go with it, or the next hold would save a
    /// restored hold's height as its own.
    fn clear_hold(&mut self) {
        self.hold = None;
        self.hold_h = None;
        self.end_floor = None;
    }

    /// The place to come back to, for `save_visit` and `reload`: `(block, rel, height)`. The hold comes
    /// first so that two Backs in one frame, and bottom-clamped jumps, round-trip; `rel` is the stored
    /// one, not the clamped one. The height is `hold_h` for a restored hold still waiting for its block
    /// to settle, else the block's measured height, and 0 for a pending or unmeasured block: a
    /// placeholder's height is not the block's, and the settle compare would scale the place into the
    /// full diagram. With 0, `restore` drops `hold_h` and `clamp_rel` still keeps the view inside the block.
    fn place(&self, measures: &[Measure]) -> Option<(usize, f32, f32)> {
        let (b, rel) = self.hold.or(self.view)?;
        let h = match self.hold_h.filter(|_| self.hold.is_some()) {
            Some(h) => h,
            None => measures.get(b).filter(|m| m.measured && !m.pending).map_or(0.0, |m| m.height),
        };
        Some((b, rel, h))
    }

    /// A new document (`set_document`, `load_path_in`'s error branch): nothing of the old one's place
    /// survives. The gate's state is left alone; it is about the scroll area, not the document.
    fn reset(&mut self) {
        self.clear_hold();
        self.shift = 0.0;
        self.view = None;
    }
}

/// Read errors a save in progress causes on Windows: the file is locked (sharing or lock
/// violation), delete-pending (access denied) or briefly gone while an editor replaces it.
fn is_transient_read_error(e: &std::io::Error) -> bool {
    cfg!(windows) && matches!(e.raw_os_error(), Some(2 | 5 | 32 | 33))
}

#[allow(dead_code)]
fn _unused(_: Color32, _: Stroke) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ctrl as egui-winit reports it: Cmd on macOS, Ctrl elsewhere.
    const CTRL: Modifiers = if cfg!(target_os = "macos") {
        Modifiers { mac_cmd: true, command: true, ..Modifiers::NONE }
    } else {
        Modifiers { ctrl: true, command: true, ..Modifiers::NONE }
    };

    #[test]
    fn screenshot_waits_for_the_delay_then_for_the_page_to_settle() {
        let ms = Duration::from_millis;
        let delay = ms(1500);
        // The delay is a minimum, even when there is nothing to wait for.
        assert!(!screenshot_due(ms(1499), delay, true));
        assert!(screenshot_due(delay, delay, true));
        // After it, a page still drawing diagrams, images or highlighting is waited for...
        assert!(!screenshot_due(delay + SCREENSHOT_WAIT_MAX - ms(1), delay, false));
        // ...but not forever.
        assert!(screenshot_due(delay + SCREENSHOT_WAIT_MAX, delay, false));
    }

    #[test]
    fn width_buttons_step_through_presets() {
        use WidthStep::{Full, Preset};
        let wide_window = 2400.0;
        assert_eq!(width_step(860.0, false, wide_window, true), Some(Preset(1200.0)));
        assert_eq!(width_step(1600.0, false, wide_window, true), Some(Full));
        assert_eq!(width_step(860.0, false, wide_window, false), Some(Preset(720.0)));
        assert_eq!(width_step(720.0, false, wide_window, false), None);
        // Widths set with [ and ] step to the nearest preset.
        assert_eq!(width_step(920.0, false, wide_window, true), Some(Preset(1200.0)));
        assert_eq!(width_step(920.0, false, wide_window, false), Some(Preset(860.0)));
        // Full width goes no wider, and narrower goes below the window's width.
        assert_eq!(width_step(wide_window, true, wide_window, true), None);
        assert_eq!(width_step(wide_window, true, wide_window, false), Some(Preset(1600.0)));
        // Presets wider than the window are skipped: the column already fills it.
        assert_eq!(width_step(860.0, false, 1100.0, true), Some(Full));
        assert_eq!(width_step(1100.0, true, 1100.0, false), Some(Preset(860.0)));
    }

    fn keys(presses: &[(Modifiers, Key)], typing: bool) -> Vec<Cmd> {
        let mut i = egui::InputState::default();
        i.events = presses
            .iter()
            .map(|&(modifiers, key)| egui::Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers })
            .collect();
        key_commands(&mut i, typing)
    }

    fn key(modifiers: Modifiers, key: Key) -> Vec<Cmd> {
        keys(&[(modifiers, key)], false)
    }

    #[test]
    fn shifted_shortcuts_are_not_taken_by_the_plain_ones() {
        let shift = Modifiers::SHIFT;
        let none = Modifiers::NONE;
        assert_eq!(key(shift, Key::N), [Cmd::Prev]);
        assert_eq!(key(none, Key::N), [Cmd::Next]);
        assert_eq!(key(shift, Key::G), [Cmd::Bottom]);
        assert_eq!(key(none, Key::G), [Cmd::Top]);
        assert_eq!(key(shift, Key::Space), [Cmd::Pages(-1.0)]);
        assert_eq!(key(none, Key::Space), [Cmd::Pages(1.0)]);
        assert_eq!(key(shift, Key::F3), [Cmd::Prev]);
        assert_eq!(key(none, Key::F3), [Cmd::Next]);
        // Both in one frame: each goes to its own binding.
        assert_eq!(keys(&[(shift, Key::N), (none, Key::N)], false), [Cmd::Prev, Cmd::Next]);
    }

    #[test]
    fn back_to_a_longer_page_restores_its_scroll_position() {
        // Back from a 1,900 px page to one left 9,000 px down: the last frame measured the
        // short page, so its height says nothing about where the long one can scroll.
        assert_eq!(scroll_target(9000.0, 1900.0, false), Some(9000.0));
        // End on the page shown: as far as that page goes.
        assert_eq!(scroll_target(f32::MAX, 1900.0, true), Some(1900.0));
        assert_eq!(scroll_target(500.0, 1900.0, true), Some(500.0));
    }

    /// Runs `frames` headless egui frames of a page `h` tall in a 600 px window, first putting
    /// it at `restore` (offset, measured_this_doc) if given. Returns where it ends up.
    fn scroll_frames(ctx: &Context, time: &mut f64, h: f32, mut restore: Option<(f32, bool)>, frames: usize) -> f32 {
        let mut y = 0.0;
        for _ in 0..frames {
            *time += 1.0 / 60.0;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))),
                time: Some(*time),
                predicted_dt: 1.0 / 60.0,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                let mut sa = ScrollArea::vertical().id_salt("content").auto_shrink([false, false]);
                if let Some((off, measured)) = restore.take() {
                    sa = set_scroll(ui, sa, off, !measured);
                }
                y = sa.show(ui, |ui| ui.allocate_space(vec2(100.0, h))).state.clamped_offset().y;
            });
            out.textures_delta.clear();
        }
        y
    }

    #[test]
    fn back_lands_where_it_left_even_from_a_scrollable_page() {
        // On the first frame the scroll area still has the previous page's bounds; with
        // rubber-banding it would spring the restored offset back and then coast up.
        let ctx = Context::default();
        let mut time = 0.0;
        scroll_frames(&ctx, &mut time, 2000.0, None, 3);
        let y = scroll_frames(&ctx, &mut time, 20000.0, Some((9000.0, false)), 90);
        assert!((y - 9000.0).abs() < 1.0, "landed at {y}, not 9000");
    }

    #[test]
    fn end_waits_for_a_new_page_to_be_measured() {
        // End in the frame a reload or the Open dialog changed the document: f32::MAX must
        // not reach the layout, so it waits a frame for the new page's height.
        assert_eq!(scroll_target(f32::MAX, 1900.0, false), None);
    }

    #[test]
    fn blocks_measured_while_loading_are_laid_out_again() {
        let settled = Measure { height: 300.0, measured: true, pending: false };
        let loading = Measure { height: 24.0, measured: true, pending: true };
        // Off screen, after the first frame: an image or diagram that was still loading when
        // the block was measured is laid out until it has its own size...
        assert!(lay_out_block(loading, false, false, false, false));
        // ...while a settled block keeps its height.
        assert!(!lay_out_block(settled, false, false, false, false));
        // The other reasons still hold.
        assert!(lay_out_block(settled, true, false, false, false));
        assert!(lay_out_block(settled, false, true, false, false));
        assert!(lay_out_block(settled, false, false, true, false));
        assert!(lay_out_block(Measure::default(), false, false, false, true));
        assert!(!lay_out_block(settled, false, false, false, true));
        assert!(!lay_out_block(Measure::default(), false, false, false, false));
    }

    #[test]
    fn shift_still_types_symbols() {
        // '?' and '+' need Shift on a US keyboard, '/' on a German one.
        assert_eq!(key(Modifiers::SHIFT, Key::Questionmark), [Cmd::Help]);
        assert_eq!(key(Modifiers::SHIFT, Key::Plus), [Cmd::ZoomIn]);
        assert_eq!(key(Modifiers::SHIFT, Key::Slash), [Cmd::Search]);
        assert_eq!(key(Modifiers::NONE, Key::OpenBracket), [Cmd::Narrower]);
    }

    #[test]
    fn ctrl_shortcuts_and_typing() {
        assert_eq!(key(CTRL, Key::O), [Cmd::Open]);
        assert_eq!(key(CTRL, Key::Q), [Cmd::Quit]);
        assert_eq!(key(Modifiers::ALT, Key::ArrowLeft), [Cmd::Back]);
        // Ctrl+N is not a plain N.
        assert_eq!(key(CTRL, Key::N), []);
        // A focused text field keeps the single keys but not the Ctrl ones.
        assert_eq!(keys(&[(Modifiers::NONE, Key::N), (Modifiers::NONE, Key::Q)], true), []);
        assert_eq!(keys(&[(CTRL, Key::F)], true), [Cmd::Search]);
    }

    #[cfg(windows)]
    #[test]
    fn alt_chords_trigger_nothing_on_windows() {
        let alt = Modifiers::ALT;
        let ctrl_alt = CTRL | Modifiers::ALT;
        for k in [Key::F, Key::O, Key::Q, Key::Space, Key::N, Key::Num0, Key::Backspace, Key::ArrowDown, Key::Home] {
            assert_eq!(key(alt, k), [], "Alt+{k:?}");
        }
        // Ctrl+Alt is AltGr: Ctrl+Alt+Q types '@' on a German keyboard.
        for k in [Key::Q, Key::W, Key::O, Key::F, Key::R] {
            assert_eq!(key(ctrl_alt, k), [], "Ctrl+Alt+{k:?}");
        }
        assert_eq!(key(Modifiers::ALT, Key::ArrowRight), [Cmd::Forward]);
        // Ctrl with the navigation keys still scrolls.
        assert_eq!(key(CTRL, Key::End), [Cmd::Bottom]);
    }

    #[cfg(windows)]
    #[test]
    fn dropped_folder_opens_its_readme_or_first_markdown_file() {
        let dir = std::env::temp_dir().join(format!("mdread-drop-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        assert_eq!(markdown_in_folder(&dir), None);
        std::fs::write(dir.join("sub").join("a.md"), "").unwrap();
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        assert_eq!(markdown_in_folder(&dir), None);
        std::fs::write(dir.join("Zeta.md"), "").unwrap();
        std::fs::write(dir.join("beta.markdown"), "").unwrap();
        assert_eq!(markdown_in_folder(&dir), Some(dir.join("beta.markdown")));
        std::fs::write(dir.join("index.md"), "").unwrap();
        assert_eq!(markdown_in_folder(&dir), Some(dir.join("index.md")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_names_are_told_from_editor_temp_files() {
        for name in ["README~1.MD", "NOTES~12.M", "AB~1", "PROGRA~1"] {
            assert!(is_short_name(name), "{name}");
        }
        for name in ["notes.md~", "~$report.docx", ".~lock.x#", "README~1.MARKDOWN", "LONGNAME~1.MD", "a~b.md", "x~1.tar.gz"] {
            assert!(!is_short_name(name), "{name}");
        }
    }

    #[test]
    fn a_failed_first_load_drops_its_anchor() {
        // `mdread folder.md#x`: the path exists, so `#x` is split off, but it cannot be read.
        // The anchor belonged to that file; the next document opened must not jump to x.
        let dir = std::env::temp_dir().join(format!("mdread-anchor-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("folder.md")).unwrap();
        let next = dir.join("next.md");
        std::fs::write(&next, "# Top\n\n## x\n").unwrap();
        let ctx = Context::default();
        let args = Args { files: vec![dir.join("folder.md")], anchor: Some("x".into()), ..Args::default() };
        let mut app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()), args);
        assert!(app.error.is_some(), "the folder should fail to load");
        app.load_path(next, true, &ctx);
        assert!(app.doc.is_some());
        assert_eq!(app.pending_anchor, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_jump_on_an_error_page_holds_nothing() {
        // Search's n/N still jump on an error page, with the previous document's matches and tops:
        // a hold made there would be restored when this path reloads.
        let dir = std::env::temp_dir().join(format!("mdread-errjump-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("folder.md")).unwrap();
        let page = dir.join("page.md");
        std::fs::write(&page, (1..=50).map(|i| format!("Paragraph {i}.\n\n")).collect::<String>()).unwrap();
        let ctx = Context::default();
        let mut app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()), Args::default());
        let mut time = 0.0;
        assert!(app.load_path(page, true, &ctx));
        app_frames(&mut app, &ctx, &mut time, 3, vec![], |_| {});
        assert!(!app.load_path(dir.join("folder.md"), true, &ctx), "the folder should fail to load");
        app.jump_to_block(30, &ctx);
        assert_eq!(app.anchor.hold, None);
        // The watcher holds the folder open on Windows.
        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An image loader for the real-App tests: `slow-a.png` and `slow-b.png` stay loading, as a slow
    /// network image does, until their flag is set, then arrive as an 800 × 2400 px image. Anything
    /// else is left to the other loaders. egui caches a texture once it is ready, so each URI flips
    /// once per `Context`.
    struct SlowLoader {
        ready: [Arc<AtomicBool>; 2],
    }

    impl SlowLoader {
        const URIS: [&'static str; 2] = ["http://mdread.test/slow-a.png", "http://mdread.test/slow-b.png"];
    }

    impl egui::load::ImageLoader for SlowLoader {
        fn id(&self) -> &str {
            concat!(module_path!(), "::SlowLoader")
        }

        fn load(&self, _ctx: &Context, uri: &str, _size_hint: egui::load::SizeHint) -> egui::load::ImageLoadResult {
            let Some(i) = Self::URIS.iter().position(|u| *u == uri) else {
                return Err(egui::load::LoadError::NotSupported);
            };
            if self.ready[i].load(Ordering::Relaxed) {
                Ok(egui::load::ImagePoll::Ready { image: Arc::new(egui::ColorImage::filled([800, 2400], Color32::GRAY)) })
            } else {
                Ok(egui::load::ImagePoll::Pending { size: None })
            }
        }

        fn forget(&self, _uri: &str) {}

        fn forget_all(&self) {}

        fn byte_size(&self) -> usize {
            0
        }
    }

    /// Runs `frames` frames of the real App in a headless egui: 800 × 600 pt, 1.5 pixels per point,
    /// 60 frames a second. `events` go into the first frame only (a key press in one call, its
    /// release in the next). `check` sees the App after each frame.
    fn app_frames(app: &mut App, ctx: &Context, time: &mut f64, frames: usize, mut events: Vec<egui::Event>, mut check: impl FnMut(&App)) {
        for _ in 0..frames {
            *time += 1.0 / 60.0;
            let mut viewports = egui::ViewportIdMap::default();
            viewports.insert(egui::ViewportId::ROOT, egui::ViewportInfo { native_pixels_per_point: Some(1.5), ..Default::default() });
            let input = egui::RawInput {
                viewports,
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))),
                time: Some(*time),
                predicted_dt: 1.0 / 60.0,
                events: std::mem::take(&mut events),
                ..Default::default()
            };
            // By path: `use eframe::App` would clash with mdread's `App`.
            let mut out = ctx.run_ui(input, |ui| eframe::App::ui(app, ui, &mut eframe::Frame::_new_kittest()));
            out.textures_delta.clear();
            check(app);
        }
    }

    /// A page with `![](image)` above 200 paragraphs and `## Target` after paragraph 100.
    fn slow_image_page(image: &str) -> String {
        let mut s = format!("![]({image})\n\n");
        for i in 1..=200 {
            s += &format!("Paragraph {i} of the page.\n\n");
            if i == 100 {
                s += "## Target\n\n";
            }
        }
        s
    }

    /// Flips `flag` (the image above the target arrives) and runs 30 frames, asserting on each that
    /// the image grew by more than a screen and that the target block stayed where it was in the
    /// frame before the flip.
    fn target_holds_across_the_flip(app: &mut App, ctx: &Context, time: &mut f64, flag: &AtomicBool, half: &str) {
        let target = app.doc.as_ref().and_then(|d| d.find_slug("target")).expect("a Target heading");
        let place = |app: &App| app.block_tops[target] - app.content_rect.top();
        let (before, h0) = (place(app), app.measures[0].height);
        assert!(app.measures[0].pending, "{half}: the image is still loading before the flip");
        assert!((0.0..app.viewport_h).contains(&before), "{half}: the target is in the view before the flip, {before} below its top");
        flag.store(true, Ordering::Relaxed);
        let mut f = 0;
        app_frames(app, ctx, time, 30, vec![], |app| {
            let grew = app.measures[0].height - h0;
            assert!(grew > app.viewport_h, "{half} f{f}: the image grew by {grew}, not more than the view ({})", app.viewport_h);
            let now = place(app);
            assert!((now - before).abs() <= 1.0, "{half} f{f} after the flip: the target is {now} below the view top, was {before}");
            f += 1;
        });
    }

    #[test]
    fn real_app_jump_and_back_hold_across_a_slow_image() {
        let dir = std::env::temp_dir().join(format!("mdread-rt-jumpback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (jump, back, other) = (dir.join("jump.md"), dir.join("back.md"), dir.join("other.md"));
        std::fs::write(&jump, slow_image_page(SlowLoader::URIS[0])).unwrap();
        std::fs::write(&back, slow_image_page(SlowLoader::URIS[1])).unwrap();
        std::fs::write(&other, "Another page, plain text.\n").unwrap();
        let ctx = Context::default();
        let mut app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()), Args::default());
        // The folder has three pages, so the files sidebar would open in whatever frame the folder
        // scan reports, a relayout at a different frame in each run. Off, every run lays out the same.
        app.settings.show_files = false;
        let ready = [Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false))];
        ctx.add_image_loader(Arc::new(SlowLoader { ready: ready.clone() }));
        let mut time = 0.0;
        let target = |app: &App| app.doc.as_ref().and_then(|d| d.find_slug("target")).expect("a Target heading");

        // A jump to a heading below an image still loading, then the image arrives.
        assert!(app.load_path(jump, true, &ctx));
        app_frames(&mut app, &ctx, &mut time, 10, vec![], |_| {});
        let b = target(&app);
        app.jump_to_block(b, &ctx);
        app_frames(&mut app, &ctx, &mut time, 10, vec![], |_| {});
        target_holds_across_the_flip(&mut app, &ctx, &mut time, &ready[0], "jump");

        // Back to that jump before the image has arrived, then it arrives.
        assert!(app.load_path(back.clone(), true, &ctx));
        app_frames(&mut app, &ctx, &mut time, 10, vec![], |_| {});
        let b = target(&app);
        app.jump_to_block(b, &ctx);
        app_frames(&mut app, &ctx, &mut time, 10, vec![], |_| {});
        assert!(app.load_path(other, true, &ctx));
        app_frames(&mut app, &ctx, &mut time, 10, vec![], |_| {});
        app.go_history(-1, &ctx);
        app_frames(&mut app, &ctx, &mut time, 10, vec![], |_| {});
        target_holds_across_the_flip(&mut app, &ctx, &mut time, &ready[1], "back");

        // The file is edited while it is read: a paragraph added at the end. The place comes back as
        // the same block at the same distance from the view top.
        let b = target(&app);
        let place = |app: &App| app.block_tops[b] - app.content_rect.top();
        let before = place(&app);
        std::fs::write(&back, slow_image_page(SlowLoader::URIS[1]) + "One more paragraph at the end.\n").unwrap();
        app.reload(&ctx, false);
        let mut f = 0;
        app_frames(&mut app, &ctx, &mut time, 10, vec![], |app| {
            assert_eq!(target(app), b, "reload f{f}: the target block moved");
            // With everything above measured in the first frame, the pixel offset alone would also
            // land here: the hold is what says the place came back as a block.
            assert!(matches!(app.anchor.hold, Some((hb, _)) if hb == b), "reload f{f}: the hold is {:?}, not on block {b}", app.anchor.hold);
            let now = place(app);
            assert!((now - before).abs() <= 1.0, "reload f{f}: the target is {now} below the view top, was {before}");
            f += 1;
        });

        // Edited again down to 5 paragraphs: the held block is past the new end, so no hold is left
        // and the pixel offset is what remains of the place.
        std::fs::write(&back, (1..=5).map(|i| format!("Paragraph {i}.\n\n")).collect::<String>()).unwrap();
        app.reload(&ctx, false);
        assert_eq!(app.measures.len(), 5);
        // Before any frame: a frame's re-base would also drop a hold past the end, so only here does
        // this see what the reload itself restored.
        assert_eq!(app.anchor.hold, None, "shrunk reload: a hold past the new end was restored");
        let mut f = 0;
        app_frames(&mut app, &ctx, &mut time, 5, vec![], |app| {
            assert_eq!(app.anchor.hold, None, "shrunk reload f{f}: a hold past the new end");
            f += 1;
        });

        // The watcher holds the folder open on Windows.
        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_app_empty_and_single_block_documents() {
        let dir = std::env::temp_dir().join(format!("mdread-rt-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (empty, one) = (dir.join("empty.md"), dir.join("one.md"));
        std::fs::write(&empty, "").unwrap();
        std::fs::write(&one, "## x\n").unwrap();
        let ctx = Context::default();
        let mut app = App::new(&eframe::CreationContext::_new_kittest(ctx.clone()), Args::default());
        let mut time = 0.0;

        // An anchor into a document with no blocks: nothing to hold.
        assert!(app.load_path(empty, true, &ctx));
        app.pending_anchor = Some("x".into());
        app_frames(&mut app, &ctx, &mut time, 3, vec![], |_| {});
        assert_eq!(app.anchor.hold, None, "empty: a hold with no blocks");

        // An anchor to the only block.
        assert!(app.load_path(one, true, &ctx));
        app.pending_anchor = Some("x".into());
        app_frames(&mut app, &ctx, &mut time, 3, vec![], |_| {});
        assert!(matches!(app.anchor.hold, Some((0, _))), "one: the hold is {:?}, not on block 0", app.anchor.hold);
        assert!(app.content_h > 0.0, "one: content height {}", app.content_h);

        // Back to the empty document: the place saved for it is none.
        app.go_history(-1, &ctx);
        app_frames(&mut app, &ctx, &mut time, 3, vec![], |_| {});
        assert_eq!(app.anchor.hold, None, "back to empty: a hold with no blocks");
        assert_eq!(app.anchor.view, None, "back to empty: a view block with no blocks");

        // Forward to the single block. With one block these steps only check that nothing panics or names a
        // block past the end: block 0 or no hold is all there can be.
        app.go_history(1, &ctx);
        app_frames(&mut app, &ctx, &mut time, 3, vec![], |_| {});
        assert!(matches!(app.anchor.hold, None | Some((0, _))), "forward to one: the hold is {:?}", app.anchor.hold);

        // The watcher holds the folder open on Windows.
        drop(app);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn only_the_open_file_counts_as_watched() {
        let dir = dunce::canonicalize(std::env::temp_dir()).unwrap().join(format!("mdread-watch-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("notes.md");
        for name in ["notes.md", "other.md", "notes.md~", "~$notes.md"] {
            std::fs::write(dir.join(name), "").unwrap();
        }
        assert!(is_watched_file(&target, &target));
        assert!(is_watched_file(&dir.join("NOTES.MD"), &target));
        for name in ["other.md", "notes.md~", "~$notes.md", "NOTES~9.MD"] {
            assert!(!is_watched_file(&dir.join(name), &target), "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn block_at_names_the_block_containing_y() {
        let tops = [10.0, 110.0, 210.0];
        assert_eq!(block_at(&tops, 5.0), None);
        assert_eq!(block_at(&tops, 50.0), Some((0, -40.0)));
        assert_eq!(block_at(&tops, 110.0), Some((1, 0.0)));
        assert_eq!(block_at(&tops, 500.0), Some((2, -290.0)));
        assert_eq!(block_at(&[], 0.0), None);
    }

    #[test]
    fn anchor_correction_has_a_dead_band_then_a_limit() {
        assert!(approx(anchor_correction(700.0, 100.0, 2000.0), 600.0));
        assert!(approx(anchor_correction(-500.0, 100.0, 2000.0), -600.0));
        assert!(approx(anchor_correction(100.6, 100.0, 2000.0), 0.0));
        assert!(approx(anchor_correction(99.4, 100.0, 2000.0), 0.0));
        assert!(approx(anchor_correction(-500.0, 100.0, 400.0), -400.0));
        assert!(approx(anchor_correction(95.0, 100.0, 0.5), -0.5));
    }

    #[test]
    fn jump_rel_stops_at_the_page_end() {
        assert!(approx(jump_rel(14.4, 600.0, 3000.0), 14.4));
        assert!(approx(jump_rel(14.4, 600.0, 302.0), 298.0));
        assert!(approx(jump_rel(14.4, 600.0, 585.6), 14.4));
    }

    #[test]
    fn clamp_rel_changes_only_a_place_deeper_than_its_block() {
        assert!(approx(clamp_rel(-800.0, 300.0, 16.0), -284.0));
        assert!(approx(clamp_rel(-800.0, 1200.0, 16.0), -800.0));
        assert!(approx(clamp_rel(14.4, 10.0, 16.0), 14.4));
        assert!(approx(clamp_rel(-5.0, 10.0, 16.0), -5.0));
        assert!(approx(clamp_rel(-20.0, 10.0, 16.0), 0.0));
        assert!(approx(clamp_rel(-290.0, 300.0, 16.0), -290.0));
        assert!(approx(clamp_rel(-300.0, 300.0, 16.0), -300.0));
    }

    #[test]
    fn rescale_rel_keeps_the_place_in_proportion() {
        let none = -1e9;
        assert!(approx(rescale_rel(-800.0, 1200.0, 600.0, 0.0, 16.0, none).unwrap(), -400.0));
        assert!(approx(rescale_rel(-800.0, 1200.0, 2400.0, 0.0, 16.0, none).unwrap(), -1600.0));
        assert!(approx(rescale_rel(-800.0, 1200.0, 600.0, 3.0, 16.0, none).unwrap(), -403.0));
        assert!(approx(rescale_rel(-1190.0, 1200.0, 100.0, 0.0, 16.0, none).unwrap(), -84.0));
        assert!(approx(rescale_rel(-800.0, 1200.0, 600.0, 0.0, 16.0, -300.0).unwrap(), -300.0));
        assert_eq!(rescale_rel(-800.0, 0.0, 600.0, 0.0, 16.0, none), None);
        assert_eq!(rescale_rel(-800.0, 1200.0, 0.0, 0.0, 16.0, none), None);
        assert_eq!(rescale_rel(-800.0, 1200.0, 1200.0, 0.0, 16.0, none), None);
    }

    #[test]
    fn end_clamp_is_not_the_reader_moving() {
        // (painted, after, max, end_clamp, moved_by_user)
        let rows = [
            (1000.0, 1100.0, 5000.0, false, true),
            (5100.0, 5000.0, 5000.0, true, false),
            (-30.0, 0.0, 5000.0, true, false),
            (30.0, 0.0, -200.0, true, false),
            (4900.0, 5000.0, 5000.0, false, true),
            (1000.0, 1000.3, 5000.0, false, false),
        ];
        for (painted, after, max, clamp, user) in rows {
            assert_eq!(end_clamp(painted, after, max), clamp, "end_clamp({painted}, {after}, {max})");
            assert_eq!(moved_by_user(painted, after, max), user, "moved_by_user({painted}, {after}, {max})");
        }
    }

    #[test]
    fn rebase_hold_keeps_or_rebases() {
        let tops: Vec<f32> = (0..10).map(|i| 10.0 + 100.0 * i as f32).collect();
        let rb = |hold, shown, view_top| rebase_hold(hold, shown, &tops, 1112.0, 300.0, view_top, 14.4);
        let is = |got: Option<(usize, f32)>, want: (usize, f32)| got.is_some_and(|(b, r)| b == want.0 && approx(r, want.1));
        let got = rb((3, 14.4), 14.4, 295.6);
        assert!(is(got, (3, 14.4)), "kept: {got:?}");
        let got = rb((3, 14.8), 14.8, 295.2);
        assert!(is(got, (3, 14.8)), "kept within the 0.5 tolerance: {got:?}");
        let got = rb((3, 50.0), 50.0, 260.0);
        assert!(is(got, (2, -50.0)), "re-based on the block at the view top: {got:?}");
        let got = rb((9, 14.4), 14.4, 895.6);
        assert!(is(got, (8, -2.0)), "re-based so the end sits at the bottom: {got:?}");
        let got = rb((0, 30.0), 30.0, -9.2);
        assert!(is(got, (0, 19.2)), "above block 0: {got:?}");
        assert_eq!(rb((10, 0.0), 0.0, 500.0), None, "stale index");
        let got = rb((1, -800.0), -84.0, 194.0);
        assert!(is(got, (1, -800.0)), "kept with its stored rel when only the shown one passes: {got:?}");
        // A page shorter than the view: the end test lands above block 0.
        let short = [10.0, 110.0, 210.0];
        let got = rebase_hold((1, 14.4), 14.4, &short, 412.0, 600.0, -9.2, 14.4);
        assert!(is(got, (0, 198.0)), "short page: {got:?}");
    }

    #[test]
    fn gate_never_caps_a_live_key_target() {
        let cap = 1.1;
        // A key held for 3 s keeps its target alive: never capped, no matter how long egui_moving has been true.
        let mut since = None;
        for t in 0..180 {
            let now = t as f64 / 60.0;
            let (animating, s) = gate(now, now + 0.3, true, since, cap);
            assert_eq!((animating, s), (true, None), "key held, t = {t}");
            since = s;
        }
        assert_eq!(gate(1.0, 0.5, false, None, cap), (false, None));
        assert_eq!(gate(5.0, 0.0, true, None, cap), (true, Some(5.0)));
        assert_eq!(gate(6.0, 0.0, true, Some(5.0), cap), (true, Some(5.0)));
        assert_eq!(gate(6.2, 0.0, true, Some(5.0), cap), (false, Some(5.0)), "capped");
        assert_eq!(gate(6.3, 0.0, false, Some(5.0), cap), (false, None), "egui stopped");
        assert_eq!(gate(6.4, 6.6, true, Some(5.0), cap), (true, None), "a key press ends the episode");
    }

    fn m(height: f32, measured: bool, pending: bool) -> Measure {
        Measure { height, measured, pending }
    }

    #[test]
    fn place_saves_no_placeholder_height() {
        let mut a = Anchor { view: Some((20, -20.0)), ..Anchor::default() };
        let mut ms = vec![Measure::default(); 21];
        ms[20] = m(38.0, true, true);
        assert_eq!(a.place(&ms), Some((20, -20.0, 0.0)), "a pending block's height is a placeholder's");
        ms[20] = m(38.0, true, false);
        assert_eq!(a.place(&ms), Some((20, -20.0, 38.0)));
        ms[20] = m(38.0, false, false);
        assert_eq!(a.place(&ms), Some((20, -20.0, 0.0)), "an unmeasured block's height is an estimate");
        // A restored hold still waiting for its block to settle keeps the height it was saved with.
        a.hold = Some((20, -800.0));
        a.hold_h = Some(1200.0);
        ms[20] = m(38.0, true, true);
        assert_eq!(a.place(&ms), Some((20, -800.0, 1200.0)));
        // Two Backs in one frame: the hold comes first, not the view.
        let mut a = Anchor { hold: Some((5, 14.4)), hold_h: None, view: Some((20, -20.0)), ..Anchor::default() };
        ms[5] = m(100.0, true, false);
        assert_eq!(a.place(&ms), Some((5, 14.4, 100.0)));
        a.hold = None;
        assert_eq!(a.place(&ms), Some((20, -20.0, 0.0)), "no hold: the view");
        a.view = None;
        assert_eq!(a.place(&ms), None, "at the page top");
    }

    #[test]
    fn restore_keeps_hold_h_only_for_a_negative_kept_hold() {
        let mut a = Anchor { end_floor: Some((3, 10.0)), ..Anchor::default() };
        a.restore(Some((20, -800.0, 1200.0)), 60);
        assert_eq!((a.hold, a.hold_h, a.end_floor), (Some((20, -800.0)), Some(1200.0), None));
        a.restore(Some((20, 14.4, 100.0)), 60);
        assert_eq!((a.hold, a.hold_h), (Some((20, 14.4)), None), "a positive rel never reaches the settle compare");
        a.restore(Some((20, -800.0, 0.0)), 60);
        assert_eq!((a.hold, a.hold_h), (Some((20, -800.0)), None), "an unknown height is not kept");
        a.restore(Some((20, -800.0, 1200.0)), 60);
        a.restore(Some((70, -10.0, 50.0)), 60);
        assert_eq!((a.hold, a.hold_h), (None, None), "an index past the new end");
        a.restore(Some((20, -800.0, 1200.0)), 60);
        a.restore(None, 60);
        assert_eq!((a.hold, a.hold_h), (None, None));
    }

    #[test]
    fn jump_holds_its_block_and_places_its_frame() {
        let tops: Vec<f32> = (0..60).map(|i| 10.0 + 100.0 * i as f32).collect();
        let mut a = Anchor { page_end: 6112.0, hold_h: Some(5.0), end_floor: Some((1, 0.0)), ..Anchor::default() };
        let off = a.jump(40, 14.4, &tops, 600.0, 0.0, true).expect("same document: an offset");
        assert!(approx(off, 3995.6), "{off}");
        assert_eq!((a.hold, a.hold_h, a.end_floor), (Some((40, 14.4)), None, None));
        // In the last screen the target cannot reach the top: it is held where the end allows.
        let off = a.jump(58, 14.4, &tops, 600.0, 0.0, true).expect("same document: an offset");
        assert!(approx(off, 5512.0), "{off}");
        assert!(a.hold.is_some_and(|(b, r)| b == 58 && approx(r, 298.0)), "{:?}", a.hold);
        // A new document: the hold corrects from the load's own offset, nothing is placed.
        assert_eq!(a.jump(30, 14.4, &tops, 600.0, 0.0, false), None);
        assert_eq!(a.hold, Some((30, 14.4)));
    }

    #[test]
    fn clear_hold_and_reset_forget_the_place() {
        let mut a = Anchor { shift: 50.0, view: Some((3, -1.0)), cap_logged: true, egui_moving_since: Some(2.0), ..Anchor::default() };
        a.restore(Some((20, -800.0, 1200.0)), 60);
        a.end_floor = Some((20, 5.0));
        a.clear_hold();
        assert_eq!((a.hold, a.hold_h, a.end_floor), (None, None, None));
        assert_eq!((a.shift, a.view), (50.0, Some((3, -1.0))), "clear_hold leaves the shift and the view");
        a.restore(Some((20, -800.0, 1200.0)), 60);
        a.end_floor = Some((20, 5.0));
        a.reset();
        assert_eq!((a.hold, a.hold_h, a.end_floor, a.view), (None, None, None, None));
        assert_eq!(a.shift, 0.0);
        assert!(a.cap_logged && a.egui_moving_since == Some(2.0), "the gate's state is left alone");
    }

    /// A page of synthetic blocks run through the real `Anchor` steps, frame by frame.
    struct Scenario {
        /// Laid-out height of each block, its gap included.
        heights: Vec<f32>,
        /// `Some(e)`: as a document over 600 blocks (no measuring of every block), an unmeasured
        /// block is given `e` pt.
        estimate: Option<f32>,
        /// `pending_offset` before frame 0, placed as a load's (last frame showed another document).
        start: f32,
        /// `anchor.hold` before frame 0: Back's restore, or a command-line `#anchor`.
        hold: Option<(usize, f32)>,
        /// `anchor.hold_h` before frame 0: the block height Back saved with the place.
        hold_h: Option<f32>,
        /// egui's `max_passes`.
        max_passes: usize,
        /// Each step is applied just before its frame runs.
        steps: Vec<(usize, Step)>,
        frames: usize,
    }

    enum Step {
        /// A key scroll by this many points (+ = down): `scroll_with_delta` in the scroll area,
        /// and `after`'s `scroll_delta`, as `show_content` does with one; in the frame's first
        /// pass only.
        Key(f32),
        /// A mouse wheel turn by this many points (+ = down) over the page, with these modifiers.
        Wheel(f32, Modifiers),
        /// As `Wheel`, from the given source (a macOS trackpad fling's tail is `Momentum`).
        WheelFrom(f32, Modifiers, MouseWheelSource),
        /// The pointer on the scrollbar at x 796, y: a primary press (0), a move only (1) or a
        /// release (2).
        Bar(u8, f32),
        /// A primary click at (x, y): pressed this frame, released the next.
        Click(f32, f32),
        /// A jump to block `b`, as `jump_to_block` makes one.
        Jump(usize),
        /// Block `i` now lays out at this height (an image or diagram arriving, or shrinking); the
        /// block is pending (laid out every frame, like a loading image) until this frame.
        Grow(usize, f32),
        /// Every height times `k`, then the listed ones set: a layout key change.
        Relayout(f32, Vec<(usize, f32)>),
        /// The available width becomes `w` from this frame on (a window resize or a sidebar at a
        /// fixed column width) and the listed heights are set. No key change, so `measured` is not
        /// cleared: a listed block takes its new height when it is next laid out.
        Resize(f32, Vec<(usize, f32)>),
        /// Back (or a reload: the same `Anchor` calls) to this document before the frame.
        Back,
        /// From this frame on, this much space above the scroll area: the view top moves without
        /// scrolling, as when the search bar opens or closes.
        Inset(f32),
    }

    /// One frame as shown, taken from its last pass.
    #[derive(Debug)]
    struct Shown {
        /// The block at the view top and its `rel` (0 or less).
        top: Option<(usize, f32)>,
        /// Each block's top minus the view top.
        rel_tops: Vec<f32>,
        /// `anchor.shift` after the frame.
        shift: f32,
        passes: usize,
        /// `anchor.hold` after the frame.
        hold: Option<(usize, f32)>,
        /// `anchor.view` after the frame.
        view: Option<(usize, f32)>,
        /// The page end minus the view top: under the view height, blank below the end.
        to_end: f32,
        /// The column Ui's rect was the right way up in every pass.
        col_ok: bool,
    }

    /// Runs a scenario through `Anchor::before`, `column` (with `at_block` per block), `after` and
    /// `jump`, the steps `show_content` and `jump_to_block` take, in a headless egui: 800 × 600 pt,
    /// 1.5 pixels per point, 60 frames a second, mdread's item spacing, a 16 pt base (so a gap of
    /// 14.4 pt) and a 400 pt column. Only the blocks are synthetic.
    fn anchored_frames(s: &Scenario) -> Vec<Shown> {
        const BASE: f32 = 16.0;
        const GAP: f32 = 14.4;
        const COL_W: f32 = 400.0;
        let n = s.heights.len();
        let ctx = Context::default();
        ctx.options_mut(|o| o.max_passes = std::num::NonZeroUsize::new(s.max_passes).expect("max_passes"));
        // theme.rs's item spacing; a plain Context has 3, which would move every "at the end" figure.
        ctx.all_styles_mut(|st| st.spacing.item_spacing.y = 6.0);
        let mut heights = s.heights.clone();
        let mut measures = vec![Measure { height: s.estimate.unwrap_or(0.0), measured: false, pending: false }; n];
        let mut tops = vec![0.0; n];
        let mut anchor = Anchor { hold: s.hold, hold_h: s.hold_h, ..Anchor::default() };
        let mut pending_offset = Some(s.start);
        // What App keeps from the last pass: the scroll state, `content_gen == gen` and the content rect's top.
        let (mut scroll_offset, mut content_h, mut viewport_h, mut content_top) = (0.0, 0.0, 0.0, 0.0);
        let mut same_doc = false;
        let mut measure_all = false;
        let mut avail_w = 800.0;
        let mut inset = 0.0;
        let mut release: Option<Pos2> = None;
        let mut shown = Vec::with_capacity(s.frames);
        for f in 0..s.frames {
            let mut events = vec![];
            let primary = |pos: Pos2, pressed: bool| Event::PointerButton { pos, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
            if let Some(p) = release.take() {
                events.push(primary(p, false));
            }
            let mut scroll_delta = 0.0;
            // As at launch, where `layout_key` starts at (0, 0).
            let mut relayout = f == 0;
            for (_, step) in s.steps.iter().filter(|(sf, _)| *sf == f) {
                match step {
                    Step::Key(d) => scroll_delta += d,
                    &Step::Wheel(d, modifiers) => wheel(&mut events, d, modifiers, MouseWheelSource::Wheel),
                    &Step::WheelFrom(d, modifiers, source) => wheel(&mut events, d, modifiers, source),
                    &Step::Bar(k, y) => {
                        let p = pos2(796.0, y);
                        events.push(Event::PointerMoved(p));
                        if k != 1 {
                            events.push(primary(p, k == 0));
                        }
                    }
                    &Step::Click(x, y) => {
                        let p = pos2(x, y);
                        events.push(Event::PointerMoved(p));
                        events.push(primary(p, true));
                        release = Some(p);
                    }
                    &Step::Jump(b) => {
                        if let Some(o) = anchor.jump(b, GAP, &tops, viewport_h, content_top, same_doc) {
                            pending_offset = Some(o);
                        }
                    }
                    &Step::Grow(i, h) => heights[i] = h,
                    Step::Relayout(k, set) => {
                        for h in &mut heights {
                            *h *= k;
                        }
                        for &(i, h) in set {
                            heights[i] = h;
                        }
                        relayout = true;
                    }
                    Step::Resize(w, set) => {
                        avail_w = *w;
                        for &(i, h) in set {
                            heights[i] = h;
                        }
                    }
                    Step::Back => {
                        // save_visit, then go_history: load_path_in (set_document) and restore.
                        let at = anchor.place(&measures);
                        pending_offset = Some(scroll_offset + anchor.shift);
                        anchor.reset();
                        measures.fill(Measure::default());
                        same_doc = false;
                        measure_all = s.estimate.is_none();
                        anchor.restore(at, n);
                    }
                    &Step::Inset(h) => inset = h,
                }
            }
            // A block is pending (a loading image) until the frame its Grow step lands.
            let pending: Vec<bool> = (0..n).map(|i| s.steps.iter().any(|(g, st)| *g > f && matches!(st, Step::Grow(j, _) if *j == i))).collect();
            let mut viewports = egui::ViewportIdMap::default();
            viewports.insert(egui::ViewportId::ROOT, egui::ViewportInfo { native_pixels_per_point: Some(1.5), ..Default::default() });
            let input = egui::RawInput {
                viewports,
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0))),
                time: Some((f + 1) as f64 / 60.0),
                predicted_dt: 1.0 / 60.0,
                events,
                ..Default::default()
            };
            let mut passes = 0;
            let mut col_ok = true;
            let mut last = None;
            let mut out = ctx.run_ui(input, |ui| {
                passes += 1;
                let key_changed = relayout && passes == 1;
                // show_content takes the key delta once per frame: a pass egui runs after a discard
                // gets no events, so it neither scrolls again nor counts as the reader scrolling.
                let scroll_delta = if passes == 1 { scroll_delta } else { 0.0 };
                if inset > 0.0 {
                    ui.add_space(inset);
                }
                let sa = ScrollArea::vertical()
                    .id_salt("content")
                    .auto_shrink([false, false])
                    .scroll_source(ScrollSource { scroll_bar: true, drag: DragScroll::OnTouch, mouse_wheel: true });
                let fin = FrameIn { scroll_offset, content_h, viewport_h, same_doc, n, key_changed, avail_w, base: BASE };
                let sa = anchor.before(ui, sa, &mut pending_offset, fin, &measures);
                if key_changed {
                    for m in &mut measures {
                        m.measured = false;
                    }
                    measure_all = s.estimate.is_none();
                }
                let all = std::mem::take(&mut measure_all);
                let (heights, tops, measures, pending) = (&heights, &mut tops, &mut measures, &pending);
                let out = sa.show_viewport(ui, |ui, viewport| {
                    if scroll_delta != 0.0 {
                        ui.scroll_with_delta(vec2(0.0, -scroll_delta));
                    }
                    let visible = ui.clip_rect().expand2(vec2(0.0, ui.clip_rect().height().max(400.0)));
                    let col_x = ui.max_rect().left() + 40.0;
                    anchor.column(ui, viewport, col_x, COL_W, BASE, GAP, |ui, anchor| {
                        for i in 0..n {
                            anchor.at_block(ui, i);
                            let top = ui.cursor().top();
                            tops[i] = top;
                            if i == 0 {
                                col_ok &= ui.max_rect().min.y < ui.max_rect().max.y;
                            }
                            let m = measures[i];
                            let est = if m.measured { m.height } else { s.estimate.unwrap_or(heights[i]) };
                            let in_view = top <= visible.max.y && top + est >= visible.min.y;
                            if lay_out_block(m, in_view, all, anchor.holds(i), i + 1 == n) {
                                ui.allocate_space(vec2(COL_W, heights[i]));
                                measures[i] = Measure { height: ui.cursor().top() - top, measured: true, pending: pending[i] };
                            } else {
                                ui.allocate_space(vec2(COL_W, est));
                            }
                        }
                    });
                });
                anchor.after(ui.ctx(), &out, scroll_delta, tops, measures, BASE);
                scroll_offset = out.state.clamped_offset().y;
                viewport_h = out.inner_rect.height();
                content_h = out.content_size.y;
                content_top = out.inner_rect.top();
                same_doc = true;
                let view_top = out.inner_rect.top();
                last = Some(Shown {
                    top: block_at(tops, view_top),
                    rel_tops: tops.iter().map(|t| t - view_top).collect(),
                    shift: anchor.shift,
                    passes: 0,
                    hold: anchor.hold,
                    view: anchor.view,
                    to_end: anchor.pass.page_end - view_top,
                    col_ok: true,
                });
            });
            out.textures_delta.clear();
            let mut frame = last.expect("a pass ran");
            frame.passes = passes;
            frame.col_ok = col_ok;
            shown.push(frame);
        }
        shown
    }

    fn wheel(events: &mut Vec<Event>, d: f32, modifiers: Modifiers, source: MouseWheelSource) {
        events.push(Event::PointerMoved(pos2(300.0, 300.0)));
        events.push(Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: vec2(0.0, -d), phase: egui::TouchPhase::Move, source, modifiers });
    }

    /// H: 60 blocks of 100 pt, block 2 a 24 pt placeholder.
    fn page_h() -> Vec<f32> {
        let mut h = vec![100.0; 60];
        h[2] = 24.0;
        h
    }

    fn scenario(heights: Vec<f32>, start: f32, hold: Option<(usize, f32)>, steps: Vec<(usize, Step)>, frames: usize) -> Scenario {
        Scenario { heights, estimate: None, start, hold, hold_h: None, max_passes: 2, steps, frames }
    }

    /// Block `b` at the view top with `x` pt of it above the view.
    fn at(s: &Shown, b: usize, x: f32, tol: f32) -> bool {
        s.top.is_some_and(|(tb, rel)| tb == b && (rel + x).abs() <= tol)
    }

    /// Block `b` held one gap below the view top.
    fn at_gap(s: &Shown, b: usize, tol: f32) -> bool {
        (s.rel_tops[b] - 14.4).abs() <= tol && s.hold.is_some_and(|(hb, _)| hb == b)
    }

    /// The page end at the bottom of the view.
    fn at_end(s: &Shown) -> bool {
        (s.to_end - 600.0).abs() <= 0.5
    }

    fn held(s: &Shown, b: usize, rel: f32, tol: f32) -> bool {
        s.hold.is_some_and(|(hb, r)| hb == b && (r - rel).abs() <= tol)
    }

    /// What every scenario asserts: the column the right way up in every frame, and no shift left.
    fn run(name: &str, s: Scenario) -> Vec<Shown> {
        let shown = anchored_frames(&s);
        for (f, x) in shown.iter().enumerate() {
            assert!(x.col_ok, "{name} f{f}: the column's rect was upside down: {x:?}");
        }
        let last = shown.last().expect("frames");
        assert!(last.shift == 0.0, "{name}: a shift left in the last frame: {last:?}");
        shown
    }

    #[test]
    fn back_lands_on_its_block_while_blocks_above_are_placeholders() {
        let s = run("back", scenario(page_h(), 2626.0, Some((24, -30.0)), vec![(6, Step::Grow(2, 600.0))], 10));
        for (f, x) in s.iter().enumerate() {
            assert!(at(x, 24, 30.0, 1.0), "f{f}: {x:?}");
        }
    }

    #[test]
    fn a_jump_holds_while_blocks_above_grow() {
        let s = run("(a)", scenario(page_h(), 0.0, None, vec![(2, Step::Jump(40)), (12, Step::Grow(2, 600.0))], 15));
        assert!(s[2].shift == 0.0, "(a) f2: the jump's own offset places it: {:?}", s[2]);
        for (f, x) in s.iter().enumerate().skip(2) {
            assert!(at_gap(x, 40, 1.0), "(a) f{f}: {x:?}");
        }
        // A command-line #anchor: the hold is set before the page was ever measured.
        let s = run("(b)", scenario(page_h(), 0.0, Some((30, 14.4)), vec![(8, Step::Grow(2, 600.0))], 12));
        for (f, x) in s.iter().enumerate() {
            assert!(at_gap(x, 30, 1.0), "(b) f{f}: {x:?}");
        }
    }

    #[test]
    fn a_jump_into_the_last_screen_shows_the_end_and_stays() {
        let a = run("(a)", scenario(page_h(), 0.0, None, vec![(2, Step::Jump(58))], 8));
        let b = run("(b)", scenario(page_h(), 0.0, None, vec![(2, Step::Jump(58)), (6, Step::Grow(50, 900.0))], 10));
        for (name, s) in [("(a)", &a), ("(b)", &b)] {
            assert!(s[2].shift == 0.0 && s[2].to_end >= 599.5, "{name} f2: no blank strip below the end: {:?}", s[2]);
            let (tb, trel) = s[2].top.expect("a block at the top");
            for (f, x) in s.iter().enumerate().skip(2) {
                // Holding rel instead of re-basing showed the block above in f6 of (b).
                assert!(at_end(x) && at(x, tb, -trel, 0.5), "{name} f{f}: {x:?}");
            }
        }
        // A command-line #anchor into the last screen: drawn a gap below the top in f0 (the page
        // was not measured yet), then re-based on the block at `page_end - view_h`.
        let s = run("(c)", scenario(page_h(), 0.0, Some((58, 14.4)), vec![], 6));
        assert!((s[0].rel_tops[58] - 14.4).abs() <= 1.0, "(c) f0: {:?}", s[0]);
        assert!(s[0].hold.is_some_and(|(b, _)| b == 55), "(c) f0: re-based on block 55: {:?}", s[0]);
        let (tb, trel) = s[1].top.expect("a block at the top");
        for (f, x) in s.iter().enumerate().skip(1) {
            assert!(at_end(x) && at(x, tb, -trel, 0.5), "(c) f{f}: {x:?}");
        }
    }

    #[test]
    fn a_click_does_not_release_a_hold_but_a_wheel_does() {
        let held_24 = |steps| scenario(page_h(), 2626.0, Some((24, -30.0)), steps, 10);
        let s = run("(a)", held_24(vec![(4, Step::Click(50.0, 300.0)), (7, Step::Grow(2, 600.0))]));
        for (f, x) in s.iter().enumerate() {
            assert!(held(x, 24, -30.0, 1.0) && at(x, 24, 30.0, 1.0), "(a) f{f}: {x:?}");
        }
        // A click on the scrollbar's track moves the view.
        let s = run("(b)", held_24(vec![(4, Step::Bar(0, 500.0)), (5, Step::Bar(2, 500.0))]));
        for (f, x) in s.iter().enumerate().skip(5) {
            assert!(x.hold.is_none(), "(b) f{f}: {x:?}");
        }
        let s = run("(c)", held_24(vec![(4, Step::Wheel(120.0, Modifiers::NONE))]));
        for (f, x) in s.iter().enumerate().skip(5) {
            assert!(x.hold.is_none(), "(c) f{f}: {x:?}");
        }
        // Ctrl+wheel zooms; it does not scroll. egui routes it to zoom, so the offset does not move and this
        // row pins that routing; the `!modifiers.command` filter in `after` is defensive, for a frame where
        // egui also moves the offset.
        let s = run("(d)", held_24(vec![(4, Step::Wheel(120.0, Modifiers::COMMAND))]));
        for (f, x) in s.iter().enumerate() {
            assert!(held(x, 24, -30.0, 1.0), "(d) f{f}: {x:?}");
        }
        // The momentum tail of a fling that started before the hold was set. Its smooth scroll runs
        // to f16 and the shift waits for it, hence 20 frames.
        let s = run("(d')", Scenario { frames: 20, ..held_24(vec![(4, Step::WheelFrom(120.0, Modifiers::NONE, MouseWheelSource::Momentum))]) });
        for (f, x) in s.iter().enumerate() {
            assert!(held(x, 24, -30.0, 1.0), "(d') f{f}: {x:?}");
        }
        // A jump while the wheel's smooth scroll is still running: its tail carries no wheel event.
        let s = run("(e)", scenario(page_h(), 2000.0, None, vec![(1, Step::Wheel(400.0, Modifiers::NONE)), (3, Step::Jump(30)), (12, Step::Grow(2, 600.0))], 20));
        for (f, x) in s.iter().enumerate().skip(3) {
            assert!(at_gap(x, 30, 1.0), "(e) f{f}: {x:?}");
        }
    }

    #[test]
    fn a_hold_survives_blocks_above_shrinking_by_more_than_a_screen() {
        // A jump during a key ease, then about five screens above it vanish: the shift waits for
        // the ease's key target to expire, so the column starts far below the cursor meanwhile.
        let mut heights = vec![100.0; 60];
        heights[2] = 3000.0;
        let s = run("shrink", scenario(heights, 3000.0, None, vec![(1, Step::Key(300.0)), (2, Step::Jump(40)), (4, Step::Grow(2, 24.0))], 30));
        for (f, x) in s.iter().enumerate().skip(2) {
            assert!(at_gap(x, 40, 1.0), "f{f}: {x:?}");
        }
    }

    #[test]
    fn a_hold_stays_when_the_view_top_moves() {
        // The search bar opens over the page and closes again: the view top moves without scrolling.
        // The start matches the hold, so any shift would come from the moving view top.
        let s = run("(a)", scenario(page_h(), 2373.2, Some((24, -30.0)), vec![(4, Step::Inset(40.0)), (7, Step::Inset(0.0))], 10));
        for (f, x) in s.iter().enumerate() {
            assert!(at(x, 24, 30.0, 1.0), "(a) f{f}: {x:?}");
            assert!(held(x, 24, -30.0, 1.0), "(a) f{f}: {x:?}");
            assert!(x.shift == 0.0 && x.passes == 1, "(a) f{f}: no correction for the moved view top: {x:?}");
            // The correction does not depend on the view top, but the place `place` saves does.
            assert!(x.view.is_some_and(|(b, rel)| b == 24 && (rel + 30.0).abs() <= 1.0), "(a) f{f}: view: {x:?}");
        }
        // A jump in the frame the view top moves still lands a gap below the new top.
        let s = run("(b)", scenario(page_h(), 2626.0, None, vec![(4, Step::Inset(40.0)), (4, Step::Jump(30))], 10));
        for (f, x) in s.iter().enumerate().skip(4) {
            assert!(at_gap(x, 30, 1.0), "(b) f{f}: {x:?}");
        }
    }

    /// T: 60 blocks of 100 pt with block `b` a 1,200 pt diagram.
    fn page_t(b: usize) -> Vec<f32> {
        let mut h = vec![100.0; 60];
        h[b] = 1200.0;
        h
    }

    /// The same place, shift and hold in both frames.
    fn same(a: &Shown, b: &Shown) -> bool {
        a.top == b.top && a.shift == b.shift && a.hold == b.hold
    }

    #[test]
    fn a_width_change_keeps_a_held_place_inside_a_tall_block() {
        // 800 pt into block 20, whose top is at 19.2 + 20 × 100.
        let held_20 = |steps| scenario(page_t(20), 2819.2, Some((20, -800.0)), steps, 10);
        for (name, h, x, shift) in [("(a)", 600.0, 400.0, -400.0), ("(b)", 2400.0, 1600.0, 800.0)] {
            let s = run(name, held_20(vec![(4, Step::Relayout(1.0, vec![(20, h)]))]));
            for (f, y) in s.iter().enumerate() {
                let want = if f < 4 { 800.0 } else { x };
                assert!(at(y, 20, want, 1.0), "{name} f{f}: {y:?}");
                assert!(y.passes == if f == 4 { 2 } else { 1 }, "{name} f{f}: passes: {y:?}");
            }
            assert!((s[4].shift - shift).abs() <= 1.0, "{name} f4: shift: {:?}", s[4]);
            for (f, y) in s.iter().enumerate().skip(5) {
                assert!(y.shift == 0.0, "{name} f{f}: shift: {y:?}");
            }
            for f in 7..10 {
                assert!(same(&s[f], &s[6]), "{name} f{f}: not the same as f6: {:?} vs {:?}", s[f], s[6]);
            }
        }
        // Near the end: the halved diagram's place is no lower than the page end allows.
        let s = run("(c)", scenario(page_t(57), 6519.2, Some((57, -800.0)), vec![(4, Step::Relayout(1.0, vec![(57, 600.0)]))], 10));
        for (f, y) in s.iter().enumerate() {
            assert!(y.to_end >= 599.5, "(c) f{f}: blank below the end: {y:?}");
            assert!(y.passes == if f == 4 { 2 } else { 1 }, "(c) f{f}: passes: {y:?}");
        }
        for (f, y) in s.iter().enumerate().skip(4) {
            assert!(y.top.is_some_and(|(b, _)| b == 57) && at_end(y), "(c) f{f}: {y:?}");
        }
        // A jump hold is not a place inside the block: not rescaled.
        let s = run("(d)", scenario(page_t(20), 0.0, None, vec![(2, Step::Jump(20)), (6, Step::Relayout(1.0, vec![(20, 600.0)]))], 10));
        for (f, y) in s.iter().enumerate() {
            if f >= 2 {
                assert!(at_gap(y, 20, 1.0), "(d) f{f}: {y:?}");
            }
            assert!(y.passes == 1, "(d) f{f}: passes: {y:?}");
        }
        // Not measured before the change (frame 0's key change, blocks above estimated): no old height.
        let s = run("(e)", Scenario { estimate: Some(70.0), ..scenario(page_t(20), 2219.2, Some((20, -800.0)), vec![], 8) });
        for (f, y) in s.iter().enumerate() {
            assert!(at(y, 20, 800.0, 1.0) && y.passes == 1, "(e) f{f}: {y:?}");
        }
        // No pass left to discard: one frame shows the unscaled place, the next the rescaled one.
        let s = run("(f)", Scenario { max_passes: 1, ..held_20(vec![(4, Step::Relayout(1.0, vec![(20, 2400.0)]))]) });
        assert!(at(&s[4], 20, 800.0, 1.0), "(f) f4: {:?}", s[4]);
        for (f, y) in s.iter().enumerate().skip(5) {
            assert!(at(y, 20, 1600.0, 1.0), "(f) f{f}: {y:?}");
        }
        // The window narrowed at a fixed column width: no key change, the diagram rescales.
        let s = run("(g)", held_20(vec![(4, Step::Resize(700.0, vec![(20, 600.0)]))]));
        for (f, y) in s.iter().enumerate() {
            let want = if f < 4 { 800.0 } else { 400.0 };
            assert!(at(y, 20, want, 1.0), "(g) f{f}: {y:?}");
            assert!(y.passes == if f == 4 { 2 } else { 1 }, "(g) f{f}: passes: {y:?}");
        }
        // A resize that rewraps nothing discards nothing.
        let s = run("(h)", held_20(vec![(4, Step::Resize(700.0, vec![]))]));
        for (f, y) in s.iter().enumerate() {
            assert!(at(y, 20, 800.0, 1.0) && y.passes == 1, "(h) f{f}: {y:?}");
        }
        // A live resize drag that rewraps the diagram every frame: one discarded pass per frame at most,
        // and the place in proportion each frame.
        let mut steps = vec![];
        for f in [4, 6, 8] {
            steps.push((f, Step::Resize(700.0, vec![(20, 600.0)])));
            steps.push((f + 1, Step::Resize(800.0, vec![(20, 1200.0)])));
        }
        let s = run("(i)", Scenario { frames: 12, ..held_20(steps) });
        for (f, y) in s.iter().enumerate() {
            assert!(y.passes <= 2, "(i) f{f}: passes: {y:?}");
            if f >= 4 {
                let want = if f < 10 && f % 2 == 0 { 400.0 } else { 800.0 };
                assert!(at(y, 20, want, 1.0), "(i) f{f}: {y:?}");
            }
        }
    }

    /// 60 blocks of 100 pt with block `b` this high.
    fn page_with(b: usize, h: f32) -> Vec<f32> {
        let mut v = vec![100.0; 60];
        v[b] = h;
        v
    }

    #[test]
    fn a_restored_place_deeper_than_its_block() {
        // Back to 800 pt into a 1,200 pt diagram that is back at its 300 pt placeholder until f4.
        let back_20 = |heights, steps, frames| Scenario { hold_h: Some(1200.0), ..scenario(heights, 2819.2, Some((20, -800.0)), steps, frames) };
        let s = run("(a)", back_20(page_with(20, 300.0), vec![(4, Step::Grow(20, 1200.0))], 10));
        for (f, x) in s.iter().enumerate() {
            let want = if f < 4 { 284.0 } else { 800.0 };
            assert!(at(x, 20, want, 1.0), "(a) f{f}: {x:?}");
            assert!(held(x, 20, -800.0, 1.0), "(a) f{f}: hold: {x:?}");
            assert!(x.passes == if f == 0 || f == 4 { 2 } else { 1 }, "(a) f{f}: passes: {x:?}");
        }
        // The width changed while away: the diagram is 600 pt now, and settled.
        let s = run("(b)", back_20(page_with(20, 600.0), vec![], 6));
        assert!(held(&s[0], 20, -400.0, 1.0), "(b) f0: hold: {:?}", s[0]);
        for (f, x) in s.iter().enumerate() {
            assert!(at(x, 20, 400.0, 1.0), "(b) f{f}: {x:?}");
            assert!(x.passes == if f == 0 { 2 } else { 1 }, "(b) f{f}: passes: {x:?}");
        }
        // A cached diagram: the same height as saved.
        let s = run("(c)", back_20(page_t(20), vec![], 6));
        for (f, x) in s.iter().enumerate() {
            assert!(at(x, 20, 800.0, 1.0) && x.passes == 1, "(c) f{f}: {x:?}");
        }
        // A place saved in block 24's last 16 pt, its trailing gap: drawn as stored.
        let s = run("(d)", Scenario { hold_h: Some(100.0), ..scenario(page_h(), 2438.2, Some((24, -95.0)), vec![], 10) });
        for (f, x) in s.iter().enumerate() {
            assert!(at(x, 24, 95.0, 0.5) && held(x, 24, -95.0, 0.01) && x.passes == 1, "(d) f{f}: {x:?}");
        }
        // The diagram arrives at a new width in the relayout frame.
        let s = run("(e)", back_20(page_with(20, 300.0), vec![(4, Step::Grow(20, 1200.0)), (4, Step::Relayout(1.0, vec![(20, 600.0)]))], 10));
        for (f, x) in s.iter().enumerate() {
            let want = if f < 4 { 284.0 } else { 400.0 };
            assert!(at(x, 20, want, 1.0), "(e) f{f}: {x:?}");
            assert!(x.passes == if f == 0 || f == 4 { 2 } else { 1 }, "(e) f{f}: passes: {x:?}");
        }
        // The placeholder rewraps meanwhile, then the diagram arrives at the new width.
        let s = run("(f)", back_20(page_with(20, 300.0), vec![(2, Step::Relayout(1.0, vec![(20, 250.0)])), (4, Step::Grow(20, 600.0))], 10));
        for (f, x) in s.iter().enumerate() {
            if f >= 2 {
                let want = if f < 4 { 234.0 } else { 400.0 };
                assert!(at(x, 20, want, 1.0), "(f) f{f}: {x:?}");
            }
            assert!(x.passes == if matches!(f, 0 | 2 | 4) { 2 } else { 1 }, "(f) f{f}: passes: {x:?}");
        }
        // A second Back while block 20 is still a placeholder keeps the stored place.
        let s = run("(g)", back_20(page_with(20, 300.0), vec![(2, Step::Back), (8, Step::Grow(20, 1200.0))], 12));
        for (f, x) in s.iter().enumerate() {
            let want = if f < 8 { 284.0 } else { 800.0 };
            assert!(at(x, 20, want, 1.0), "(g) f{f}: {x:?}");
            assert!(held(x, 20, -800.0, 1.0), "(g) f{f}: hold: {x:?}");
            assert!(x.passes == if matches!(f, 0 | 2 | 8) { 2 } else { 1 }, "(g) f{f}: passes: {x:?}");
        }
        // A Back saved on a placeholder stores no height: the place is not scaled into the diagram.
        let s = run("(h)", scenario(page_with(20, 38.0), 2039.2, Some((20, -20.0)), vec![(3, Step::Back), (10, Step::Grow(20, 1200.0))], 14));
        for (f, x) in s.iter().enumerate() {
            assert!(at(x, 20, 20.0, 1.0), "(h) f{f}: {x:?}");
        }
        // Near the end: the stored place is kept while the placeholder shows the end.
        let s = run("(i)", Scenario { hold_h: Some(1200.0), ..scenario(page_with(57, 300.0), 6519.2, Some((57, -800.0)), vec![(4, Step::Grow(57, 1200.0))], 10) });
        for (f, x) in s.iter().enumerate() {
            if f < 4 {
                assert!(at_end(x), "(i) f{f}: not at the end: {x:?}");
            } else {
                assert!(at(x, 57, 800.0, 1.0), "(i) f{f}: {x:?}");
            }
            assert!(held(x, 57, -800.0, 1.0), "(i) f{f}: hold: {x:?}");
            assert!(x.passes == if f == 0 || f == 4 { 2 } else { 1 }, "(i) f{f}: passes: {x:?}");
        }
        // No pass left to discard: one frame shows a later block, and the regain shows a frame late.
        let s = run("(j)", Scenario { max_passes: 1, ..back_20(page_with(20, 300.0), vec![(4, Step::Grow(20, 1200.0))], 10) });
        assert!(s[0].top.is_some_and(|(b, _)| b > 20), "(j) f0: {:?}", s[0]);
        for (f, x) in s.iter().enumerate() {
            if f >= 1 {
                let want = if f < 5 { 284.0 } else { 800.0 };
                assert!(at(x, 20, want, 1.0), "(j) f{f}: {x:?}");
            }
            assert!(held(x, 20, -800.0, 1.0), "(j) f{f}: hold: {x:?}");
        }
    }
}
