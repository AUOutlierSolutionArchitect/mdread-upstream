# Plan

Follow-ups from the review of PR #9 (small fixes), left out of that PR to be solved separately.

## Keep the reading position when content above it grows

A block whose image or diagram is still loading is laid out again every frame, even far off
screen (`lay_out_block` in `src/app.rs`). When a slow image or diagram above the view
arrives, its block grows from its placeholder height and the text being read moves down by
that much. Before PR #9 the same jump happened later, when scrolling back near the block,
so this is not new, but it is visible.

The fix is scroll anchoring: when a block entirely above the viewport changes height,
scroll by the difference so the visible content stays put. It must not apply while a
restored position (history Back, reload, session restore at launch) or an anchor jump is
settling, because those rely on the blocks above growing back to their full heights. One
way: anchor only once the user has scrolled since the document was opened, or once the
page has settled.

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
