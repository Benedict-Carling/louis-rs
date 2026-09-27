//! Numeric Braille indication
//!
//! [`Indicator`] analyses the full input text in a single pass before translation begins:
//!
//! - Emits the numsign translation at the first digit of a numeric run.
//! - Emits the non-numsign translation at the first non-numeric terminating character.

use crate::{
    parser::AnchoredRule,
    translator::{ResolvedTranslation, TranslationStage},
};

use std::collections::HashSet;

/// A builder for [`Indicator`]
#[derive(Debug)]
pub struct IndicatorBuilder(Indicator);

impl IndicatorBuilder {
    pub fn new() -> Self {
        IndicatorBuilder(Indicator {
            numeric_chars: HashSet::default(),
            extra_numeric_chars: HashSet::default(),
            mid_numeric_chars: HashSet::default(),
            start_translation: None,
            end_translation: None,
            terminating_chars: HashSet::default(),
            midend_numeric_mode_chars: HashSet::default(),
            uses_numeric_mode: false,
        })
    }

    pub fn build(self) -> Option<Indicator> {
        if self.0.start_translation.is_some() && !self.0.numeric_chars.is_empty() {
            Some(self.0)
        } else {
            None
        }
    }

    pub fn numsign(&mut self, s: &str, origin: &AnchoredRule) {
        self.0.start_translation = Some(ResolvedTranslation::new(
            "",
            s,
            1,
            TranslationStage::Main,
            origin.clone(),
        ));
    }

    pub fn nonumsign(&mut self, s: &str, origin: &AnchoredRule) {
        self.0.end_translation = Some(ResolvedTranslation::new(
            "",
            s,
            1,
            TranslationStage::Main,
            origin.clone(),
        ));
    }

    pub fn midnum(&mut self, s: &str) {
        self.0.mid_numeric_chars.extend(s.chars());
    }

    pub fn numericnocontchars(&mut self, s: &str) {
        self.0.terminating_chars = HashSet::from_iter(s.chars());
        self.0.uses_numeric_mode = true;
    }

    pub fn numericmodechars(&mut self, s: &str) {
        self.0.extra_numeric_chars = HashSet::from_iter(s.chars());
        self.0.uses_numeric_mode = true;
    }

    pub fn midendnumericmodechars(&mut self, s: &str) {
        self.0.midend_numeric_mode_chars.extend(s.chars());
        self.0.uses_numeric_mode = true;
    }

    pub fn numeric_characters(&mut self, chars: HashSet<char>) {
        self.0.numeric_chars = chars;
    }
}

#[derive(Debug, Clone)]
pub struct Indicator {
    /// Characters that trigger entry into a numeric run (digits)
    numeric_chars: HashSet<char>,
    /// Characters that extend a numeric run without being digits themselves
    /// (e.g. `numericmodechars`)
    extra_numeric_chars: HashSet<char>,
    /// Characters that extend a numeric run only when sandwiched between two
    /// digits (`midnum`)
    mid_numeric_chars: HashSet<char>,
    /// Translation emitted at the start of a numeric run (the numsign)
    start_translation: Option<ResolvedTranslation>,
    /// Translation emitted at the end of a numeric run (the non-numsign)
    end_translation: Option<ResolvedTranslation>,
    /// Characters that trigger end-of-run indication when the run exits into them
    terminating_chars: HashSet<char>,
    midend_numeric_mode_chars: HashSet<char>,
    uses_numeric_mode: bool,
}

impl Indicator {
    /// liblouis's `checkNumericMode`, used instead of `insertNumberSign` by a table that
    /// defines `numericmodechars`, `midendnumericmodechars` or `numericnocontchars`.
    fn precompute_numeric_mode(&self, chars: &[char]) -> Vec<(usize, ResolvedTranslation)> {
        let mut translations = Vec::new();
        let Some(numsign) = &self.start_translation else {
            return translations;
        };
        let mut numeric = false;
        for (pos, &c) in chars.iter().enumerate() {
            if !numeric {
                if self.numeric_chars.contains(&c) || self.starts_numeric_mode(&chars[pos..]) {
                    numeric = true;
                    translations.push((pos, numsign.clone()));
                }
            } else if !(self.numeric_chars.contains(&c)
                || self.extra_numeric_chars.contains(&c)
                || self.midend_numeric_mode_chars.contains(&c))
            {
                numeric = false;
                if self.terminating_chars.contains(&c)
                    && let Some(t) = &self.end_translation
                {
                    translations.push((pos, t.clone()));
                }
            }
        }
        translations
    }

    /// Returns sparse `(position, translation)` pairs.
    pub fn precompute(&self, input: &str) -> Vec<(usize, ResolvedTranslation)> {
        let chars: Vec<char> = input.chars().collect();
        let mut translations: Vec<(usize, ResolvedTranslation)> = Vec::new();

        if self.start_translation.is_none() {
            return translations;
        }
        if self.uses_numeric_mode {
            return self.precompute_numeric_mode(&chars);
        }

        let mut in_numeric = false;

        for (pos, &c) in chars.iter().enumerate() {
            let is_numeric = self.numeric_chars.contains(&c);
            match (is_numeric, in_numeric) {
                (false, false) if self.starts_numeric_mode(&chars[pos..]) => {
                    in_numeric = true;
                    if let Some(t) = &self.start_translation {
                        translations.push((pos, t.clone()));
                    }
                }
                (false, false) => {}
                (true, false) => {
                    in_numeric = true;
                    if let Some(t) = &self.start_translation {
                        translations.push((pos, t.clone()));
                    }
                }
                (true, true) => {}
                (false, true) if self.extra_numeric_chars.contains(&c) => {}
                (false, true)
                    if self.extra_numeric_chars.is_empty()
                        && self.mid_numeric_chars.contains(&c)
                        && chars
                            .get(pos + 1)
                            .is_some_and(|nc| self.numeric_chars.contains(nc)) => {}
                (false, true) => {
                    in_numeric = false;
                    if self.terminating_chars.contains(&c)
                        && let Some(t) = &self.end_translation
                    {
                        translations.push((pos, t.clone()));
                    }
                }
            }
        }

        translations
    }

    /// A run of `numericmodechars` followed by a digit enters numeric mode at its first
    /// character, as liblouis' `checkNumericMode` does.
    fn starts_numeric_mode(&self, chars: &[char]) -> bool {
        chars
            .iter()
            .find(|c| !self.extra_numeric_chars.contains(c))
            .is_some_and(|c| self.numeric_chars.contains(c))
            && self.extra_numeric_chars.contains(&chars[0])
    }
}

#[cfg(test)]
mod tests {
    use crate::parser::RuleParser;

    use super::*;

    fn rule(s: &str) -> AnchoredRule {
        AnchoredRule::new(RuleParser::new(s).rule().unwrap(), None, 0)
    }

    fn pairs(t: &[(usize, ResolvedTranslation)]) -> Vec<(usize, String)> {
        t.iter()
            .map(|(pos, r)| (*pos, r.output().to_string()))
            .collect()
    }

    #[test]
    fn precompute_number() {
        let mut builder = IndicatorBuilder::new();
        builder.numeric_characters(HashSet::from(['1', '2', '3']));
        builder.numsign("⠼", &rule("numsign 3456"));
        builder.numericnocontchars("abc");
        let indicator = builder.build().unwrap();

        assert_eq!(
            pairs(&indicator.precompute("ab12 a")),
            vec![(2, "⠼".to_string())]
        );
    }

    #[test]
    fn precompute_end_indication() {
        let mut builder = IndicatorBuilder::new();
        builder.numeric_characters(HashSet::from(['1', '2', '3']));
        builder.numsign("⠼", &rule("numsign 3456"));
        builder.nonumsign("⠰", &rule("nonumsign 56"));
        builder.numericnocontchars("abc");
        let indicator = builder.build().unwrap();

        assert_eq!(
            pairs(&indicator.precompute("ab12a")),
            vec![(2, "⠼".to_string()), (4, "⠰".to_string())],
        );
    }

    #[test]
    fn numeric_mode_ends_at_a_numericnocontchar() {
        let mut builder = IndicatorBuilder::new();
        builder.numeric_characters(HashSet::from(['1', '2', '3']));
        builder.numsign("⠼", &rule("numsign 3456"));
        builder.nonumsign("⠰", &rule("nonumsign 56"));
        builder.numericmodechars(".");
        builder.numericnocontchars("abc");
        let indicator = builder.build().unwrap();

        assert_eq!(
            pairs(&indicator.precompute("1.2a")),
            vec![(0, "⠼".to_string()), (3, "⠰".to_string())]
        );
    }

    #[test]
    fn midend_numeric_mode_char_keeps_numeric_mode() {
        let mut builder = IndicatorBuilder::new();
        builder.numeric_characters(HashSet::from(['1', '2', '3']));
        builder.numsign("⠼", &rule("numsign 3456"));
        builder.midendnumericmodechars("-");
        let indicator = builder.build().unwrap();

        assert_eq!(
            pairs(&indicator.precompute("1-2")),
            vec![(0, "⠼".to_string())]
        );
        assert_eq!(
            pairs(&indicator.precompute("1 2")),
            vec![(0, "⠼".to_string()), (2, "⠼".to_string())]
        );
    }

    #[test]
    fn precompute_numericmodechars_before_digit() {
        let mut builder = IndicatorBuilder::new();
        builder.numeric_characters(HashSet::from(['1', '2', '3']));
        builder.numsign("⠼", &rule("numsign 3456"));
        builder.numericmodechars(".,");
        let indicator = builder.build().unwrap();

        assert_eq!(
            pairs(&indicator.precompute("a.1")),
            vec![(1, "⠼".to_string())]
        );
        assert_eq!(
            pairs(&indicator.precompute(".,2")),
            vec![(0, "⠼".to_string())]
        );
        assert_eq!(
            pairs(&indicator.precompute("1.2")),
            vec![(0, "⠼".to_string())]
        );
        assert!(pairs(&indicator.precompute("a.b")).is_empty());
    }
}
