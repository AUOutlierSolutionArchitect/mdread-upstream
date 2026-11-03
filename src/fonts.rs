//! Locate good system fonts and register them with egui.
//!
//! Preference order (body): Ubuntu Sans (variable, so bold is a real weight axis),
//! Ubuntu static, Noto Sans, DejaVu Sans, SF Pro (macOS, variable), Arial, then egui's
//! bundled fonts. On Windows: Segoe UI Variable (variable), Segoe UI, Arial.
//! Preference order (code): JetBrains Mono, any installed Nerd Font, Ubuntu Sans Mono,
//! Ubuntu Mono, Fira Code, DejaVu Sans Mono, SF Mono / Menlo-era Monaco (macOS), egui's Hack.
//! On Windows: any installed Nerd Font, Cascadia Mono / Code, Consolas.
//! Files are looked up by bare name in `font_dirs()`, so a user-installed copy in
//! `~/.local/share/fonts`, `~/Library/Fonts` or `%LOCALAPPDATA%\Microsoft\Windows\Fonts`
//! wins over the system one.
//! Override with MDREAD_FONT / MDREAD_MONO_FONT (paths to .ttf/.otf files, or bare file
//! names looked up in the font directories).

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use egui::epaint::text::Tag;
use egui::{Context, FontData, FontDefinitions, FontFamily};

#[derive(Clone, Debug)]
pub struct FontSet {
    /// Regular body face (also installed as `FontFamily::Proportional`).
    pub sans: FontFamily,
    /// True italic face, or `sans` again if none was found (egui then synthesises a slant).
    pub sans_italic: FontFamily,
    /// Static bold face when the body font is not variable.
    pub sans_bold: Option<FontFamily>,
    pub sans_bold_italic: Option<FontFamily>,
    /// Code face (also installed as `FontFamily::Monospace`).
    pub mono: FontFamily,
    /// Code face for inline code, shifted onto the body text's baseline where needed;
    /// otherwise `mono`.
    pub inline_mono: FontFamily,
    /// How far `inline_mono` draws its glyphs below where egui lays them out (negative:
    /// above), as a fraction of the code font size; 0 when it is `mono`. Backgrounds
    /// drawn around inline code move by the same amount.
    pub inline_code_shift: f32,
    /// True when `sans` is a variable font with a `wght` axis, so bold is done with coords.
    pub variable_weight: bool,
    /// Range of the body font's `wght` axis (only meaningful with `variable_weight`).
    pub wght_range: (f32, f32),
    pub sans_name: String,
    pub mono_name: String,
}

impl FontSet {
    /// `wght` coordinate for body text: `block` is the weight a heading or table header
    /// asks for, `bold` is `**strong**` emphasis on top of it.
    pub fn weight(&self, block: Option<f32>, bold: bool) -> f32 {
        span_weight(block, bold, self.wght_range)
    }
}

/// The weights below were tuned for Ubuntu Sans, whose axis reaches 800: headings use
/// 600-700 and bold adds 150 on top. A font whose axis stops lower (Segoe UI Variable
/// tops out at 700) gets its heading weights compressed to stay 100 under the maximum,
/// so bold inside a heading still stands out instead of clamping to the same weight.
fn span_weight(block: Option<f32>, bold: bool, (min, max): (f32, f32)) -> f32 {
    let top = max.min(800.0);
    let ceil = top - 100.0;
    let block = block.map(|w| {
        if ceil < 700.0 && w > 400.0 {
            400.0 + (w - 400.0) * (ceil - 400.0).max(0.0) / 300.0
        } else {
            w
        }
    });
    let w = match (block, bold) {
        (Some(w), true) => (w + 150.0).min(top),
        (Some(w), false) => w,
        (None, true) => 700.0f32.min(top),
        (None, false) => 400.0,
    };
    w.clamp(min, max)
}

struct Candidate {
    name: &'static str,
    regular: &'static str,
    italic: Option<&'static str>,
    bold: Option<&'static str>,
    bold_italic: Option<&'static str>,
    /// A second face appended right after `regular` for scripts it lacks.
    extra: Option<&'static str>,
}

/// Directories searched, in order, for the bare file names in `SANS` and `MONO`.
/// User font directories come first so a locally installed font beats the system copy.
#[cfg(target_os = "macos")]
const FONT_DIRS: &[&str] = &[
    "~/Library/Fonts",
    "/Library/Fonts",
    "/System/Library/Fonts",
    "/System/Library/Fonts/Supplemental",
];
#[cfg(not(any(target_os = "macos", windows)))]
const FONT_DIRS: &[&str] = &[
    "~/.local/share/fonts",
    "~/.local/share/fonts/ttf",
    "~/.fonts",
    "/usr/share/fonts/truetype/ubuntu",
    "/usr/share/fonts/truetype/noto",
    "/usr/share/fonts/truetype/dejavu",
    "/usr/share/fonts/truetype/jetbrains-mono",
    "/usr/share/fonts/truetype/firacode",
    "/usr/share/fonts/truetype",
    "/usr/share/fonts",
    // Arch, and Fedora's DejaVu package.
    "/usr/share/fonts/TTF",
    "/usr/share/fonts/dejavu-sans-fonts",
];

#[cfg(not(windows))]
const SANS: &[Candidate] = &[
    Candidate {
        name: "Ubuntu Sans",
        regular: "UbuntuSans[wdth,wght].ttf",
        italic: Some("UbuntuSans-Italic[wdth,wght].ttf"),
        bold: None,
        bold_italic: None,
        extra: None,
    },
    Candidate {
        name: "Ubuntu",
        regular: "Ubuntu-R.ttf",
        italic: Some("Ubuntu-RI.ttf"),
        bold: Some("Ubuntu-B.ttf"),
        bold_italic: Some("Ubuntu-BI.ttf"),
        extra: None,
    },
    Candidate {
        name: "Noto Sans",
        regular: "NotoSans-Regular.ttf",
        italic: Some("NotoSans-Italic.ttf"),
        bold: Some("NotoSans-Bold.ttf"),
        bold_italic: Some("NotoSans-BoldItalic.ttf"),
        extra: None,
    },
    Candidate {
        name: "DejaVu Sans",
        regular: "DejaVuSans.ttf",
        italic: Some("DejaVuSans-Oblique.ttf"),
        bold: Some("DejaVuSans-Bold.ttf"),
        bold_italic: Some("DejaVuSans-BoldOblique.ttf"),
        extra: None,
    },
    // macOS system UI font: a variable TrueType with a `wght` axis, like Ubuntu Sans.
    Candidate {
        name: "SF Pro",
        regular: "SFNS.ttf",
        italic: Some("SFNSItalic.ttf"),
        bold: None,
        bold_italic: None,
        extra: None,
    },
    // Ships with macOS in /System/Library/Fonts/Supplemental.
    Candidate {
        name: "Arial",
        regular: "Arial.ttf",
        italic: Some("Arial Italic.ttf"),
        bold: Some("Arial Bold.ttf"),
        bold_italic: Some("Arial Bold Italic.ttf"),
        extra: None,
    },
];

/// Only Microsoft-shipped faces: apps such as LibreOffice drop Noto and DejaVu into
/// C:\Windows\Fonts, and those should not win over the system UI font.
#[cfg(windows)]
const SANS: &[Candidate] = &[
    // Windows 11 UI font: variable `wght` 300-700 (plus `opsz`), no italic file, so
    // italics are slanted. Static Segoe UI follows it for Arabic, Hebrew, Armenian and
    // Georgian, which the variable font lacks.
    Candidate {
        name: "Segoe UI Variable",
        regular: "SegUIVar.ttf",
        italic: None,
        bold: None,
        bold_italic: None,
        extra: Some("segoeui.ttf"),
    },
    Candidate {
        name: "Segoe UI",
        regular: "segoeui.ttf",
        italic: Some("segoeuii.ttf"),
        bold: Some("segoeuib.ttf"),
        bold_italic: Some("segoeuiz.ttf"),
        extra: None,
    },
    Candidate {
        name: "Arial",
        regular: "arial.ttf",
        italic: Some("ariali.ttf"),
        bold: Some("arialbd.ttf"),
        bold_italic: Some("arialbi.ttf"),
        extra: None,
    },
];

#[cfg(not(windows))]
const MONO: &[(&str, &str)] = &[("JetBrains Mono", "JetBrainsMono-Regular.ttf")];
#[cfg(windows)]
const MONO: &[(&str, &str)] = &[];

/// Any Nerd Font patched monospace face found in `font_dirs()`, tried after `MONO` and
/// before the distro fallbacks in `MONO_FALLBACK`. Nerd Fonts v3 names files
/// `<Family>NerdFontMono-Regular.ttf` (icons fitted to one cell) and
/// `<Family>NerdFont-Regular.ttf`; the Mono variant is preferred since it never
/// disturbs code alignment.
const NERD_FONT_SUFFIXES: &[&str] = &["NerdFontMono-Regular.ttf", "NerdFont-Regular.ttf"];

#[cfg(not(windows))]
const MONO_FALLBACK: &[(&str, &str)] = &[
    ("Ubuntu Sans Mono", "UbuntuSansMono[wght].ttf"),
    ("Ubuntu Mono", "UbuntuMono-R.ttf"),
    ("Fira Code", "FiraCode-Regular.ttf"),
    ("DejaVu Sans Mono", "DejaVuSansMono.ttf"),
    ("SF Mono", "SFNSMono.ttf"),
    ("Monaco", "Monaco.ttf"),
];
/// Cascadia ships with Windows Terminal (not always in the Fonts folder) or from its
/// GitHub releases (variable `CascadiaMono.ttf`, or static `-Regular` files); Consolas
/// is always present. Third-party copies of DejaVu Sans Mono are deliberately not listed.
#[cfg(windows)]
const MONO_FALLBACK: &[(&str, &str)] = &[
    ("Cascadia Mono", "CascadiaMono.ttf"),
    ("Cascadia Code", "CascadiaCode.ttf"),
    ("Cascadia Mono", "CascadiaMono-Regular.ttf"),
    ("Cascadia Code", "CascadiaCode-Regular.ttf"),
    ("Consolas", "consola.ttf"),
];

/// The user's home directory. Windows has no HOME unless a POSIX shell exported one
/// (Git Bash sets it to `/c/Users/...`, which is not a Windows path), so there the
/// profile directory is used instead.
pub(crate) fn home_dir() -> Option<PathBuf> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    #[cfg(windows)]
    return var("HOME").filter(|h| h.is_absolute()).or_else(|| var("USERPROFILE"));
    #[cfg(not(windows))]
    return var("HOME");
}

fn expand(p: &str) -> PathBuf {
    expand_with(p, home_dir().as_deref())
}

fn expand_with(p: &str, home: Option<&Path>) -> PathBuf {
    let rest = p.strip_prefix("~/").or_else(|| if cfg!(windows) { p.strip_prefix("~\\") } else { None });
    match (rest, home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(p),
    }
}

/// True for values that name one exact file (`/x.ttf`, `C:\x.ttf`, `\\server\x.ttf`,
/// `~/x.ttf`) rather than a file name to look up in the font directories.
fn is_explicit_path(p: &str) -> bool {
    p.starts_with("~/") || (cfg!(windows) && p.starts_with("~\\")) || Path::new(p).is_absolute()
}

/// The font directories, built once: Windows takes them from the environment.
fn font_dirs() -> &'static [PathBuf] {
    static DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    DIRS.get_or_init(build_font_dirs)
}

#[cfg(not(windows))]
fn build_font_dirs() -> Vec<PathBuf> {
    FONT_DIRS.iter().map(|d| expand(d)).collect()
}

/// Per-user installs (the default for Settings > Fonts and most Nerd Font installers)
/// live in %LOCALAPPDATA%\Microsoft\Windows\Fonts; system fonts in %WINDIR%\Fonts.
#[cfg(windows)]
fn build_font_dirs() -> Vec<PathBuf> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let mut dirs = Vec::new();
    if let Some(local) = var("LOCALAPPDATA").or_else(|| home_dir().map(|h| h.join("AppData").join("Local"))) {
        dirs.push(local.join("Microsoft").join("Windows").join("Fonts"));
    }
    let windir = var("WINDIR")
        .or_else(|| var("SystemRoot"))
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    dirs.push(windir.join("Fonts"));
    dirs
}

/// Read a font file. Absolute (or `~/`) paths are read as given; a bare file name is
/// looked up in each of `font_dirs()` in turn.
fn read(p: &str) -> Option<Vec<u8>> {
    if is_explicit_path(p) {
        return std::fs::read(expand(p)).ok();
    }
    font_dirs().iter().find_map(|d| std::fs::read(d.join(p)).ok())
}

/// Read the font named by an override variable, as (display name, bytes). A relative
/// path that is not in the font directories is tried against the current directory.
fn read_override(var: &str) -> Option<(String, Vec<u8>)> {
    let p = std::env::var_os(var)?.to_string_lossy().to_string();
    let bytes = read(&p).or_else(|| {
        let relative = p.contains('/') || (cfg!(windows) && p.contains('\\'));
        if relative { std::fs::read(&p).ok() } else { None }
    });
    let Some(bytes) = bytes else {
        log::warn!("{var}={p}: cannot read font file, using the default");
        return None;
    };
    let name = Path::new(&p)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or(p.clone());
    Some((name, bytes))
}

/// The range of a font's `wght` axis, or `None` for a static face. Read from the font's
/// own `fvar` table, since variable fonts are not reliably named `…[wght].ttf`
/// (Segoe UI Variable is `SegUIVar.ttf`).
fn wght_axis(data: &FontData) -> Option<(f32, f32)> {
    let wght = Tag::new(b"wght");
    data.variation_axes()
        .into_iter()
        .find(|a| a.tag == wght)
        // In order even if a broken font has them the wrong way round: `clamp` panics.
        .map(|a| (a.range.min.min(a.range.max), a.range.min.max(a.range.max)))
}

/// A face's line metrics, as fractions of its em. `descent` is negative below the baseline.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(not(windows), allow(dead_code))]
struct LineMetrics {
    ascent: f32,
    descent: f32,
    line_gap: f32,
}

/// A face's line metrics, chosen the way skrifa (and so epaint) does: the OS/2 typo
/// metrics when USE_TYPO_METRICS is set, else hhea's, else (when hhea's ascender and
/// descender are zero) the OS/2 typo metrics, else the Windows ascent and descent with
/// hhea's line gap. Read straight from the sfnt tables, using the first face of a
/// collection.
#[cfg_attr(not(windows), allow(dead_code))]
fn line_metrics_em(font: &[u8]) -> Option<LineMetrics> {
    let u16_at = |o: usize| Some(u16::from_be_bytes(font.get(o..o + 2)?.try_into().ok()?));
    let i16_at = |o: usize| u16_at(o).map(|v| v as i16 as i32);
    let u32_at = |o: usize| Some(u32::from_be_bytes(font.get(o..o + 4)?.try_into().ok()?));
    let base = if font.get(..4)? == b"ttcf" { u32_at(12)? as usize } else { 0 };
    let tables = u16_at(base + 4)? as usize;
    let table = |tag: &[u8]| -> Option<usize> {
        let rec = (0..tables).map(|i| base + 12 + 16 * i).find(|&r| font.get(r..r + 4) == Some(tag))?;
        u32_at(rec + 8).map(|o| o as usize)
    };
    let upem = u16_at(table(b"head")? + 18)?;
    let os2 = table(b"OS/2");
    let use_typo = os2.and_then(|o| u16_at(o + 62)).is_some_and(|fs_selection| fs_selection & 0x80 != 0);
    let typo = os2.and_then(|o| Some((i16_at(o + 68)?, i16_at(o + 70)?, i16_at(o + 72)?)));
    let (ascent, descent, line_gap) = match (use_typo, typo) {
        (true, Some(typo)) => typo,
        _ => {
            let hhea = table(b"hhea").and_then(|o| Some((i16_at(o + 4)?, i16_at(o + 6)?, i16_at(o + 8)?)));
            match (hhea, typo) {
                (Some(hhea @ (asc, desc, _)), _) if asc != 0 || desc != 0 => hhea,
                (_, Some(typo @ (asc, desc, _))) if asc != 0 || desc != 0 => typo,
                // Windows descent is positive below the baseline; the others are negative.
                (hhea, _) => (u16_at(os2? + 74)? as i32, -(u16_at(os2? + 76)? as i32), hhea.map_or(0, |h| h.2)),
            }
        }
    };
    let em = |v: i32| v as f32 / upem as f32;
    (upem > 0).then(|| LineMetrics { ascent: em(ascent), descent: em(descent), line_gap: em(line_gap) })
}

/// Downward shift, as a fraction of the code font size, that puts inline code on the
/// body text's baseline (negative: upward). epaint centres each face's row (ascent −
/// descent + line gap) in the line height the text asks for, so a face's baseline sits
/// (ascent + descent − line gap) / 2 below the middle of the line. Segoe UI Variable's
/// baseline sits lower than that of any (smaller) code font, which leaves code visibly
/// above the line. `None` when the baselines already agree.
///
/// Superscript and subscript text has no line height of its own (render.rs aligns it to
/// the top or bottom of the row instead), so code inside it is not centred and ends up
/// about 0.17 em of its size too high or too low. That needs `` ^`x`^ `` and is rare
/// enough not to justify two more shifted copies of the code font.
#[cfg_attr(not(windows), allow(dead_code))]
fn code_baseline_shift(sans: LineMetrics, mono: LineMetrics) -> Option<f32> {
    let below_middle = |m: LineMetrics| (m.ascent + m.descent - m.line_gap) / 2.0;
    let delta = below_middle(sans) - CODE_SCALE * below_middle(mono);
    (delta.abs() > 1e-3).then(|| delta / CODE_SCALE)
}

/// First Nerd Font monospace face found, as (display name, bytes). Only the top level of
/// each font directory is listed (a handful of entries), so this costs well under a
/// millisecond and never walks the whole of /usr/share/fonts.
fn find_nerd_font() -> Option<(String, Vec<u8>)> {
    for suffix in NERD_FONT_SUFFIXES {
        for dir in font_dirs() {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(suffix))
                .collect();
            names.sort();
            for file in names {
                if let Ok(bytes) = std::fs::read(dir.join(&file)) {
                    let family = file.trim_end_matches(suffix);
                    let kind = if suffix.starts_with("NerdFontMono") { "Nerd Font Mono" } else { "Nerd Font" };
                    return Some((format!("{family} {kind}"), bytes));
                }
            }
        }
    }
    None
}

/// Faces for the arrows, math operators, check marks and other symbols that the body
/// font and egui's bundled fonts lack (Ubuntu Sans has no arrows at all), so they are
/// not drawn as boxes. The first group with a face present is used. Its faces go last
/// in every family, after egui's emoji fonts, so whatever those draw looks the same; the
/// Windows build loads Segoe UI Symbol for the same purpose (`SCRIPT_FACES`).
#[cfg(not(any(target_os = "macos", windows)))]
const SYMBOL_FACES: &[&[&str]] = &[&["DejaVuSans.ttf"], &["NotoSansMath-Regular.ttf", "NotoSansSymbols2-Regular.ttf"]];

/// Append the first `SYMBOL_FACES` group found to every family, unless the body font
/// (`body_file`) is one of them and so covers the symbols already.
#[cfg(not(any(target_os = "macos", windows)))]
fn add_symbol_faces(defs: &mut FontDefinitions, body_file: Option<&str>) {
    for group in SYMBOL_FACES {
        if body_file.is_some_and(|b| group.contains(&b)) {
            return;
        }
        let faces: Vec<(&str, Vec<u8>)> = group.iter().filter_map(|f| read(f).map(|b| (*f, b))).collect();
        if faces.is_empty() {
            continue;
        }
        let mut names = Vec::new();
        for (file, bytes) in faces {
            log::debug!("fonts: {file} for symbols");
            let name = format!("mdread-symbols-{}", names.len());
            defs.font_data.insert(name.clone(), Arc::new(FontData::from_owned(bytes)));
            names.push(name);
        }
        for list in defs.families.values_mut() {
            list.extend(names.iter().cloned());
        }
        return;
    }
}

/// Size of code (inline and blocks) relative to the surrounding text.
pub const CODE_SCALE: f32 = 0.88;

/// Emoji / symbol fallbacks that ship inside egui and should end every family list.
const TAIL: &[&str] = &["Ubuntu-Light", "NotoEmoji-Regular", "emoji-icon-font"];

fn family(defs: &mut FontDefinitions, name: &str, primary: Vec<String>) -> FontFamily {
    let mut list = primary;
    for t in TAIL {
        if defs.font_data.contains_key(*t) && !list.iter().any(|n| n == t) {
            list.push((*t).to_owned());
        }
    }
    let fam = FontFamily::Name(Arc::from(name));
    defs.families.insert(fam.clone(), list);
    fam
}

pub fn install(ctx: &Context) -> FontSet {
    let mut defs = FontDefinitions::default();

    // ---- body font -------------------------------------------------------
    let mut sans_name = String::from("egui default");
    let mut regular: Option<Vec<u8>> = None;
    let mut italic: Option<Vec<u8>> = None;
    let mut bold: Option<Vec<u8>> = None;
    let mut bold_italic: Option<Vec<u8>> = None;
    let mut extra: Option<Vec<u8>> = None;
    let mut variable = false;
    let mut wght_range = (100.0, 900.0);

    if let Some((name, bytes)) = read_override("MDREAD_FONT") {
        sans_name = name;
        regular = Some(bytes);
    }
    if regular.is_none() {
        for cand in SANS {
            if let Some(bytes) = read(cand.regular) {
                regular = Some(bytes);
                italic = cand.italic.and_then(read);
                bold = cand.bold.and_then(read);
                bold_italic = cand.bold_italic.and_then(read);
                extra = cand.extra.and_then(read);
                sans_name = cand.name.to_owned();
                break;
            }
        }
    }

    let mut sans_bold = None;
    let mut sans_bold_italic = None;
    let sans;
    let sans_italic;
    #[cfg(windows)]
    let sans_metrics = regular.as_deref().and_then(line_metrics_em);
    if let Some(bytes) = regular {
        let data = FontData::from_owned(bytes);
        if let Some(range) = wght_axis(&data) {
            variable = true;
            wght_range = range;
        }
        defs.font_data.insert("mdread-sans".to_owned(), Arc::new(data));
        // Put our face first for the standard proportional family so all UI chrome uses it.
        let prop = defs.families.entry(FontFamily::Proportional).or_default();
        prop.insert(0, "mdread-sans".to_owned());
        // Faces every body family falls back to before egui's bundled ones.
        let mut fallback = Vec::new();
        if let Some(bytes) = extra {
            defs.font_data
                .insert("mdread-sans-extra".to_owned(), Arc::new(FontData::from_owned(bytes)));
            prop.insert(1, "mdread-sans-extra".to_owned());
            fallback.push("mdread-sans-extra".to_owned());
        }
        sans = FontFamily::Proportional;

        if let Some(bytes) = italic {
            defs.font_data
                .insert("mdread-sans-italic".to_owned(), Arc::new(FontData::from_owned(bytes)));
            let mut list = vec!["mdread-sans-italic".to_owned(), "mdread-sans".to_owned()];
            list.extend(fallback.iter().cloned());
            sans_italic = family(&mut defs, "mdread-sans-italic", list);
        } else {
            sans_italic = FontFamily::Proportional;
        }
        // A static font without a bold of its own (an MDREAD_FONT override) stays regular:
        // egui cannot synthesise bold, and another typeface's bold would switch every
        // heading and bold word to that typeface.
        if !variable {
            if let Some(bytes) = bold {
                defs.font_data
                    .insert("mdread-sans-bold".to_owned(), Arc::new(FontData::from_owned(bytes)));
                let mut list = vec!["mdread-sans-bold".to_owned(), "mdread-sans".to_owned()];
                list.extend(fallback.iter().cloned());
                sans_bold = Some(family(&mut defs, "mdread-sans-bold", list));
            }
            if let Some(bytes) = bold_italic {
                defs.font_data.insert(
                    "mdread-sans-bold-italic".to_owned(),
                    Arc::new(FontData::from_owned(bytes)),
                );
                let mut list = vec![
                    "mdread-sans-bold-italic".to_owned(),
                    "mdread-sans-bold".to_owned(),
                    "mdread-sans".to_owned(),
                ];
                list.extend(fallback.iter().cloned());
                sans_bold_italic = Some(family(&mut defs, "mdread-sans-bold-italic", list));
            }
        }
    } else {
        sans = FontFamily::Proportional;
        sans_italic = FontFamily::Proportional;
    }

    // ---- code font -------------------------------------------------------
    let mut mono_name = String::from("Hack");
    let mut mono_bytes: Option<Vec<u8>> = None;
    if let Some((name, bytes)) = read_override("MDREAD_MONO_FONT") {
        mono_name = name;
        mono_bytes = Some(bytes);
    }
    if mono_bytes.is_none() {
        for (name, path) in MONO {
            if let Some(bytes) = read(path) {
                mono_name = (*name).to_owned();
                mono_bytes = Some(bytes);
                break;
            }
        }
    }
    if mono_bytes.is_none() {
        if let Some((name, bytes)) = find_nerd_font() {
            mono_name = name;
            mono_bytes = Some(bytes);
        }
    }
    if mono_bytes.is_none() {
        for (name, path) in MONO_FALLBACK {
            if let Some(bytes) = read(path) {
                mono_name = (*name).to_owned();
                mono_bytes = Some(bytes);
                break;
            }
        }
    }
    #[allow(unused_mut)]
    let mut inline_mono = FontFamily::Monospace;
    #[allow(unused_mut)]
    let mut inline_code_shift = 0.0;
    if let Some(bytes) = mono_bytes {
        // Segoe UI Variable's baseline sits much lower in its row than any code font's.
        // Only inline code sits on the body text's baseline, so it gets a shifted copy of
        // the face; code blocks and the rest of the monospace text keep the plain one.
        #[cfg(windows)]
        if let Some(shift) = sans_metrics.zip(line_metrics_em(&bytes)).and_then(|(s, m)| code_baseline_shift(s, m)) {
            log::debug!("fonts: shifting inline code down by {shift:.3} em to the body baseline");
            let mut data = FontData::from_owned(bytes.clone());
            data.tweak.y_offset_factor = shift;
            defs.font_data.insert("mdread-mono-inline".to_owned(), Arc::new(data));
            let mut list = vec!["mdread-mono-inline".to_owned()];
            list.extend(defs.families.get(&FontFamily::Monospace).into_iter().flatten().cloned());
            inline_mono = family(&mut defs, "mdread-mono-inline", list);
            inline_code_shift = shift;
        }
        defs.font_data.insert("mdread-mono".to_owned(), Arc::new(FontData::from_owned(bytes)));
        let mono = defs.families.entry(FontFamily::Monospace).or_default();
        mono.insert(0, "mdread-mono".to_owned());
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    add_symbol_faces(&mut defs, SANS.iter().find(|c| c.name == sans_name).map(|c| c.regular));

    log::debug!("fonts: body {sans_name} (variable weight: {variable}, wght {wght_range:?}), code {mono_name}");
    #[cfg(windows)]
    if let Ok(mut fb) = FALLBACKS.lock() {
        fb.defs = Some(defs.clone());
    }
    ctx.set_fonts(defs);

    FontSet {
        sans,
        sans_italic,
        sans_bold,
        sans_bold_italic,
        mono: FontFamily::Monospace,
        inline_mono,
        inline_code_shift,
        variable_weight: variable,
        wght_range,
        sans_name,
        mono_name,
    }
}

// ----------------------------------------------------------------------
// CJK and symbol fallbacks (Windows)
// ----------------------------------------------------------------------

/// East Asian scripts, and symbols egui's bundled fonts lack, present in a text.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(not(windows), allow(dead_code))]
struct Scripts {
    han: bool,
    kana: bool,
    hangul: bool,
    /// Arrows, math operators, technical symbols, dingbats (✓ ✗ ⇒ ☐ …).
    symbols: bool,
}

#[cfg_attr(not(windows), allow(dead_code))]
fn scripts(text: &str) -> Scripts {
    let mut s = Scripts::default();
    // Every code point checked below encodes with a lead byte of 0xE1 or more, so plain
    // Latin documents are rejected by one fast byte scan.
    if !text.bytes().any(|b| b >= 0xE1) {
        return s;
    }
    for c in text.chars() {
        match c as u32 {
            0x1100..=0x11FF | 0x3130..=0x318F | 0xA960..=0xA97F | 0xAC00..=0xD7FF => s.hangul = true,
            0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9F => s.kana = true,
            0x2E80..=0x2FDF | 0x3000..=0x303F | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
            | 0xFF00..=0xFF65 | 0x20000..=0x3FFFF => s.han = true,
            0x2190..=0x2BFF => s.symbols = true,
            _ => {}
        }
        if s.han && s.kana && s.hangul && s.symbols {
            break;
        }
    }
    s
}

/// Windows always has CJK fonts, but they are .ttc collections of 13-20 MB each, so they
/// are only read the first time a document or the UI needs them, and appended (lowest
/// priority) to every body and code family in this order, whatever order they were
/// loaded in. Japanese text (any kana) gets Yu Gothic UI, which also covers its kanji;
/// other Han text gets Microsoft YaHei UI, after Yu Gothic, so kanji keep their Japanese
/// forms even when a Chinese document was opened first; Hangul gets Malgun Gothic.
/// Weight coords do not apply to these static faces, so CJK bold renders regular.
/// Segoe UI Symbol likewise covers the arrows, check marks and other symbols that
/// would otherwise be boxes; it comes after egui's emoji fonts, so whatever those
/// already draw looks the same.
#[cfg(windows)]
const SCRIPT_FACES: [(&str, u32, &str); 4] = [
    ("YuGothM.ttc", 1, "mdread-cjk-ja"),
    ("msyh.ttc", 1, "mdread-cjk-zh"),
    ("malgun.ttf", 0, "mdread-cjk-ko"),
    ("seguisym.ttf", 0, "mdread-symbols"),
];

/// The fonts as `install` set them plus the script faces read so far, and the scripts
/// whose face has been looked for.
#[cfg(windows)]
struct Fallbacks {
    defs: Option<FontDefinitions>,
    loaded: Scripts,
}

#[cfg(windows)]
static FALLBACKS: std::sync::Mutex<Fallbacks> =
    std::sync::Mutex::new(Fallbacks { defs: None, loaded: Scripts { han: false, kana: false, hangul: false, symbols: false } });

/// Reads the [`SCRIPT_FACES`] that `text` needs and has not needed before.
#[cfg(windows)]
pub fn add_script_fallbacks(ctx: &Context, fonts: &FontSet, text: &str) {
    let Ok(mut fb) = FALLBACKS.lock() else { return };
    let mut loaded = fb.loaded;
    if loaded.han && loaded.kana && loaded.hangul && loaded.symbols {
        return;
    }
    let need = scripts(text);
    let mut new = [false; SCRIPT_FACES.len()];
    if need.kana && !loaded.kana {
        loaded.kana = true;
        loaded.han = true;
        new[0] = true;
    }
    if need.han && !loaded.han {
        loaded.han = true;
        new[1] = true;
    }
    if need.hangul && !loaded.hangul {
        loaded.hangul = true;
        new[2] = true;
    }
    if need.symbols && !loaded.symbols {
        loaded.symbols = true;
        new[3] = true;
    }
    fb.loaded = loaded;
    let Some(defs) = fb.defs.as_mut() else { return };
    let mut added = false;
    for ((file, index, name), _) in SCRIPT_FACES.into_iter().zip(new).filter(|&(_, new)| new) {
        let Some(bytes) = read(file) else {
            log::debug!("fonts: {file} not found");
            continue;
        };
        log::debug!("fonts: adding {file} #{index} as a fallback");
        defs.font_data.insert(name.to_owned(), Arc::new(FontData { index, ..FontData::from_owned(bytes) }));
        added = true;
    }
    if !added {
        return;
    }
    let mut families = vec![FontFamily::Proportional, FontFamily::Monospace];
    for f in [Some(&fonts.sans_italic), Some(&fonts.inline_mono), fonts.sans_bold.as_ref(), fonts.sans_bold_italic.as_ref()]
        .into_iter()
        .flatten()
    {
        if !families.contains(f) {
            families.push(f.clone());
        }
    }
    let faces: Vec<String> =
        SCRIPT_FACES.iter().map(|&(_, _, name)| name.to_owned()).filter(|name| defs.font_data.contains_key(name)).collect();
    for f in families {
        let list = defs.families.entry(f).or_default();
        list.retain(|name| !faces.contains(name));
        list.extend(faces.iter().cloned());
    }
    ctx.set_fonts(defs.clone());
    ctx.request_repaint();
}

#[cfg(not(windows))]
pub fn add_script_fallbacks(_ctx: &Context, _fonts: &FontSet, _text: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    const UBUNTU_SANS: (f32, f32) = (100.0, 800.0);
    const SEGOE_VAR: (f32, f32) = (300.0, 700.0);

    #[test]
    fn weights_unchanged_for_fonts_reaching_800() {
        for range in [UBUNTU_SANS, (1.0, 1000.0)] {
            assert_eq!(span_weight(None, false, range), 400.0);
            assert_eq!(span_weight(None, true, range), 700.0);
            assert_eq!(span_weight(Some(700.0), false, range), 700.0);
            assert_eq!(span_weight(Some(700.0), true, range), 800.0);
            assert_eq!(span_weight(Some(650.0), false, range), 650.0);
            assert_eq!(span_weight(Some(600.0), true, range), 750.0);
        }
    }

    #[test]
    fn weights_compressed_for_axis_ending_at_700() {
        let h1 = span_weight(Some(700.0), false, SEGOE_VAR);
        let h1_bold = span_weight(Some(700.0), true, SEGOE_VAR);
        assert_eq!(h1, 600.0);
        assert_eq!(h1_bold, 700.0);
        // Heading levels stay ordered and below plain bold.
        let h2 = span_weight(Some(650.0), false, SEGOE_VAR);
        let h4 = span_weight(Some(600.0), false, SEGOE_VAR);
        assert!(h1 > h2 && h2 > h4 && h4 > 400.0);
        assert!(span_weight(Some(650.0), true, SEGOE_VAR) > h2);
        assert_eq!(span_weight(None, true, SEGOE_VAR), 700.0);
        assert_eq!(span_weight(None, false, SEGOE_VAR), 400.0);
    }

    #[test]
    fn weights_stay_inside_the_axis() {
        let range = (450.0, 550.0);
        for block in [None, Some(600.0), Some(700.0)] {
            for bold in [false, true] {
                let w = span_weight(block, bold, range);
                assert!((450.0..=550.0).contains(&w), "{block:?} {bold} -> {w}");
            }
        }
    }

    #[test]
    fn tilde_expansion() {
        let home = Path::new("/home/u");
        assert_eq!(expand_with("~/fonts/x.ttf", Some(home)), home.join("fonts/x.ttf"));
        assert_eq!(expand_with("~/x.ttf", None), PathBuf::from("~/x.ttf"));
        assert_eq!(expand_with("x.ttf", Some(home)), PathBuf::from("x.ttf"));
        assert_eq!(expand_with("~x.ttf", Some(home)), PathBuf::from("~x.ttf"));
        #[cfg(windows)]
        assert_eq!(
            expand_with(r"~\Fonts\x.ttf", Some(Path::new(r"C:\Users\u"))),
            PathBuf::from(r"C:\Users\u\Fonts\x.ttf")
        );
    }

    #[test]
    fn explicit_paths() {
        assert!(is_explicit_path("~/x.ttf"));
        assert!(!is_explicit_path("x.ttf"));
        assert!(!is_explicit_path("UbuntuSans[wdth,wght].ttf"));
        #[cfg(unix)]
        assert!(is_explicit_path("/usr/share/fonts/x.ttf"));
        #[cfg(windows)]
        {
            assert!(is_explicit_path(r"C:\Windows\Fonts\segoeui.ttf"));
            assert!(is_explicit_path("C:/fonts/x.ttf"));
            assert!(is_explicit_path(r"~\x.ttf"));
            assert!(is_explicit_path(r"\\server\share\x.ttf"));
            assert!(!is_explicit_path(r"fonts\x.ttf"));
        }
    }

    #[test]
    fn detects_cjk_scripts() {
        assert_eq!(scripts("plain *Markdown* — with “quotes” and émoji 🎉"), Scripts::default());
        assert_eq!(scripts("中文文本"), Scripts { han: true, ..Default::default() });
        assert_eq!(scripts("ひらがなとカタカナ"), Scripts { kana: true, ..Default::default() });
        assert_eq!(scripts("日本語のテキスト"), Scripts { han: true, kana: true, ..Default::default() });
        assert_eq!(scripts("한국어"), Scripts { hangul: true, ..Default::default() });
        assert_eq!(scripts("全角，标点。"), Scripts { han: true, ..Default::default() });
    }

    #[test]
    fn detects_symbols() {
        for s in ["done ✓", "✗ failed", "a ⇒ b", "☐ todo", "→"] {
            assert_eq!(scripts(s), Scripts { symbols: true, ..Default::default() }, "{s}");
        }
        assert_eq!(scripts("ASCII, Latin-1 é ü ß, € and ‘curly’ “quotes” … – —"), Scripts::default());
        assert_eq!(scripts("中文 ✓"), Scripts { han: true, symbols: true, ..Default::default() });
    }

    #[test]
    fn line_metrics_from_sfnt_tables() {
        // A minimal sfnt: table directory with head, hhea and OS/2, 2048 units per em.
        // hhea's descent is -512 (0 when its ascent is 0) and its gap always 205; typo's
        // descent and gap are -409 and 102 (0 when its ascent is 0); Windows' ascent and
        // descent are 1024 and 256.
        fn font(hhea_asc: i16, typo_asc: i16, use_typo: bool, ttc: bool) -> Vec<u8> {
            let base = if ttc { 16 } else { 0 };
            let dir = base + 12 + 3 * 16;
            let (head, hhea, os2) = (dir, dir + 54, dir + 54 + 36);
            let mut f = vec![0u8; os2 + 78];
            if ttc {
                f[..4].copy_from_slice(b"ttcf");
                f[12..16].copy_from_slice(&(base as u32).to_be_bytes());
            }
            f[base + 4..base + 6].copy_from_slice(&3u16.to_be_bytes());
            for (i, (tag, off)) in [(b"head", head), (b"hhea", hhea), (b"OS/2", os2)].into_iter().enumerate() {
                let rec = base + 12 + 16 * i;
                f[rec..rec + 4].copy_from_slice(tag);
                f[rec + 8..rec + 12].copy_from_slice(&(off as u32).to_be_bytes());
            }
            f[head + 18..head + 20].copy_from_slice(&2048u16.to_be_bytes());
            if hhea_asc != 0 {
                f[hhea + 4..hhea + 6].copy_from_slice(&hhea_asc.to_be_bytes());
                f[hhea + 6..hhea + 8].copy_from_slice(&(-512i16).to_be_bytes());
            }
            f[hhea + 8..hhea + 10].copy_from_slice(&205i16.to_be_bytes());
            f[os2 + 62..os2 + 64].copy_from_slice(&(if use_typo { 0x80u16 } else { 0 }).to_be_bytes());
            f[os2 + 68..os2 + 70].copy_from_slice(&typo_asc.to_be_bytes());
            if typo_asc != 0 {
                f[os2 + 70..os2 + 72].copy_from_slice(&(-409i16).to_be_bytes());
                f[os2 + 72..os2 + 74].copy_from_slice(&102i16.to_be_bytes());
            }
            f[os2 + 74..os2 + 76].copy_from_slice(&1024u16.to_be_bytes());
            f[os2 + 76..os2 + 78].copy_from_slice(&256u16.to_be_bytes());
            f
        }
        let m = |a: i16, d: i16, g: i16| Some(LineMetrics { ascent: a as f32 / 2048.0, descent: d as f32 / 2048.0, line_gap: g as f32 / 2048.0 });
        assert_eq!(line_metrics_em(&font(2048, 1536, false, false)), m(2048, -512, 205));
        assert_eq!(line_metrics_em(&font(2048, 1536, true, false)), m(1536, -409, 102));
        assert_eq!(line_metrics_em(&font(2048, 1536, false, true)), m(2048, -512, 205));
        // hhea ascent and descent zero: typo metrics, then the Windows ones with hhea's gap.
        assert_eq!(line_metrics_em(&font(0, 1536, false, false)), m(1536, -409, 102));
        assert_eq!(line_metrics_em(&font(0, 0, false, false)), m(1024, -256, 205));
        assert_eq!(line_metrics_em(b"not a font"), None);
        assert_eq!(line_metrics_em(&[]), None);
    }

    /// Where epaint puts a face's baseline in a line of the given height: it centres the
    /// face's row in the line (egui 0.37 text_layout.rs `galley_from_rows`).
    fn epaint_baseline(m: LineMetrics, size: f32, line_height: f32) -> f32 {
        size * m.ascent + 0.5 * (line_height - size * (m.ascent - m.descent + m.line_gap))
    }

    #[test]
    fn code_shift_puts_code_on_the_body_baseline() {
        // Segoe UI Variable and Cascadia Mono, as on Windows 11.
        let sans = LineMetrics { ascent: 1.079, descent: -0.251, line_gap: 0.0 };
        let mono = LineMetrics { ascent: 0.928, descent: -0.234, line_gap: 0.0 };
        let shift = code_baseline_shift(sans, mono).unwrap();
        assert!(shift > 0.0);
        for (size, lh) in [(16.0, 1.6), (24.0, 1.3), (13.0, 1.0)] {
            let code_size = CODE_SCALE * size;
            let body = epaint_baseline(sans, size, size * lh);
            let code = epaint_baseline(mono, code_size, size * lh) + shift * code_size;
            assert!((body - code).abs() < 1e-4, "{size} {lh}: {body} vs {code}");
        }
        // Code whose baseline already matches is left alone; one that sits low moves up.
        let same = LineMetrics { ascent: sans.ascent / CODE_SCALE, descent: sans.descent / CODE_SCALE, line_gap: 0.0 };
        assert_eq!(code_baseline_shift(sans, same), None);
        assert!(code_baseline_shift(mono, sans).unwrap() < 0.0);
    }

    #[cfg(windows)]
    #[test]
    fn line_metrics_of_installed_fonts() {
        for file in ["SegUIVar.ttf", "consola.ttf", "CascadiaMono.ttf", "YuGothM.ttc"] {
            if let Some(bytes) = read(file) {
                let m = line_metrics_em(&bytes).unwrap();
                assert!((0.6..1.3).contains(&m.ascent), "{file}: {m:?}");
                assert!((-0.6..0.0).contains(&m.descent), "{file}: {m:?}");
                // Yu Gothic's is 0.5.
                assert!((0.0..1.0).contains(&m.line_gap), "{file}: {m:?}");
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn segoe_ui_variable_has_a_wght_axis() {
        let Some(bytes) = read("SegUIVar.ttf") else { return };
        assert_eq!(wght_axis(&FontData::from_owned(bytes)), Some((300.0, 700.0)));
        if let Some(bytes) = read("segoeui.ttf") {
            assert_eq!(wght_axis(&FontData::from_owned(bytes)), None);
        }
    }
}
