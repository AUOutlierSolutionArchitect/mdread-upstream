//! mdread — a very fast GPU-rendered Markdown viewer.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod document;
mod files;
mod fonts;
mod highlight;
mod links;
mod mermaid;
mod render;
mod search;
#[cfg(windows)]
mod svg_loader;
mod text;
mod theme;
#[cfg(windows)]
mod win;

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use app::{Args, ThemeChoice};

const HELP: &str = "\
mdread — fast Markdown viewer

USAGE:
    mdread [OPTIONS] [FILE[#anchor]]
    cat FILE.md | mdread -

OPTIONS:
    --dark, --light      Force a theme for this session (default: follow the system)
    --find TERM          Open with the search box filled in
    --screenshot PATH    Save a PNG of the window after 1.5 s and exit
    --screenshot-delay N Delay in ms for --screenshot
    -h, --help           Show this help
    -V, --version        Show version

ENVIRONMENT:
    MDREAD_FONT          Path to a .ttf/.otf to use for body text
    MDREAD_MONO_FONT     Path to a .ttf/.otf to use for code
";

/// Makes stdout/stderr reach the terminal before printing to stderr (`err`) or stdout.
/// Release builds on Windows have no console of their own; returns false if that stream
/// has nowhere to go.
pub fn cli_console(err: bool) -> bool {
    #[cfg(windows)]
    return win::attach_console(err);
    #[cfg(not(windows))]
    {
        let _ = err;
        true
    }
}

/// Prints CLI output (help, version, usage errors) and exits. On Windows, without a
/// terminal to print to, the text is shown in a message box instead.
fn exit_with(text: &str, code: i32) -> ! {
    use std::io::Write;
    if cli_console(code != 0) {
        // Not print!: it panics when the reader has gone away (`mdread --help | head -1`).
        let mut out: Box<dyn Write> = if code == 0 { Box::new(std::io::stdout()) } else { Box::new(std::io::stderr()) };
        let _ = out.write_all(text.as_bytes()).and_then(|_| out.flush());
    } else {
        #[cfg(windows)]
        win::message_box(text.trim_end(), code != 0);
    }
    std::process::exit(code);
}

fn parse_args() -> Args {
    parse_args_from(std::env::args_os().skip(1), |p| p.exists())
}

/// `parse_args` over a given argument list, with `exists` standing in for the file system.
fn parse_args_from(argv: impl IntoIterator<Item = OsString>, exists: impl Fn(&Path) -> bool) -> Args {
    let mut args = Args { screenshot_delay_ms: 1500, ..Args::default() };
    let mut it = argv.into_iter();
    while let Some(a) = it.next() {
        let s = a.to_string_lossy();
        match s.as_ref() {
            "--screenshot" => args.screenshot = it.next().map(PathBuf::from),
            "--find" => args.find = it.next().map(|v| v.to_string_lossy().to_string()),
            "--screenshot-delay" => {
                args.screenshot_delay_ms = it.next().and_then(|v| v.to_string_lossy().parse().ok()).unwrap_or(1500);
            }
            "-h" | "--help" => exit_with(HELP, 0),
            "-V" | "--version" => exit_with(&format!("mdread {}\n", env!("CARGO_PKG_VERSION")), 0),
            "--dark" => args.theme = Some(ThemeChoice::Dark),
            "--light" => args.theme = Some(ThemeChoice::Light),
            "--system" => args.theme = Some(ThemeChoice::System),
            "-" => args.stdin = true,
            _ if s.starts_with("--") => exit_with(&format!("unknown option {s}\n{HELP}\n"), 2),
            _ => {
                let (file, anchor) = split_anchor(&a, &exists);
                // Only the first file is opened, so only its anchor means anything.
                if args.files.is_empty() {
                    args.anchor = anchor;
                }
                args.files.push(file);
            }
        }
    }
    args
}

/// Splits `file.md#section` so the viewer can open straight at a heading. Splits at the
/// last `#`, and only when the whole argument is not an existing path but the part before
/// the `#` is, so names like `C#/notes.md` or `issue#12.md` keep working.
fn split_anchor(arg: &OsStr, exists: impl Fn(&Path) -> bool) -> (PathBuf, Option<String>) {
    if let Some((file, frag)) = arg.to_str().and_then(|s| s.rsplit_once('#')) {
        if !exists(Path::new(arg)) && exists(Path::new(file)) {
            return (PathBuf::from(file), Some(frag.to_owned()));
        }
    }
    (PathBuf::from(arg), None)
}

/// Window icon, used for `_NET_WM_ICON` on X11, the Dock on macOS and the title bar and
/// task bar button on Windows (Explorer and shortcuts use the icon resource that build.rs
/// embeds). Wayland compositors ignore it and take the icon from the desktop entry that
/// matches the app id instead, which `install.sh` installs together with the icon files.
fn app_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/icons/hicolor/256x256/apps/mdread.png"))
        .expect("embedded icon PNG is valid")
}

fn init_logging() {
    // usvg, its CSS parser and tiny-skia warn about routine things in diagrams (a fallback
    // font for each character a font lacks, CSS selectors they skip, unfillable straight
    // paths); RUST_LOG still shows them. arboard warns at startup on GNOME, which lacks the
    // Wayland clipboard protocol it tries first; egui uses smithay-clipboard there anyway.
    let filter = "warn,usvg=error,simplecss=error,tiny_skia=error,arboard::platform::linux=error,arboard::platform::linux::x11=warn";
    let mut builder = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(filter));
    // Release builds on Windows have no stderr; send RUST_LOG output to a file instead.
    #[cfg(windows)]
    if !cfg!(debug_assertions) && std::env::var_os("RUST_LOG").is_some() {
        if let Some((file, _)) = win::open_log("mdread.log") {
            use std::io::Write;
            builder.target(env_logger::Target::Pipe(Box::new(file))).format(|buf, r| {
                writeln!(buf, "[{} {} {:5} {}] {}", win::timestamp(), std::process::id(), r.level(), r.target(), r.args())
            });
        }
    }
    builder.init();
}

fn native_options() -> eframe::NativeOptions {
    const SIZE: egui::Vec2 = egui::vec2(1500.0, 1250.0);
    let viewport = egui::ViewportBuilder::default()
        .with_title("mdread")
        .with_app_id("mdread")
        .with_icon(app_icon())
        .with_inner_size(SIZE)
        .with_min_inner_size([420.0, 300.0]);
    // eframe only clamps the first-run size to the whole monitor, which leaves the bottom
    // of the window under the taskbar on typical laptop screens.
    #[cfg(windows)]
    let viewport = match win::first_run_geometry(SIZE) {
        Some((pos, size)) => viewport.with_position(pos).with_inner_size(size),
        None => viewport,
    };
    eframe::NativeOptions { viewport, persist_window: true, ..Default::default() }
}

fn run(native_options: eframe::NativeOptions, args: Args) -> eframe::Result {
    eframe::run_native(
        "mdread",
        native_options,
        Box::new(move |cc| {
            #[cfg(windows)]
            win::disarm_renderer_fallback();
            Ok(Box::new(app::App::new(cc, args)))
        }),
    )
}

/// Starts with glow and retries with wgpu (DX12, or WARP where there is no GPU driver)
/// if glow fails before the window is up, e.g. in VMs and RDP sessions that only offer
/// OpenGL 1.1. `MDREAD_RENDERER=glow|wgpu` (any case) forces one renderer; any other
/// value is ignored with a warning.
#[cfg(windows)]
fn run_windows(args: Args) -> eframe::Result {
    let forced = std::env::var(win::RENDERER_ENV).ok().map(|v| v.trim().to_ascii_lowercase());
    let forced = match forced.as_deref() {
        Some("glow" | "wgpu") | None => forced,
        Some(v) => {
            log::warn!("ignoring {}={v:?}; expected glow or wgpu", win::RENDERER_ENV);
            None
        }
    };
    if forced.as_deref() != Some("wgpu") {
        win::arm_renderer_fallback(forced.is_none());
        let result = run(eframe::NativeOptions { renderer: eframe::Renderer::Glow, ..native_options() }, args.clone());
        match result {
            Err(e) if win::disarm_renderer_fallback() => log::warn!("glow renderer failed ({e}); retrying with wgpu"),
            r => return r,
        }
    }
    run(eframe::NativeOptions { renderer: eframe::Renderer::Wgpu, ..native_options() }, args)
}

fn main() -> eframe::Result {
    let launched = std::time::Instant::now();
    #[cfg(windows)]
    win::install_panic_hook();
    init_logging();
    let mut args = parse_args();
    args.launched = Some(launched);

    #[cfg(not(windows))]
    let result = run(native_options(), args);
    #[cfg(windows)]
    let result = run_windows(args);
    #[cfg(windows)]
    if let Err(e) = &result {
        win::report_fatal(&format!("mdread stopped with an error:\n{e}"), &format!("run_native failed: {e:?}"));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(arg: &str, existing: &[&str]) -> (PathBuf, Option<String>) {
        split_anchor(OsStr::new(arg), |p| existing.iter().any(|e| Path::new(e) == p))
    }

    #[test]
    fn anchor_is_split_off_an_existing_file() {
        assert_eq!(split("docs/guide.md#install", &["docs/guide.md"]), (PathBuf::from("docs/guide.md"), Some("install".into())));
        assert_eq!(split(r"C:\x.md#h", &[r"C:\x.md"]), (PathBuf::from(r"C:\x.md"), Some("h".into())));
    }

    #[test]
    fn hash_in_directory_or_file_name_is_kept() {
        assert_eq!(split(r"C:\src\C#\notes.md#intro", &[r"C:\src\C#\notes.md"]), (PathBuf::from(r"C:\src\C#\notes.md"), Some("intro".into())));
        assert_eq!(split("src/C#/notes.md", &["src/C#/notes.md"]), (PathBuf::from("src/C#/notes.md"), None));
        assert_eq!(split("issue#12.md", &["issue#12.md", "issue"]), (PathBuf::from("issue#12.md"), None));
    }

    #[test]
    fn missing_files_are_passed_through_unchanged() {
        assert_eq!(split("missing.md#top", &[]), (PathBuf::from("missing.md#top"), None));
        assert_eq!(split("plain.md", &[]), (PathBuf::from("plain.md"), None));
    }

    fn parse(argv: &[&str], existing: &[&str]) -> Args {
        parse_args_from(argv.iter().map(OsString::from), |p| existing.iter().any(|e| Path::new(e) == p))
    }

    #[test]
    fn only_the_first_files_anchor_is_used() {
        // Only the first file is opened, so an anchor on a later one points into a page
        // that is not shown.
        let both = ["a.md", "b.md"];
        let args = parse(&["a.md", "b.md#x"], &both);
        assert_eq!(args.files, [PathBuf::from("a.md"), PathBuf::from("b.md")]);
        assert_eq!(args.anchor, None);
        assert_eq!(parse(&["a.md#x", "b.md#y"], &both).anchor, Some("x".into()));
    }
}
