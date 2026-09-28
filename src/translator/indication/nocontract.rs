//! Nocontract braille indication
//!
//! Sometimes a sequence of letters happens to be the same as a braille contraction. In that case an
//! indication is needed to clarify the meaning. The nocontract indicator signals that the following
//! braille cells are not a contraction.
//!
//! [`Indicator`] analyses the full input text in a single pass before translation begins. At each
//! position it checks whether the remaining input matches a known contraction and, if so, emits the
//! nocontractsign translation at that position.
//!
//! # FIXME:
//!
//! This indicator is basically the same as the [lettersign
//! indicator](crate::translator::indication::lettersign::Indicator). In liblouis they do not seem
//! to have exactly the same behaviour. Find a way to merge them.

use crate::translator::TranslationStage;
use crate::translator::case::CaseClasses;
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
    nocontractsign: Option<(String, AnchoredRule)>,
    contractions: HashMap<String, AnchoredRule>,
}

impl IndicatorBuilder {
    pub fn new() -> Self {
        Self {
            nocontractsign: None,
            contractions: HashMap::new(),
        }
    }

    /// Build an [`Indicator`]
    pub fn build(self, ctx: &TableContext, sequences: Option<Sequences>) -> Option<Indicator> {
        let has_contractions = !self.contractions.is_empty();
        match self.nocontractsign {
            Some((nocontractsign, origin)) if has_contractions => {
                let start_translation = ResolvedTranslation::new(
                    "",
                    &nocontractsign,
                    1,
                    TranslationStage::Main,
                    origin.clone(),
                );
                let mut trie = Trie::new()
                    .with_context(ctx.character_classes().clone())
                    .with_case_folding(ctx.case_folding().clone());
                let (start, end) = whole_word_boundaries(sequences.as_ref());
                for (contraction, _origin) in self.contractions {
                    trie.insert(
                        &contraction,
                        &nocontractsign,
                        Some(start.clone()),
                        end.clone(),
                        Direction::Forward,
                        Precedence::Default,
                        vec![],
                        TranslationStage::Main,
                        &origin,
                    );
                }
                Some(Indicator {
                    contractions: trie,
                    sequences,
                    start_translation,
                    case: CaseClasses::from_context(ctx),
                })
            }
            Some(_) => {
                warn!("Table contains nocontractionsign but no contractions");
                None
            }
            None if has_contractions => {
                warn!("Table contains contractions but no nocontractionsign");
                None
            }
            None => None,
        }
    }

    pub fn nocontractsign(&mut self, s: &str, origin: &AnchoredRule) {
        self.nocontractsign = Some((s.to_string(), origin.clone()));
    }

    pub fn contraction(&mut self, s: &str, origin: &AnchoredRule) {
        self.contractions.insert(s.to_string(), origin.clone());
    }
}

#[derive(Debug, Clone)]
pub struct Indicator {
    contractions: Trie,
    sequences: Option<Sequences>,
    start_translation: ResolvedTranslation,
    case: CaseClasses,
}

impl Indicator {
    /// Returns sparse `(position, translation)` pairs.
    pub fn precompute(&self, input: &str) -> Vec<(usize, ResolvedTranslation)> {
        let mut translations: Vec<(usize, ResolvedTranslation)> = Vec::new();
        let mut prev: Option<char> = None;
        let mut char_pos = 0;
        let chars: Vec<char> = input.chars().collect();

        for (byte_pos, c) in input.char_indices() {
            if self.contraction_at(&input[byte_pos..], prev, &chars, char_pos) {
                translations.push((char_pos, self.start_translation.clone()));
            }
            prev = Some(c);
            char_pos += 1;
        }

        translations
    }

    fn contraction_at(&self, s: &str, prev: Option<char>, chars: &[char], pos: usize) -> bool {
        self.contractions
            .find_translations(s, prev)
            .iter()
            .any(|t| {
                self.case.valid_match(chars, pos, t.length())
                    && self
                        .sequences
                        .as_ref()
                        .is_none_or(|seq| seq.in_sequence(chars, pos, t.length()))
            })
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

    fn make_ctx(letter_chars: &[char]) -> TableContext {
        let cc = CharacterClasses::new(&[(CharacterClass::Letter, letter_chars)]);
        TableContext::new(cc, CharacterClasses::default(), Default::default())
    }

    #[test]
    fn indicator() {
        let mut builder = IndicatorBuilder::new();
        builder.nocontractsign("⡀", &rule("nocontractsign 7"));
        builder.contraction("ab", &rule("contraction ab"));
        builder.contraction("cd", &rule("contraction cd"));
        let ctx = make_ctx(&['a', 'b', 'c', 'd']);
        let indicator = builder.build(&ctx, None).unwrap();
        assert!(indicator.precompute("aa").is_empty());
        assert_eq!(
            pairs(&indicator.precompute("ab")),
            vec![(0, "⡀".to_string())]
        );
        assert_eq!(
            pairs(&indicator.precompute("cd")),
            vec![(0, "⡀".to_string())]
        );
    }

    #[test]
    fn precompute_indicator() {
        let mut builder = IndicatorBuilder::new();
        builder.nocontractsign("⡀", &rule("nocontractsign 7"));
        builder.contraction("ab", &rule("contraction ab"));
        builder.contraction("cd", &rule("contraction cd"));
        let ctx = make_ctx(&['a', 'b', 'c', 'd']);
        let indicator = builder.build(&ctx, None).unwrap();

        assert_eq!(pairs(&indicator.precompute("aa")), vec![]);
        assert_eq!(
            pairs(&indicator.precompute("ab")),
            vec![(0, "⡀".to_string())]
        );
    }
}
