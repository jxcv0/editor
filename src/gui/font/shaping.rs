//! The embedded Fira Code's ASCII `calt` program. Fira draws programming
//! ligatures with contextual, one-for-one substitutions (including blank
//! spacer glyphs), so every input character retains its original cell.
//!
//! This is deliberately font-specific, not a general script shaper: Unicode
//! graphemes and fallback fonts stay on egui's existing path. Only the single
//! substitutions and chained contexts used by this font's ASCII repertoire
//! are evaluated. Tests pin the font and compare against HarfBuzz fixtures.
use ttf_parser::{
    Face, GlyphId, LazyArray16, Tag,
    gsub::{SingleSubstitution, SubstitutionSubtable},
    opentype_layout::{ChainedContextLookup, LayoutTable, SequenceLookupRecord},
};

pub(super) struct Calt {
    face: Face<'static>,
    table: LayoutTable<'static>,
    lookups: Vec<u16>,
}

impl Calt {
    pub(super) fn new(bytes: &'static [u8]) -> Self {
        let face = Face::parse(bytes, 0).expect("embedded Fira Code font");
        let table = face.tables().gsub.expect("Fira Code GSUB");
        let feature = table
            .features
            .find(Tag::from_bytes(b"calt"))
            .expect("Fira Code calt");
        let mut lookups: Vec<_> = feature.lookup_indices.into_iter().collect();
        lookups.sort_unstable();
        lookups.dedup();
        Self {
            face,
            table,
            lookups,
        }
    }

    pub(super) fn shape(&self, text: &str) -> Vec<GlyphId> {
        debug_assert!(text.bytes().all(|c| c.is_ascii_graphic() || c == b' '));
        let mut glyphs: Vec<_> = text
            .chars()
            .map(|c| self.face.glyph_index(c).unwrap())
            .collect();
        for &lookup in &self.lookups {
            let mut at = 0;
            while at < glyphs.len() {
                at += self.apply(lookup, &mut glyphs, at, 0).unwrap_or(1);
            }
        }
        glyphs
    }

    fn apply(&self, index: u16, glyphs: &mut [GlyphId], at: usize, depth: u8) -> Option<usize> {
        if depth >= 8 {
            return None;
        }
        let glyph = *glyphs.get(at)?;
        let lookup = self.table.lookups.get(index)?;
        for subtable in lookup.subtables.into_iter::<SubstitutionSubtable<'_>>() {
            match subtable {
                SubstitutionSubtable::Single(single) => {
                    let Some(index) = single.coverage().get(glyph) else {
                        continue;
                    };
                    glyphs[at] = match single {
                        SingleSubstitution::Format1 { delta, .. } => {
                            GlyphId(glyph.0.wrapping_add_signed(delta))
                        }
                        SingleSubstitution::Format2 { substitutes, .. } => {
                            substitutes.get(index)?
                        }
                    };
                    return Some(1);
                }
                SubstitutionSubtable::ChainContext(chain) => {
                    if let Some((count, records)) = context(chain, glyphs, at) {
                        for record in records {
                            self.apply(
                                record.lookup_list_index,
                                glyphs,
                                at + usize::from(record.sequence_index),
                                depth + 1,
                            );
                        }
                        // Matching an empty rule is significant: it inhibits a
                        // later substitution (e.g. operators in longer runs).
                        return Some(count);
                    }
                }
                // Other features and the Unicode-only calt decomposition are
                // outside this ASCII program. Its reachable forms are audited
                // by the embedded_font_program_is_cell_preserving test.
                _ => {}
            }
        }
        None
    }
}

fn matches<T>(
    glyphs: &[GlyphId],
    start: isize,
    step: isize,
    values: impl IntoIterator<Item = T>,
    equal: impl Fn(T, GlyphId) -> bool,
) -> bool {
    values.into_iter().enumerate().all(|(i, value)| {
        usize::try_from(start + i as isize * step)
            .ok()
            .and_then(|i| glyphs.get(i))
            .is_some_and(|&glyph| equal(value, glyph))
    })
}

fn context<'a>(
    chain: ChainedContextLookup<'a>,
    glyphs: &[GlyphId],
    at: usize,
) -> Option<(usize, LazyArray16<'a, SequenceLookupRecord>)> {
    let coverage_index = chain.coverage().get(glyphs[at])?;
    let before = at as isize - 1;
    let after = at as isize + 1;
    match chain {
        ChainedContextLookup::Format1 { sets, .. } => {
            for rule in sets.get(coverage_index)? {
                let count = usize::from(rule.input.len()) + 1;
                let equal = |value, glyph: GlyphId| value == glyph.0;
                if matches(glyphs, before, -1, rule.backtrack, equal)
                    && matches(glyphs, after, 1, rule.input, equal)
                    && matches(glyphs, (at + count) as isize, 1, rule.lookahead, equal)
                {
                    return Some((count, rule.lookups));
                }
            }
        }
        ChainedContextLookup::Format2 {
            sets,
            backtrack_classes,
            input_classes,
            lookahead_classes,
            ..
        } => {
            for rule in sets.get(input_classes.get(glyphs[at]))? {
                let count = usize::from(rule.input.len()) + 1;
                if matches(glyphs, before, -1, rule.backtrack, |c, g| {
                    c == backtrack_classes.get(g)
                }) && matches(glyphs, after, 1, rule.input, |c, g| {
                    c == input_classes.get(g)
                }) && matches(glyphs, (at + count) as isize, 1, rule.lookahead, |c, g| {
                    c == lookahead_classes.get(g)
                }) {
                    return Some((count, rule.lookups));
                }
            }
        }
        ChainedContextLookup::Format3 {
            backtrack_coverages,
            input_coverages,
            lookahead_coverages,
            lookups,
            ..
        } => {
            let count = usize::from(input_coverages.len()) + 1;
            if matches(glyphs, before, -1, backtrack_coverages, |c, g| {
                c.contains(g)
            }) && matches(glyphs, after, 1, input_coverages, |c, g| c.contains(g))
                && matches(
                    glyphs,
                    (at + count) as isize,
                    1,
                    lookahead_coverages,
                    |c, g| c.contains(g),
                )
            {
                return Some((count, lookups));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::font::FONT_BYTES;
    use std::collections::BTreeSet;

    #[test]
    fn ligatures_match_harfbuzz_fixtures_without_losing_cells() {
        // Expected glyph IDs independently produced by system HarfBuzz with
        // default features on the unchanged Nerd Fonts 3.4.0 / Fira Code 6.2.
        let calt = Calt::new(FONT_BYTES);
        for (text, expected) in [
            ("!=", &[12208, 12150][..]),
            ("->", &[12190, 12318]),
            ("=>", &[12317, 12321]),
            ("==", &[12386, 12263]),
            ("===", &[12386, 12386, 12264]),
            ("!==", &[12208, 12386, 12151]),
            ("..=", &[12205, 12205, 12140]),
            ("::", &[12206, 12143]),
            ("<=", &[12388, 12279]),
            ("<!--", &[12388, 12208, 12230, 12268]),
            ("www", &[12073, 12073, 12079]),
            ("a!=b", &[69, 12208, 12150, 70]),
            ("---->", &[12190, 12189, 12189, 12189, 12318]),
        ] {
            let actual: Vec<_> = calt.shape(text).iter().map(|g| g.0).collect();
            assert_eq!(actual, expected, "{text}");
            assert_eq!(actual.len(), text.len());
        }
        let text = "let ordinary = 123;";
        assert_eq!(
            calt.shape(text),
            text.chars()
                .map(|c| calt.face.glyph_index(c).unwrap())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn embedded_font_program_is_cell_preserving() {
        let calt = Calt::new(FONT_BYTES);
        // Audit ALL lookups transitively referenced by calt, including rules
        // that a short example suite might miss. Overapproximate reachability
        // by allowing every single substitution, regardless of its context.
        let mut visited = BTreeSet::new();
        let mut pending = calt.lookups.clone();
        while let Some(index) = pending.pop() {
            if !visited.insert(index) {
                continue;
            }
            let lookup = calt.table.lookups.get(index).unwrap();
            assert!(matches!(lookup.flags.0, 0 | 8));
            for sub in lookup.subtables.into_iter::<SubstitutionSubtable<'_>>() {
                if let SubstitutionSubtable::ChainContext(chain) = sub {
                    let mut add = |records: LazyArray16<'_, SequenceLookupRecord>| {
                        pending.extend(records.into_iter().map(|r| r.lookup_list_index));
                    };
                    match chain {
                        ChainedContextLookup::Format1 { sets, .. }
                        | ChainedContextLookup::Format2 { sets, .. } => {
                            for set in sets {
                                for rule in set {
                                    add(rule.lookups);
                                }
                            }
                        }
                        ChainedContextLookup::Format3 { lookups, .. } => add(lookups),
                    }
                }
            }
        }
        let mut reachable: BTreeSet<u16> = (b' '..=b'~')
            .map(|c| calt.face.glyph_index(char::from(c)).unwrap().0)
            .collect();
        loop {
            let before = reachable.clone();
            for &index in &visited {
                for sub in calt
                    .table
                    .lookups
                    .get(index)
                    .unwrap()
                    .subtables
                    .into_iter::<SubstitutionSubtable<'_>>()
                {
                    if let SubstitutionSubtable::Single(single) = sub {
                        for &id in &before {
                            if let Some(i) = single.coverage().get(GlyphId(id)) {
                                reachable.insert(match single {
                                    SingleSubstitution::Format1 { delta, .. } => {
                                        id.wrapping_add_signed(delta)
                                    }
                                    SingleSubstitution::Format2 { substitutes, .. } => {
                                        substitutes.get(i).unwrap().0
                                    }
                                });
                            }
                        }
                    }
                }
            }
            if reachable == before {
                break;
            }
        }
        let advance = calt
            .face
            .glyph_hor_advance(calt.face.glyph_index('M').unwrap());
        for &id in &reachable {
            assert_eq!(
                calt.face.glyph_hor_advance(GlyphId(id)),
                advance,
                "glyph {id} must occupy one cell"
            );
            assert_ne!(
                calt.face
                    .tables()
                    .gdef
                    .and_then(|t| t.glyph_class(GlyphId(id))),
                Some(ttf_parser::gdef::GlyphClass::Mark)
            );
        }
        for index in visited {
            for sub in calt
                .table
                .lookups
                .get(index)
                .unwrap()
                .subtables
                .into_iter::<SubstitutionSubtable<'_>>()
            {
                if !matches!(
                    sub,
                    SubstitutionSubtable::Single(_) | SubstitutionSubtable::ChainContext(_)
                ) {
                    for &id in &reachable {
                        assert!(
                            !sub.coverage().contains(GlyphId(id)),
                            "unsupported reachable lookup {index}"
                        );
                    }
                }
            }
        }
    }
}
