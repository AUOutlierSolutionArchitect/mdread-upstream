//! Background syntax highlighting with syntect (two-face's extended syntax set).
//! Results are cached per (document generation, code block id, dark/light).

use std::collections::{HashMap, HashSet};
use std::sync::{mpsc, Arc, OnceLock};

use egui::Color32;
use syntect::easy::HighlightLines;
use syntect::highlighting::FontStyle;
use syntect::parsing::SyntaxSet;
use two_face::theme::{EmbeddedLazyThemeSet, EmbeddedThemeName};

#[derive(Clone, Copy, Debug)]
pub struct HlSpan {
    pub start: usize,
    pub end: usize,
    pub color: Color32,
    pub italic: bool,
    pub bold: bool,
}

#[derive(Debug, Default)]
pub struct Highlighted {
    /// Byte ranges into the code text. Empty means "render plain".
    pub spans: Vec<HlSpan>,
}

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
static THEMES: OnceLock<EmbeddedLazyThemeSet> = OnceLock::new();

fn syntaxes() -> &'static SyntaxSet {
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

fn themes() -> &'static EmbeddedLazyThemeSet {
    THEMES.get_or_init(two_face::theme::extra)
}

/// Start loading the (large) syntax definitions off the main thread right away.
pub fn preload() {
    std::thread::Builder::new()
        .name("syntax-preload".into())
        .spawn(|| {
            let _ = syntaxes();
            let _ = themes();
        })
        .ok();
}

type Key = (u64, usize, bool);

struct Req {
    key: Key,
    code: Arc<str>,
    lang: Option<String>,
}

struct Resp {
    key: Key,
    result: Arc<Highlighted>,
}

pub struct Highlighter {
    tx: mpsc::Sender<Req>,
    rx: mpsc::Receiver<Resp>,
    cache: HashMap<Key, Arc<Highlighted>>,
    pending: HashSet<Key>,
}

impl Highlighter {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, worker_rx) = mpsc::channel::<Req>();
        let (worker_tx, rx) = mpsc::channel::<Resp>();
        std::thread::Builder::new()
            .name("highlight".into())
            .spawn(move || {
                while let Ok(req) = worker_rx.recv() {
                    let result = highlight(&req.code, req.lang.as_deref(), req.key.2);
                    if worker_tx.send(Resp { key: req.key, result: Arc::new(result) }).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("spawn highlight thread");
        Self { tx, rx, cache: HashMap::new(), pending: HashSet::new() }
    }

    /// Drain finished work. Returns true if anything new arrived.
    pub fn poll(&mut self) -> bool {
        let mut any = false;
        while let Ok(r) = self.rx.try_recv() {
            self.pending.remove(&r.key);
            self.cache.insert(r.key, r.result);
            any = true;
        }
        any
    }

    /// Cached result, or `None` (and a background request) if not ready yet.
    pub fn get(&mut self, gen: u64, id: usize, dark: bool, code: &Arc<str>, lang: Option<&str>) -> Option<Arc<Highlighted>> {
        let key = (gen, id, dark);
        if let Some(h) = self.cache.get(&key) {
            return Some(h.clone());
        }
        if self.pending.insert(key) {
            let _ = self.tx.send(Req { key, code: code.clone(), lang: lang.map(str::to_owned) });
        }
        None
    }

    /// Whether any code block is still waiting for its colours.
    pub fn busy(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Forget results for older documents.
    pub fn retain_generation(&mut self, gen: u64) {
        self.cache.retain(|k, _| k.0 == gen);
        self.pending.retain(|k| k.0 == gen);
    }
}

fn normalize_lang(l: &str) -> &str {
    match l {
        "sh" | "shell" | "zsh" | "console" | "shell-session" | "shellsession" => "bash",
        "yml" => "yaml",
        "jsonc" | "json5" => "json",
        "ts" => "typescript",
        "js" | "mjs" | "cjs" | "node" => "javascript",
        "rs" => "rust",
        "py" | "python3" => "python",
        "rb" => "ruby",
        "md" => "markdown",
        "cs" => "c#",
        "cpp" | "cxx" | "hpp" | "cc" | "hh" => "c++",
        "golang" => "go",
        "kt" | "kts" => "kotlin",
        "hcl" => "terraform",
        "docker" => "dockerfile",
        "plaintext" | "text" | "txt" | "plain" | "none" | "mermaid" => "",
        other => other,
    }
}

fn highlight(code: &str, lang: Option<&str>, dark: bool) -> Highlighted {
    let ss = syntaxes();
    let lang = lang
        .map(|l| l.split(|c: char| c.is_whitespace() || c == ',' || c == '{' || c == ':').next().unwrap_or(""))
        .map(normalize_lang)
        .filter(|l| !l.is_empty());

    let syntax = match lang {
        Some(l) => ss
            .find_syntax_by_token(l)
            .or_else(|| ss.find_syntax_by_extension(l))
            .or_else(|| ss.find_syntax_by_name(l)),
        None => {
            let first = code.lines().next().unwrap_or("");
            if first.starts_with("#!") || first.starts_with("<?") {
                ss.find_syntax_by_first_line(first)
            } else {
                None
            }
        }
    };
    let Some(syntax) = syntax else {
        return Highlighted::default();
    };
    if syntax.name == "Plain Text" {
        return Highlighted::default();
    }

    let theme = themes().get(if dark { EmbeddedThemeName::OneHalfDark } else { EmbeddedThemeName::OneHalfLight });
    let mut hl = HighlightLines::new(syntax, theme);
    let mut spans: Vec<HlSpan> = Vec::new();
    let mut offset = 0usize;
    for line in code.split_inclusive('\n') {
        let Ok(ranges) = hl.highlight_line(line, ss) else {
            break;
        };
        for (style, piece) in ranges {
            let start = offset;
            offset += piece.len();
            let fg = style.foreground;
            let color = Color32::from_rgba_unmultiplied(fg.r, fg.g, fg.b, fg.a);
            let italic = style.font_style.contains(FontStyle::ITALIC);
            let bold = style.font_style.contains(FontStyle::BOLD);
            if let Some(last) = spans.last_mut() {
                if last.end == start && last.color == color && last.italic == italic && last.bold == bold {
                    last.end = offset;
                    continue;
                }
            }
            spans.push(HlSpan { start, end: offset, color, italic, bold });
        }
    }
    Highlighted { spans }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn busy_until_the_worker_answers_or_the_generation_is_dropped() {
        let mut hl = Highlighter::new(egui::Context::default());
        let code: Arc<str> = Arc::from("fn main() {}\n");
        assert!(!hl.busy());
        assert!(hl.get(1, 0, false, &code, Some("rust")).is_none());
        assert!(hl.busy());
        // The first request waits for the syntax set to load.
        let until = Instant::now() + Duration::from_secs(60);
        while hl.get(1, 0, false, &code, Some("rust")).is_none() {
            assert!(hl.busy());
            assert!(Instant::now() < until, "no highlighting within 60 s");
            std::thread::sleep(Duration::from_millis(10));
            hl.poll();
        }
        assert!(!hl.busy());
        // A request for a document no longer shown does not keep it busy.
        assert!(hl.get(1, 1, false, &code, Some("rust")).is_none());
        assert!(hl.busy());
        hl.retain_generation(2);
        assert!(!hl.busy());
    }
}
