//! The display table: how braille cells are shown as characters.
//!
//! A stage of the translation pipeline like [`super::primary::PrimaryTable`] and
//! [`super::multipass::MultipassTable`], but on the braille side of it -- last when
//! translating to braille, first when translating from it. See
//! [`crate::translator::TranslationPipeline::with_display`].

use std::collections::HashMap;

use crate::{
    Direction,
    parser::{AnchoredRule, HasDirection, Rule},
    translator::{ResolvedTranslation, TranslationStage},
};

/// What one braille cell is shown as, and the `display` rule that says so.
///
#[derive(Debug)]
struct DisplayMapping {
    to: char,
    // FIXME: unboxed AnchoredRule inflates the HashMap value, measured slower than the
    // Box<AnchoredRule> version (cargo bench --bench translate). Display tables are small and
    // char-keyed, so a sorted Vec<(char, DisplayMapping)> + binary_search (or a fixed-size array
    // keyed by dot pattern) would likely beat re-boxing if this shows up as a hot path.
    origin: AnchoredRule,
}

#[derive(Debug)]
pub struct DisplayTable {
    mappings: HashMap<char, DisplayMapping>,
}

impl DisplayTable {
    pub fn compile(rules: &[AnchoredRule], direction: Direction) -> DisplayTable {
        let mut mappings = HashMap::new();
        let rules: Vec<_> = rules.iter().filter(|r| r.is_direction(direction)).collect();

        for rule in rules {
            match &rule.rule {
                Rule::Display {
                    character, dots, ..
                } => {
                    let braille = dots.to_unicode();
                    // swap `from` and `to` for backwards translation
                    let (from, to) = match direction {
                        Direction::Forward => (braille, *character),
                        Direction::Backward => (*character, braille),
                    };
                    let mapping = DisplayMapping {
                        to,
                        origin: rule.clone(),
                    };
                    if cfg!(feature = "backwards_compatibility") {
                        // first rule wins
                        mappings.entry(from).or_insert(mapping);
                    } else {
                        // last rule wins
                        mappings.insert(from, mapping);
                    }
                }
                _ => (), // ignore all other rules for display tables
            }
        }
        DisplayTable { mappings }
    }

    /// Map the `input` to the output using the display rules in the
    /// `DisplayTable`.
    ///
    /// If the `DisplayTable` does not contain a mapping for a
    /// specific char then the original character is returned
    pub fn translate(&self, input: &str) -> String {
        input.chars().map(|c| self.displayed(c)).collect()
    }

    /// The character `c` is shown as, or `c` itself when the table has no mapping for it.
    fn displayed(&self, c: char) -> char {
        self.mappings.get(&c).map_or(c, |mapping| mapping.to)
    }

    /// One [`ResolvedTranslation`] per character, so a display table can take part in the
    /// pipeline like any other stage.
    ///
    /// Every mapping is one character to one character, which is what lets
    /// [`PositionMap::from_trace`] compose this stage as the identity it is. A character the
    /// table has no rule for translates to itself and carries no origin.
    pub fn trace(&self, input: &str) -> Vec<ResolvedTranslation> {
        input
            .chars()
            .map(|c| {
                let (to, origin) = match self.mappings.get(&c) {
                    Some(mapping) => (mapping.to, Some(mapping.origin.clone())),
                    None => (c, None),
                };
                ResolvedTranslation::new(
                    &c.to_string(),
                    &to.to_string(),
                    1,
                    TranslationStage::Display,
                    origin,
                )
            })
            .collect()
    }
}

/// Each character paired with its cell by a rule liblouis puts in a display table, in table
/// order: `display` rules, character definitions with a single cell and both characters of a
/// `grouping`. So a translation table that includes no display table still has one.
fn char_dots_pairs(rules: &[AnchoredRule]) -> Vec<(char, char)> {
    let mut pairs = Vec::new();
    for rule in rules {
        match &rule.rule {
            Rule::Display {
                character, dots, ..
            } => pairs.push((*character, dots.to_unicode())),
            Rule::Space {
                character, dots, ..
            }
            | Rule::Punctuation {
                character, dots, ..
            }
            | Rule::Digit {
                character, dots, ..
            }
            | Rule::Letter {
                character, dots, ..
            }
            | Rule::Lowercase {
                character, dots, ..
            }
            | Rule::Uppercase {
                character, dots, ..
            }
            | Rule::Litdigit {
                character, dots, ..
            }
            | Rule::Sign {
                character, dots, ..
            }
            | Rule::Math {
                character, dots, ..
            } if dots.len() == 1 => {
                pairs.push((*character, dots.iter().next().unwrap().to_unicode()))
            }
            Rule::Grouping {
                chars, left, right, ..
            } => {
                let mut chars = chars.chars();
                if let (Some(open), Some(close)) = (chars.next(), chars.next()) {
                    pairs.push((open, left.to_unicode()));
                    pairs.push((close, right.to_unicode()));
                }
            }
            _ => (),
        }
    }
    pairs
}

fn add_mapping(mappings: &mut HashMap<char, char>, from: char, to: char) {
    if cfg!(feature = "backwards_compatibility") {
        mappings.entry(from).or_insert(to);
    } else {
        mappings.insert(from, to);
    }
}

const BLANK: char = '\u{2800}';

/// The braille cell each character is shown as, the reverse of a [`DisplayTable`], as
/// liblouis's `lou_charToDots` reads it off a table.
#[derive(Debug)]
pub struct CharToDots {
    mappings: HashMap<char, char>,
}

impl CharToDots {
    pub fn compile(rules: &[AnchoredRule]) -> CharToDots {
        let mut mappings = HashMap::new();
        for (character, dots) in char_dots_pairs(rules) {
            // ucBrl output only has room for dots 1 to 8
            let cell = char::from_u32(0x2800 | (dots as u32 & 0xff)).unwrap();
            add_mapping(&mut mappings, character, cell);
        }
        CharToDots { mappings }
    }

    /// The cells `input` is shown as, one per character, with a blank cell for a
    /// character the table has no mapping for.
    pub fn dots(&self, input: &str) -> String {
        input
            .chars()
            .map(|c| *self.mappings.get(&c).unwrap_or(&BLANK))
            .collect()
    }
}

/// A translation table read as its own display table, which is what liblouis does when a
/// translation is given no display table. Both directions come from the same
/// [`char_dots_pairs`], each keeping the first mapping for its key.
#[derive(Debug)]
pub struct ImpliedDisplay {
    chars: HashMap<char, char>,
    cells: HashMap<char, char>,
}

impl ImpliedDisplay {
    pub fn compile(rules: &[AnchoredRule]) -> ImpliedDisplay {
        let mut chars = HashMap::new();
        let mut cells = HashMap::new();
        for (character, cell) in char_dots_pairs(rules) {
            add_mapping(&mut chars, cell, character);
            add_mapping(&mut cells, character, cell);
        }
        ImpliedDisplay { chars, cells }
    }

    /// The characters `braille` is shown as, or `None` if a cell has no mapping, which
    /// liblouis treats as a failed translation.
    pub fn show(&self, braille: &str) -> Option<String> {
        braille
            .chars()
            .map(|c| self.chars.get(&c).copied())
            .collect()
    }

    /// The cells `text` is read as, with a blank cell for a character that has no mapping.
    pub fn read(&self, text: &str) -> String {
        text.chars()
            .map(|c| *self.cells.get(&c).unwrap_or(&BLANK))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{
        parser::RuleParser, translator::table::TableContext,
        translator::table::primary::PrimaryTable,
    };

    fn parse_rule(source: &str) -> AnchoredRule {
        RuleParser::new(source).rule().unwrap().into()
    }

    #[test]
    fn display_table() {
        let display_rules = [parse_rule("display a 1"), parse_rule("display \\s 0")];
        let display_table = DisplayTable::compile(&display_rules, Direction::Forward);
        assert_eq!(display_table.translate("⠁"), "a");
        assert_eq!(display_table.translate("⠀"), " ");
        assert_eq!(display_table.translate(""), "");
        assert_eq!(display_table.translate("x"), "x"); // unknown chars are translated to themselves
    }

    #[test]
    fn char_to_dots_from_display_rules() {
        let rules = [parse_rule("display a 1"), parse_rule("display \\s 0")];
        let table = CharToDots::compile(&rules);
        assert_eq!(table.dots("a a"), "⠁⠀⠁");
        assert_eq!(table.dots(""), "");
        assert_eq!(table.dots("x"), "⠀"); // unknown chars get a blank cell
    }

    #[test]
    fn char_to_dots_from_character_definitions() {
        let rules = [
            parse_rule("letter a 1"),
            parse_rule("digit 1 2"),
            parse_rule("punctuation . 256"),
            parse_rule("sign % 46-356"),
            parse_rule("grouping parens () 2356,2356"),
        ];
        let table = CharToDots::compile(&rules);
        assert_eq!(table.dots("a1."), "⠁⠂⠲");
        assert_eq!(table.dots("%"), "⠀"); // only single-cell definitions map
        assert_eq!(table.dots("()"), "⠶⠶");
    }

    #[test]
    #[cfg(feature = "backwards_compatibility")]
    fn char_to_dots_first_rule_wins() {
        let rules = [parse_rule("display a 2"), parse_rule("letter a 1")];
        assert_eq!(CharToDots::compile(&rules).dots("a"), "⠂");
    }

    #[test]
    fn char_to_dots_keeps_eight_dots_and_drops_virtual_ones() {
        let rules = [parse_rule("display a 78"), parse_rule("display b 19")];
        let table = CharToDots::compile(&rules);
        assert_eq!(table.dots("ab"), "⣀⠁");
    }

    #[test]
    #[cfg(feature = "backwards_compatibility")]
    fn implied_display_keeps_the_first_mapping_each_way() {
        let rules = [
            parse_rule("letter a 1"),
            parse_rule("display A 1"),
            parse_rule("display b 1"),
            parse_rule("grouping parens () 2356,356"),
        ];
        let display = ImpliedDisplay::compile(&rules);
        assert_eq!(display.show("⠁⠶⠴").as_deref(), Some("a()"));
        assert_eq!(display.read("aAb()"), "⠁⠁⠁⠶⠴");
    }

    #[test]
    fn implied_display_has_no_character_for_an_unmapped_cell() {
        let display = ImpliedDisplay::compile(&[parse_rule("letter a 1")]);
        assert_eq!(display.show("⠁⠂"), None);
        assert_eq!(display.read("ax"), "⠁⠀");
    }

    #[test]
    fn translate_with_display() {
        let display_rules = [parse_rule("display A 1"), parse_rule("display \\s 0")];
        let rules = [parse_rule("letter a 1"), parse_rule("space \\s 0")];
        let display_table = DisplayTable::compile(&display_rules, Direction::Forward);
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(display_table.translate(&table.translate("a")), "A");
        assert_eq!(display_table.translate(&table.translate(" ")), " ");
        assert_eq!(display_table.translate(&table.translate("a a")), "A A");
    }
}
