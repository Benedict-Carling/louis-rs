//! Lettersign braille indication
//!
//! Sometimes a sequence of letters happens to be the same as a braille contraction. In that case an
//! indication is needed to clarify the meaning. The lettersign indicator signals that the following
//! braille cells are not a contraction.
//!
//! [`Indicator`] analyses the full input text in a single pass before translation begins. At each
//! position it checks whether the remaining input matches a known contraction and, if so, emits the
//! lettersign translation at that position.
//!
//! In addition, the indicator fires for any isolated letter (not followed by another letter)
//! that does not follow a letter, as liblouis's `insertLetterSign` does — for example a
//! letter standing alone as a word, or a Tamil consonant followed by a vowel sign or virama.
//! The `noletsignafter` and `noletsignbefore` opcodes list characters adjacent to a letter that
//! suppress the indicator. The `noletsign` opcode lists letters that must never themselves be
//! preceded by a letter sign, regardless of context; tables also get every letter that is a
//! `word` or `largesign` by itself.

use std::collections::HashSet;

use crate::parser::CharacterClass;
use crate::translator::TranslationStage;
use crate::translator::sequence::{Sequences, whole_word_boundaries};
use crate::translator::table::TableContext;
use crate::{
    parser::{AnchoredRule, Direction, Precedence},
    translator::{ResolvedTranslation, trie::Trie},
};

use log::warn;

use std::collections::HashMap;

/// A builder for [`Indicator`]
#[derive(Debug)]
pub struct IndicatorBuilder {
    lettersign: Option<(String, AnchoredRule)>,
    contractions: HashMap<String, AnchoredRule>,
    noletsign_chars: HashSet<char>,
    noletsignafter_chars: HashSet<char>,
    noletsignbefore_chars: HashSet<char>,
}

impl IndicatorBuilder {
    pub fn new() -> Self {
        Self {
            lettersign: None,
            contractions: HashMap::new(),
            noletsign_chars: HashSet::new(),
            noletsignafter_chars: HashSet::new(),
            noletsignbefore_chars: HashSet::new(),
        }
    }

    /// Build an [`Indicator`]
    pub fn build(self, ctx: &TableContext, sequences: Option<Sequences>) -> Option<Indicator> {
        if self.lettersign.is_none() {
            if !self.contractions.is_empty() {
                warn!("Table contains contractions but no letsign");
            }
            return None;
        }

        let (lettersign, origin) = self.lettersign.unwrap();
        let start_translation =
            ResolvedTranslation::new("", &lettersign, 1, TranslationStage::Main, origin.clone());

        let cc = ctx.character_classes();
        let mut trie = Trie::new()
            .with_context(cc.clone())
            .with_case_folding(ctx.case_folding().clone());
        let (start, end) = whole_word_boundaries(sequences.as_ref());
        for (contraction, contraction_origin) in self.contractions {
            trie.insert(
                &contraction,
                &lettersign,
                Some(start.clone()),
                end.clone(),
                Direction::Forward,
                Precedence::Default,
                vec![],
                TranslationStage::Main,
                &contraction_origin,
            );
        }

        // Collect letter and numeric character sets from the character class context.
        let letter_chars: HashSet<char> = [
            CharacterClass::Letter,
            CharacterClass::Uppercase,
            CharacterClass::Lowercase,
        ]
        .iter()
        .filter_map(|class| cc.get(class))
        .flatten()
        .collect();

        let digit_chars: HashSet<char> = cc
            .get(&CharacterClass::Digit)
            .into_iter()
            .flatten()
            .collect();

        Some(Indicator {
            contractions: trie,
            sequences,
            start_translation,
            letter_chars,
            digit_chars,
            noletsign_chars: self.noletsign_chars,
            noletsignafter_chars: self.noletsignafter_chars,
            noletsignbefore_chars: self.noletsignbefore_chars,
        })
    }

    pub fn letsign(&mut self, s: &str, origin: &AnchoredRule) {
        self.lettersign = Some((s.to_string(), origin.clone()));
    }

    pub fn contraction(&mut self, s: &str, origin: &AnchoredRule) {
        self.contractions.insert(s.to_string(), origin.clone());
    }

    pub fn noletsign(&mut self, s: &str) {
        self.noletsign_chars.extend(s.chars());
    }

    pub fn noletsignafter(&mut self, s: &str) {
        self.noletsignafter_chars.extend(s.chars());
    }

    pub fn noletsignbefore(&mut self, s: &str) {
        self.noletsignbefore_chars.extend(s.chars());
    }
}

#[derive(Debug, Clone)]
pub struct Indicator {
    contractions: Trie,
    sequences: Option<Sequences>,
    start_translation: ResolvedTranslation,
    /// Characters that are considered letters (Letter + Uppercase + Lowercase classes).
    letter_chars: HashSet<char>,
    /// Characters of the `digit` class, after which a letter takes the letter sign even
    /// when more letters follow.
    digit_chars: HashSet<char>,
    /// These letters must never themselves be preceded by a letter sign.
    noletsign_chars: HashSet<char>,
    /// When one of these characters follows a single letter the letsign must NOT be inserted.
    noletsignafter_chars: HashSet<char>,
    /// When one of these characters precedes a single letter the letsign must NOT be inserted.
    noletsignbefore_chars: HashSet<char>,
}

impl Indicator {
    /// Returns sparse `(position, translation)` pairs.
    pub fn precompute(&self, input: &str) -> Vec<(usize, ResolvedTranslation)> {
        let chars: Vec<char> = input.chars().collect();
        let n = chars.len();

        // Collect positions where letsign should fire (deduplicated via HashSet).
        let mut fire: HashSet<usize> = HashSet::new();

        // ── Isolation-based letsign ───────────────────────────────────────────
        // liblouis's insertLetterSign: a letter not preceded by a letter, and either not
        // followed by one or preceded by a digit. `noletsign` letters, which include every
        // letter with a one-letter word rule, are left alone.
        for (i, &c) in chars.iter().enumerate() {
            if !self.letter_chars.contains(&c) || self.noletsign_chars.contains(&c) {
                continue;
            }
            let prev = if i > 0 { Some(chars[i - 1]) } else { None };
            let next = if i + 1 < n { Some(chars[i + 1]) } else { None };

            // Skip if preceded by a noletsignbefore character.
            if prev.is_some_and(|p| self.noletsignbefore_chars.contains(&p)) {
                continue;
            }
            // Skip if followed by a noletsignafter character.
            if next.is_some_and(|n| self.noletsignafter_chars.contains(&n)) {
                continue;
            }

            let isolated = next.is_none_or(|n| !self.letter_chars.contains(&n));
            let prev_is_letter = prev.is_some_and(|p| self.letter_chars.contains(&p));
            let prev_is_digit = prev.is_some_and(|p| self.digit_chars.contains(&p));
            if (isolated || prev_is_digit) && !prev_is_letter {
                fire.insert(i);
            }
        }

        // ── Contraction-based letsign ──────────────────────────────────────────
        // liblouis inserts it only if the contraction rule is the one chosen there, so it
        // carries that rule for the translation loop to check
        let mut result: Vec<(usize, ResolvedTranslation)> = fire
            .iter()
            .map(|&pos| (pos, self.start_translation.clone()))
            .collect();
        let mut prev: Option<char> = None;
        for (char_pos, (byte_pos, c)) in input.char_indices().enumerate() {
            if !fire.contains(&char_pos)
                && let Some(origin) =
                    self.contraction_at(&input[byte_pos..], prev, &chars, char_pos)
            {
                let translation = ResolvedTranslation::new(
                    "",
                    &self.start_translation.output(),
                    1,
                    TranslationStage::Main,
                    origin,
                );
                result.push((char_pos, translation));
            }
            prev = Some(c);
        }
        result.sort_by_key(|(pos, _)| *pos);
        result
    }

    fn contraction_at(
        &self,
        s: &str,
        prev: Option<char>,
        chars: &[char],
        pos: usize,
    ) -> Option<AnchoredRule> {
        self.contractions
            .find_translations(s, prev)
            .into_iter()
            .find(|t| {
                self.sequences
                    .as_ref()
                    .is_none_or(|seq| seq.in_sequence(chars, pos, t.length()))
            })
            .and_then(|t| t.origin())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        parser::{CharacterClass, CharacterClasses, RuleParser},
        translator::{TranslationStage, table::TableContext},
    };

    fn rule(rule: &str) -> AnchoredRule {
        let rule = RuleParser::new(rule).rule().unwrap();
        AnchoredRule::new(rule, None, 0)
    }

    fn pairs(t: &[(usize, ResolvedTranslation)]) -> Vec<(usize, String)> {
        t.iter()
            .map(|(pos, r)| (*pos, r.output().to_string()))
            .collect()
    }

    fn build_indicator(
        letter_chars: &[char],
        noletsignbefore: &[char],
        contraction: Option<&str>,
    ) -> Indicator {
        let mut builder = IndicatorBuilder::new();
        builder.letsign("⠠", &rule("letsign 6"));
        let noletsignbefore_str: String = noletsignbefore.iter().collect();
        builder.noletsignbefore(&noletsignbefore_str);
        if let Some(c) = contraction {
            builder.contraction(c, &rule(&format!("contraction {}", c)));
        }
        let cc = CharacterClasses::new(&[
            (CharacterClass::Letter, letter_chars),
            (CharacterClass::Space, &[' ']),
        ]);
        let ctx = TableContext::new(cc, CharacterClasses::default(), Default::default());
        builder.build(&ctx, None).unwrap()
    }

    #[test]
    fn contraction_indicator() {
        let indicator = build_indicator(&['a', 'b', 'c', 'd'], &[], Some("ab"));
        assert!(indicator.precompute("aa").is_empty());
        assert_eq!(
            pairs(&indicator.precompute("ab")),
            vec![(0, "⠠".to_string())]
        );
    }

    #[test]
    fn letter_after_a_digit_fires_even_before_more_letters() {
        let mut builder = IndicatorBuilder::new();
        builder.letsign("⠠", &rule("letsign 6"));
        let cc = CharacterClasses::new(&[
            (CharacterClass::Letter, &['a', 'b']),
            (CharacterClass::Digit, &['2']),
        ]);
        let ctx = TableContext::new(cc, CharacterClasses::default(), Default::default());
        let indicator = builder.build(&ctx, None).unwrap();
        assert_eq!(
            pairs(&indicator.precompute("2ab")),
            vec![(1, "⠠".to_string())]
        );
        assert_eq!(pairs(&indicator.precompute(" ab")), vec![]);
    }

    #[test]
    fn isolated_letter_fires() {
        let indicator = build_indicator(&['a', 'b', 'c'], &[], None);
        // isolated letter at string start (prev=None): fires
        assert_eq!(
            pairs(&indicator.precompute("b:")),
            vec![(0, "⠠".to_string())]
        );
        // isolated letter after punctuation (prev is non-space, non-letter): fires
        assert_eq!(
            pairs(&indicator.precompute("2b")),
            vec![(1, "⠠".to_string())]
        );
        // isolated letter preceded by space: fires too
        assert_eq!(
            pairs(&indicator.precompute(" b ")),
            vec![(1, "⠠".to_string())]
        );
        // letter followed by another letter (not isolated): does NOT fire
        assert_eq!(pairs(&indicator.precompute(":bc")), vec![]);
        // multi-letter word: no letsign
        assert_eq!(pairs(&indicator.precompute("abc")), vec![]);
    }

    #[test]
    fn noletsign_suppresses_isolation_letsign() {
        let mut builder = IndicatorBuilder::new();
        builder.letsign("⠠", &rule("letsign 6"));
        builder.noletsign("i");
        let cc = CharacterClasses::new(&[
            (CharacterClass::Letter, &['a', 'b', 'i']),
            (CharacterClass::Space, &[' ']),
        ]);
        let ctx = TableContext::new(cc, CharacterClasses::default(), Default::default());
        let indicator = builder.build(&ctx, None).unwrap();

        // 'i' is listed in noletsign: never gets a letsign, even isolated after a
        // non-space, non-letter character.
        assert_eq!(pairs(&indicator.precompute("2i")), vec![]);
        // other isolated letters are unaffected.
        assert_eq!(
            pairs(&indicator.precompute("2b")),
            vec![(1, "⠠".to_string())]
        );
    }

    #[test]
    fn noletsign_does_not_suppress_contraction_letsign() {
        let mut builder = IndicatorBuilder::new();
        builder.letsign("⠠", &rule("letsign 6"));
        builder.contraction("ab", &rule("contraction ab"));
        builder.noletsign("a");
        let cc = CharacterClasses::new(&[(CharacterClass::Letter, &['a', 'b'])]);
        let ctx = TableContext::new(cc, CharacterClasses::default(), Default::default());
        let indicator = builder.build(&ctx, None).unwrap();

        // liblouis inserts the letter sign before a contraction without consulting noletsign
        assert_eq!(
            pairs(&indicator.precompute("ab")),
            vec![(0, "⠠".to_string())]
        );
    }

    #[test]
    fn noletsignbefore_suppresses_isolation_letsign() {
        let indicator = build_indicator(&['a', 'b', 'c'], &['2'], None);
        // noletsignbefore '2' suppresses the letsign for "2b" -- '2' precedes the letter
        assert_eq!(pairs(&indicator.precompute("2b")), vec![]);
        // other non-space, non-letter chars still trigger it
        assert_eq!(
            pairs(&indicator.precompute("3b")),
            vec![(1, "⠠".to_string())]
        );
    }

    #[test]
    fn noletsignafter_suppresses_isolation_letsign() {
        let mut builder = IndicatorBuilder::new();
        builder.letsign("⠠", &rule("letsign 6"));
        builder.noletsignafter("2");
        let cc = CharacterClasses::new(&[
            (CharacterClass::Letter, &['a', 'b', 'c']),
            (CharacterClass::Space, &[' ']),
        ]);
        let ctx = TableContext::new(cc, CharacterClasses::default(), Default::default());
        let indicator = builder.build(&ctx, None).unwrap();

        // noletsignafter '2' suppresses the letsign for "3b2" -- '2' follows the letter
        assert_eq!(pairs(&indicator.precompute("3b2")), vec![]);
        // a different following char leaves it in place
        assert_eq!(
            pairs(&indicator.precompute("3b3")),
            vec![(1, "⠠".to_string())]
        );
    }

    #[test]
    fn precompute_contraction() {
        let mut builder = IndicatorBuilder::new();
        builder.letsign("⠠", &rule("letsign 6"));
        builder.contraction("ab", &rule("contraction ab"));
        builder.contraction("cd", &rule("contraction cd"));
        let cc = CharacterClasses::new(&[(CharacterClass::Letter, &['a', 'b', 'c', 'd'])]);
        let ctx = TableContext::new(cc, CharacterClasses::default(), Default::default());
        let indicator = builder.build(&ctx, None).unwrap();

        assert_eq!(pairs(&indicator.precompute("aa")), vec![]);
        assert_eq!(
            pairs(&indicator.precompute("ab")),
            vec![(0, "⠠".to_string())]
        );
    }
}
