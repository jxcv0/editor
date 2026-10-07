# Embedded GUI font

`FiraCodeNerdFontMono-Regular.ttf` is the unmodified Regular / Mono face from
[Nerd Fonts v3.4.0](https://github.com/ryanoasis/nerd-fonts/releases/tag/v3.4.0),
based on [Fira Code 6.2](https://github.com/tonsky/FiraCode/releases/tag/6.2).
It was copied from the locally available `FiraCode.zip` release archive. The
Mono variant keeps Nerd Font icons within one cell and preserves programming
ligatures. See the [upstream release README](https://github.com/ryanoasis/nerd-fonts/blob/v3.4.0/patched-fonts/FiraCode/README.md).

SHA-256:

```text
ad88c69cb6a497db9f2714e4b414817aabbee621484a1560bfdb3fd73abdd564
```

The font is distributed under the SIL Open Font License 1.1, with the full
copyright notice and license in [FiraCode-LICENSE.txt](FiraCode-LICENSE.txt).
It is separate from the editor's MIT-licensed source code. Preserve this
notice and license when redistributing the font.
The full license is also embedded in GUI binaries and readable through the
titlebar's **editor → FiraCode font license** menu, including standalone installs.

Only the optional `gui` feature embeds the 2,647,492-byte font. Both egui font
families prefer it and retain egui's bundled fallback fonts. There is no
runtime font discovery or download, and a terminal-only build does not embed
this asset or change the terminal emulator's selected font.

The shared canvas keeps grapheme/cell positions. `gui::font` applies the
embedded font's ASCII `calt` substitutions within runs of identically styled
cells and rasterizes substituted glyphs through `ab_glyph`. This is a small,
font-specific evaluator, not a general Unicode shaping engine. Unicode
clusters and fallback characters continue through egui. Changing this asset
requires reviewing its GSUB program and refreshing the HarfBuzz fixtures;
tests audit every transitively referenced lookup and the reachable glyphs'
cell advances.
