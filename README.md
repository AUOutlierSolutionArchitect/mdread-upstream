# mdread

A very fast, GPU-rendered Markdown viewer for Linux, macOS and Windows 11, built
for large high-DPI screens. Rust + [egui](https://github.com/emilk/egui) (OpenGL,
with a Direct3D 12 fallback on Windows), parses with pulldown-cmark, highlights
code with syntect.

- Cold start to first frame in about 100 ms; an 800 KB / 1100-block document
  parses in ~4 ms and renders in under 2 ms per frame (only visible blocks are
  laid out).
- Crisp text at any scale, including fractional Wayland scaling and Retina,
  using the system's Ubuntu Sans, SF Pro or Segoe UI Variable (variable weight)
  and JetBrains Mono, Cascadia or any installed Nerd Font.
- Widescreen layout: outline sidebar that tracks the section you are reading,
  files sidebar for the rest of the folder, and a reading column capped at a
  comfortable measure. Widen it from the status bar (`−` `860pt` `+`, or click
  the width for presets) when tables or long lines need the room.
- Live reload when the file changes on disk.
- Dark and light themes, following the system by default.
- GFM: tables, task lists, strikethrough, footnotes, alerts (`> [!NOTE]`),
  definition lists, front matter, math (shown as monospace), local and remote
  images (PNG, JPEG, GIF, WebP, SVG), inline badges, HTML `<img>`.
- Links: web links open in the browser, relative `.md` links open in the
  viewer (back and forward with `←` `→` in the status bar), `#anchors` jump
  within the document. The files sidebar keeps the folder you came from in
  view; `↑` or a click on its folder name shows another.
- Mermaid diagrams (```` ```mermaid ```` blocks) drawn natively, without a
  browser: flowcharts, sequence, class, state, ER, gantt, pie and the other
  Mermaid types, in Mermaid's dark theme on the dark page. A diagram wider than
  the column spreads into the margins beside it.
- Search with highlighted matches, copy button on code blocks, text selection,
  right-click menu to copy the selection or open and copy links.

## Build

```bash
cargo build --release          # binary at target/release/mdread
./install.sh                   # installs to ~/.local/bin, adds a desktop entry and
                               # icons, and makes mdread the default for Markdown files
```

Requires rustc 1.95+. Ubuntu 26.04's own Rust packages stop at 1.93, so install
the toolchain with rustup: `sudo apt install rustup && rustup default stable`
(this replaces the `cargo` and `rustc` packages). No system GUI libraries are needed beyond Mesa/Wayland/X11 runtime libraries.
No packaging (.deb) is needed: a per-user desktop entry is enough for GNOME,
Nautilus and `xdg-open`. The installer writes the binary's absolute path into
the entry because GNOME launches apps with a `PATH` that does not include
`~/.local/bin`, and GLib silently drops entries whose `Exec` it cannot resolve.
To undo the default: `xdg-mime default org.gnome.TextEditor.desktop text/markdown`.
The installer also puts the app icon into `~/.local/share/icons/hicolor`, which
every icon theme falls back to, so the dock, window switcher and app grid show
mdread's own icon rather than a placeholder (icon sources and a render script
are in `assets/icons`). The icon is embedded in the binary too, which covers the
X11 window icon and the macOS Dock while mdread is running.

On macOS, `cargo build --release` works unchanged (install the toolchain with
`brew install rustup && rustup default stable`; the build needs Xcode Command
Line Tools and nothing else). `./install.sh` copies the binary to `~/.local/bin`
and skips the desktop entry. Shortcuts use `⌘` where the table below says `Ctrl`,
and the file dialog is the native panel instead of zenity.

### Windows

Install Rust with [rustup](https://rustup.rs) (the default MSVC toolchain) and
Visual Studio 2022 Build Tools with the "Desktop development with C++" workload
(MSVC and the Windows SDK, whose `rc.exe` embeds the icon and version info).

```powershell
cargo build --release                                  # target\release\mdread.exe
powershell -ExecutionPolicy Bypass -File .\install.ps1 # install for the current user
```

`mdread.exe` is a single self-contained file: `.cargo/config.toml` links the C
runtime statically, so no VC++ redistributable is needed (`cargo install` run
from outside the checkout does not read that file). `install.ps1` builds,
copies the exe to `%LOCALAPPDATA%\Programs\mdread`, adds a Start menu shortcut,
puts the folder on the user `PATH` (open a new terminal afterwards), registers
App Paths (so Win+R `mdread` works) and registers mdread for `.md .markdown
.mdown .mkd .mdx` under HKCU; no admin rights needed. Switches: `-NoBuild`
installs the existing exe, `-NoAssociate` skips the file types, `-NoDefaultApps`
skips opening Settings, `-Uninstall` removes everything it added. Running
`install.sh` from Git Bash just points you here.

To build a setup program instead, install [Inno Setup](https://jrsoftware.org/isinfo.php)
6.3+ or 7 and run `ISCC packaging\windows\mdread.iss` from the repo root after
`cargo build --release`. That writes `target\installer\mdread-setup-<version>.exe`
(version from Cargo.toml; override with `/DAppVersion=x.y.z`). The setup installs
per user without an admin prompt into the same folder and registers the same
keys as `install.ps1`, with a Start menu shortcut, optional desktop shortcut and
an "Add to PATH" task (on by default). It closes a running mdread when upgrading.
Uninstall it from Settings > Apps; `install.ps1 -Uninstall` refuses to touch a
setup install. Both keep your settings. The builds are unsigned, so SmartScreen
asks for "More info > Run anyway" when a copied or downloaded setup is run.

Windows does not let a program make itself the default app: the choice is
stored with a hash that only Settings writes. Both installers therefore list
mdread in "Open with" and Settings > Default apps, make it the default only for
extensions no other app has claimed, and finish by opening Settings > Default
apps on mdread's page (skipped when mdread already opens `.md` files). Pick
mdread there, or use "Open with > Choose another app > Always" on a `.md` file.

mdread draws with OpenGL and switches to Direct3D 12 by itself when OpenGL is
not usable, which includes the WARP software renderer in VMs, Remote Desktop
and Windows Sandbox. `MDREAD_RENDERER=glow` or `wgpu` forces one of the two.
Native ARM64 builds need clang on `PATH` and are untested.

## Usage

```bash
mdread README.md
mdread docs/guide.md#installation     # open at a heading
mdread notes.md --find TODO           # open with the search box filled in
cat file.md | mdread -                # from stdin
mdread --light file.md                # force a theme for this session
```

Open the file dialog with `Ctrl+O` (`⌘O` on macOS; uses zenity on Linux, the
standard Open dialog on Windows) or drop a file onto the window. On Windows a
dropped folder opens its README.md or index.md (else its first Markdown file),
and a dropped shortcut opens its target.

Files may be UTF-8 or, with a byte order mark, UTF-16; anything else is read
in the system ANSI code page on Windows and as Windows-1252 elsewhere, and the
status bar says "Decoded as …". Line endings do not matter.

In PowerShell, `<` does not exist and Windows PowerShell 5.1 pipes non-ASCII
text as `?`, so open files by name where you can:

```powershell
mdread README.md
Get-Content -Raw -Encoding UTF8 file.md | mdread -     # PowerShell 7: drop -Encoding
mdread - < file.md                                     # cmd.exe
```

(in 5.1, set `$OutputEncoding = [Text.UTF8Encoding]::new($false)` first.)
mdread is a GUI program, so `--help` and `--version` print into the terminal you
started it from, possibly after the next prompt, and show a message box when
there is no terminal. In PowerShell, pipe instead of redirecting
(`mdread --help | Out-File help.txt`, `mdread --version | Out-String`): `>` and
`$x = mdread ...` capture nothing and `$LASTEXITCODE` is not set. In cmd.exe `>`
works.

On Windows, links leave the app only for `http`, `https` and `mailto`. A link to
a local file opens it in its default app only for images, PDF, plain text, data
files (CSV, JSON, YAML, TOML) and audio/video; anything else (exe, scripts,
shortcuts, HTML, Office documents) is shown selected in Explorer rather than run.
Images and links on network (UNC, `//host`) paths are blocked unless they are on
the same share as the open document.

## Keys

| Keys | Action |
|---|---|
| `j` `k` `↓` `↑` | scroll by line |
| `Space` `PgDn` / `Shift+Space` `PgUp` | scroll by page |
| `d` `u` | half page |
| `g` `G` `Home` `End` | top / bottom |
| `/` `Ctrl+F` | find; `Enter` `n` `N` next / previous |
| `o` | outline sidebar |
| `f` | files sidebar |
| `t` | dark / light |
| `w` | full-width column; `[` `]` narrower / wider in small steps |
| `Ctrl +` `Ctrl -` `Ctrl 0`, `Ctrl`+scroll | zoom |
| `Alt+←` `Backspace` / `Alt+→` | back / forward (also the mouse side buttons, and `←` `→` in the status bar) |
| `r` `F5` | reload |
| `?` `F1` | help |
| `q` `Ctrl+Q` | quit |

Settings (zoom, column width, sidebars, theme, window size) persist between runs,
in `~/.local/share/mdread`, `~/Library/Application Support/mdread` or
`%APPDATA%\mdread\data`.

## Fonts

Body text uses the first of: Ubuntu Sans (variable), Ubuntu, Noto Sans,
DejaVu Sans, SF Pro (macOS, variable), Arial. Code uses the first of: JetBrains
Mono, any installed [Nerd Font](https://www.nerdfonts.com/) (`*NerdFontMono-Regular.ttf`,
then `*NerdFont-Regular.ttf`), Ubuntu Sans Mono, Ubuntu Mono, Fira Code, DejaVu
Sans Mono, SF Mono, Monaco. Fonts are found by file name in the user font
directories (`~/.local/share/fonts`, `~/.fonts`, `~/Library/Fonts`) before the
system ones, so installing a preferred font per user is enough. Override with
`MDREAD_FONT=/path/to/font.ttf` and `MDREAD_MONO_FONT=...`.

On Windows, body text is Segoe UI Variable (bold through its weight axis,
italics slanted), else Segoe UI, else Arial; code is any installed Nerd Font,
else Cascadia Mono / Cascadia Code, else Consolas. Fonts installed "for me" in
`%LOCALAPPDATA%\Microsoft\Windows\Fonts` win over `%WINDIR%\Fonts`. Chinese,
Japanese and Korean text use Microsoft YaHei UI, Yu Gothic UI and Malgun Gothic,
and arrows, check marks and other symbols Segoe UI Symbol; each is loaded the
first time a document needs it. Overrides take a full path, a `~\` path or a
bare file name from the font folders:

```powershell
$env:MDREAD_FONT = 'C:\Windows\Fonts\segoeui.ttf'
$env:MDREAD_MONO_FONT = 'consola.ttf'
```

egui renders text with greyscale antialiasing (no ClearType) and monochrome
emoji, and lays out Arabic and Hebrew left to right without shaping.

Diagram labels use Trebuchet MS, Mermaid's default font. Windows and macOS have
it; on Linux it comes with the Microsoft core fonts (`ttf-mscorefonts-installer`),
and without it diagrams use Arial, Liberation Sans, Helvetica, Noto Sans or
DejaVu Sans. Boxes are sized to the labels as that font draws them. Characters
those fonts lack are taken from any installed font that has them. The installed
fonts are scanned the first time a document has a diagram.

## Debugging

```bash
RUST_LOG=mdread=debug mdread file.md            # parse timing, image errors
RUST_LOG=mdread=trace mdread file.md            # per-frame timing
mdread --screenshot out.png --screenshot-delay 1500 file.md   # headless-ish capture
```

In PowerShell: `$env:RUST_LOG = 'mdread=debug'; mdread file.md`. Release builds
on Windows have no console, so the log goes to `%LOCALAPPDATA%\mdread\mdread.log`
(debug builds from `cargo run` keep one). Startup errors and crashes are shown
in a message box and appended to `%LOCALAPPDATA%\mdread\crash.log`.

## License

[MIT](LICENSE).
