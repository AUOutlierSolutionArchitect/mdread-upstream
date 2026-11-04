# AGENTS.md

mdread is a fast, GPU-rendered Markdown viewer for Linux, macOS and Windows 11, written in
Rust with egui/eframe 0.37. It parses with pulldown-cmark, highlights code with syntect,
and draws Mermaid diagrams natively with merman and resvg. See README.md for features,
keys and installation.

## Startup time comes first

mdread exists to open a Markdown file faster than anything else. Cold start to first frame
is about 100 ms (README.md says so), and every change must keep it there or make it
shorter.

- Do nothing before the first frame that the first frame does not need. Load or scan
  anything else lazily, on a worker thread, or the first time it is used. The code already
  works this way: the syntax set loads on the highlighter's thread, the installed fonts are
  scanned only when a document first has a diagram, only visible blocks are laid out, and
  the Windows build leaves out `svg_text` because it scans every system font at startup.
  Follow those patterns.
- Measure before and after any change that touches the startup path: `main.rs`, `App::new`,
  font setup, the first `set_document`, and new dependencies or features. Run a release
  build with `RUST_LOG=mdread=info` (debug for parse timing) and read the
  `first frame N ms after launch` line. Compare several cold runs of each binary, and put
  the numbers in the PR.
- Be wary of new dependencies and crate features. They cost binary size, link and
  load time, and often hidden initialisation (font scans, registries, thread pools).
- Where startup speed and a feature conflict, startup wins unless the user decides
  otherwise. Make the feature lazy, or say what it costs.

## Build and test

```bash
cargo build --release              # target/release/mdread
cargo test --release               # unit tests (faster than debug here)
cargo clippy --release --all-targets   # main is clippy-clean; a new warning is yours
```

- rustc 1.95+ (`rust-version` in Cargo.toml). `.cargo/config.toml` keeps dependency
  resolution within it.
- Do not run `cargo fmt`. The code is not formatted with rustfmt's defaults (lines are
  longer), and `cargo fmt` rewrites every file. Match the surrounding style by hand.
- GUI checks: `mdread --screenshot out.png [--screenshot-delay MS] file.md[#anchor]`
  saves a PNG once diagrams, images and highlighting are done, then exits. Run it on a
  hidden display (for example `Xvfb :9` and `DISPLAY=:9 env -u WAYLAND_DISPLAY ...`), not
  on the desktop of the person you are working for, because test windows take focus. Set
  `XDG_DATA_HOME` to a scratch directory, since mdread keeps its settings in
  `$XDG_DATA_HOME/mdread`. `cargo test` does not rebuild the binary; run `cargo build`
  before screenshots.
- `samples/showcase.md` exercises most rendering features.
- Windows code (`src/win.rs`, `install.ps1`, `packaging/windows/mdread.iss`) can be
  type-checked from Linux with `cargo check --target x86_64-pc-windows-msvc --all-targets`.
  Build scripts with C code need stub `cl.exe`/`lib.exe` on PATH for that.

## Layout

- `src/main.rs`: command line and eframe setup. `src/app.rs`: the app, input, scrolling,
  history and the reading column. `src/render.rs`: draws blocks. `src/document.rs`:
  Markdown to blocks.
- `src/mermaid.rs`: diagram layout and drawing on a worker thread, and diagram fonts.
  `src/highlight.rs`: syntax highlighting on a worker thread. `src/fonts.rs`: UI fonts and
  script fallbacks.
- `src/files.rs`, `src/links.rs`, `src/search.rs`, `src/text.rs`, `src/theme.rs`,
  `src/svg_loader.rs`, `src/win.rs` (Windows only).
- `PLAN.md`: follow-ups noted for later.

## Conventions

- Comments and docs explain why, in plain British English, as the existing ones do.
- PRs merge with merge commits.
