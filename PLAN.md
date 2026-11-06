# Plan

Follow-ups from the review of PR #9 (small fixes), left out of that PR to be solved separately.

## Diagram labels after a keycap emoji

In a label like `1️⃣ 日本`, the CJK text is drawn as missing glyphs. usvg's fallback loop
(`shape_text` in usvg 0.45 `text/layout.rs`) gives up as soon as a fallback font shapes the
text to a different number of glyphs. A colour emoji font turns the keycap sequence into
one glyph, and usvg's default selector picks it for the `1` before mdread's
`load_fallback` is asked. Options: wrap the default selector so that it offers fonts in a
better order (emoji fonts last), or ask usvg upstream to replace clusters rather than give
up on a glyph count mismatch.

## Installers: whitespace-only or `; `-terminated user PATH

`Update-UserPath` in `install.ps1` and `UpdateUserPath` in `packaging/windows/mdread.iss`
keep the PATH character for character. With a PATH of `' '` or `'A; '`, installing gives
`' ;C:\...\mdread'` or `'A; ;C:\...\mdread'`: a harmless blank entry. Could treat a
whitespace-only PATH as empty and test for the trailing `;` after trimming trailing spaces.
Low priority.

## Smaller cleanups

- `run` in the `src/mermaid.rs` tests has a return type clippy calls very complex; give it
  a type alias.
- The `--screenshot` settle check (`handle_screenshot` in `src/app.rs`) scans every
  `Measure` each frame. `show_content` could set an any-pending flag as it lays blocks out
  instead. Cheap as it is, so only worth doing alongside other changes there.
