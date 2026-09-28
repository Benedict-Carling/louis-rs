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

/// The character sets that decide where a numeric run starts and ends.
#[derive(Debug, Clone, Default)]
pub struct NumericRuns {
    /// Characters that trigger entry into a numeric run (digits)
    numeric_chars: HashSet<char>,
    /// `numericmodechars`: extend a run, and start one when a digit follows
    mode_chars: HashSet<char>,
    /// `midendnumericmodechars`: extend a run but never start one
    midend_chars: HashSet<char>,
    /// `midnum` and `decpoint`: extend a run only when sandwiched between two digits
    mid_numeric_chars: HashSet<char>,
    /// `numericnocontchars`: a run exiting into one of these gets the nonumsign
    nocont_chars: HashSet<char>,
}

/// What happens to the numeric run at one input position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEvent {
    Outside,
    /// A run starts here; `by_digit` is false when a `numericmodechars` character starts it
    Start {
        by_digit: bool,
    },
    Inside,
    /// The run ends here; `nonumsign` is true when this character needs the nonumsign
    End {
        nonumsign: bool,
    },
}

impl NumericRuns {
    pub fn numeric_characters(&mut self, chars: HashSet<char>) {
        self.numeric_chars = chars;
    }

    pub fn numericmodechars(&mut self, s: &str) {
        self.mode_chars.extend(s.chars());
    }

    pub fn midendnumericmodechars(&mut self, s: &str) {
        self.midend_chars.extend(s.chars());
    }

    pub fn midnum(&mut self, s: &str) {
        self.mid_numeric_chars.extend(s.chars());
    }

    pub fn numericnocontchars(&mut self, s: &str) {
        self.nocont_chars.extend(s.chars());
    }

    /// The `numericmodechars` and `midendnumericmodechars` characters, which keep a
    /// number going without being digits
    pub fn continuation_chars(&self) -> impl Iterator<Item = &char> {
        self.mode_chars.union(&self.midend_chars)
    }

    pub fn is_empty(&self) -> bool {
        self.numeric_chars.is_empty()
    }

    /// liblouis's `usesNumericMode`: any of the numeric mode opcodes switches from
    /// `insertNumberSign` to `checkNumericMode`
    pub fn uses_numeric_mode(&self) -> bool {
        !self.mode_chars.is_empty()
            || !self.midend_chars.is_empty()
            || !self.nocont_chars.is_empty()
    }

    pub fn events(&self, chars: &[char]) -> Vec<RunEvent> {
        let mut in_numeric = false;
        (0..chars.len())
            .map(|pos| {
                let event = self.step(in_numeric, chars, pos);
                in_numeric = event.in_numeric();
                event
            })
            .collect()
    }

    /// What happens at `pos`, given whether a number is running before it
    pub fn step(&self, in_numeric: bool, chars: &[char], pos: usize) -> RunEvent {
        let numeric_mode = self.uses_numeric_mode();
        let c = &chars[pos];
        if !in_numeric {
            if self.numeric_chars.contains(c) {
                RunEvent::Start { by_digit: true }
            } else if numeric_mode
                && self.mode_chars.contains(c)
                && chars[pos + 1..]
                    .iter()
                    .find(|c| !self.mode_chars.contains(c))
                    .is_some_and(|c| self.numeric_chars.contains(c))
            {
                RunEvent::Start { by_digit: false }
            } else {
                RunEvent::Outside
            }
        } else if self.numeric_chars.contains(c)
            || numeric_mode && (self.mode_chars.contains(c) || self.midend_chars.contains(c))
            || !numeric_mode
                && self.mid_numeric_chars.contains(c)
                && chars
                    .get(pos + 1)
                    .is_some_and(|nc| self.numeric_chars.contains(nc))
        {
            RunEvent::Inside
        } else {
            RunEvent::End {
                nonumsign: self.nocont_chars.contains(c),
            }
        }
    }
}

impl RunEvent {
    pub fn in_numeric(self) -> bool {
        matches!(self, RunEvent::Start { .. } | RunEvent::Inside)
    }
}

/// A builder for [`Indicator`]
#[derive(Debug)]
pub struct IndicatorBuilder(Indicator);

impl IndicatorBuilder {
    pub fn new() -> Self {
        IndicatorBuilder(Indicator {
            runs: NumericRuns::default(),
            start_translation: None,
            end_translation: None,
        })
    }

    pub fn build(self) -> Option<Indicator> {
        if self.0.start_translation.is_some() && !self.0.runs.is_empty() {
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

    pub fn runs(&mut self) -> &mut NumericRuns {
        &mut self.0.runs
    }
}

#[derive(Debug, Clone)]
pub struct Indicator {
    runs: NumericRuns,
    /// Translation emitted at the start of a numeric run (the numsign)
    start_translation: Option<ResolvedTranslation>,
    /// Translation emitted at the end of a numeric run (the non-numsign)
    end_translation: Option<ResolvedTranslation>,
}

impl Indicator {
    /// liblouis's `checkNumericMode` looks at a character only where a rule starts, so
    /// in numeric mode the translation loop steps the indicator itself via [`Self::at`]
    /// instead of using [`Self::precompute`]. Without numeric mode liblouis's
    /// `insertNumberSign` compares a digit with the character before it, which
    /// precomputing does.
    pub fn is_live(&self) -> bool {
        self.runs.uses_numeric_mode()
    }

    /// The translation to emit where a rule starts at `pos`, updating `in_numeric`
    pub fn at(
        &self,
        in_numeric: &mut bool,
        chars: &[char],
        pos: usize,
    ) -> Option<ResolvedTranslation> {
        if pos >= chars.len() {
            return None;
        }
        let event = self.runs.step(*in_numeric, chars, pos);
        *in_numeric = event.in_numeric();
        self.translation(event)
    }

    fn translation(&self, event: RunEvent) -> Option<ResolvedTranslation> {
        match event {
            RunEvent::Start { .. } => self.start_translation.clone(),
            RunEvent::End { nonumsign: true } => self.end_translation.clone(),
            _ => None,
        }
    }

    /// Returns sparse `(position, translation)` pairs.
    pub fn precompute(&self, input: &str) -> Vec<(usize, ResolvedTranslation)> {
        let chars: Vec<char> = input.chars().collect();
        self.runs
            .events(&chars)
            .into_iter()
            .enumerate()
            .filter_map(|(pos, event)| self.translation(event).map(|t| (pos, t)))
            .collect()
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
        builder
            .runs()
            .numeric_characters(HashSet::from(['1', '2', '3']));
        builder.numsign("⠼", &rule("numsign 3456"));
        builder.runs().numericnocontchars("abc");
        let indicator = builder.build().unwrap();

        assert_eq!(
            pairs(&indicator.precompute("ab12 a")),
            vec![(2, "⠼".to_string())]
        );
    }

    #[test]
    fn precompute_end_indication() {
        let mut builder = IndicatorBuilder::new();
        builder
            .runs()
            .numeric_characters(HashSet::from(['1', '2', '3']));
        builder.numsign("⠼", &rule("numsign 3456"));
        builder.nonumsign("⠰", &rule("nonumsign 56"));
        builder.runs().numericnocontchars("abc");
        let indicator = builder.build().unwrap();

        assert_eq!(
            pairs(&indicator.precompute("ab12a")),
            vec![(2, "⠼".to_string()), (4, "⠰".to_string())],
        );
    }

    #[test]
    fn midendnumericmodechars_continue_the_run() {
        let mut builder = IndicatorBuilder::new();
        builder
            .runs()
            .numeric_characters(HashSet::from(['1', '2', '3']));
        builder.numsign("⠼", &rule("numsign 3456"));
        builder.runs().midnum(",");
        builder.runs().midendnumericmodechars(",");
        let indicator = builder.build().unwrap();

        assert_eq!(
            pairs(&indicator.precompute("1,2 3, 1")),
            vec![
                (0, "⠼".to_string()),
                (4, "⠼".to_string()),
                (7, "⠼".to_string())
            ]
        );
        assert_eq!(
            pairs(&indicator.precompute("a,1")),
            vec![(2, "⠼".to_string())]
        );
    }

    #[test]
    fn numeric_mode_ends_at_a_numericnocontchar() {
        let mut builder = IndicatorBuilder::new();
        builder
            .runs()
            .numeric_characters(HashSet::from(['1', '2', '3']));
        builder.numsign("⠼", &rule("numsign 3456"));
        builder.nonumsign("⠰", &rule("nonumsign 56"));
        builder.runs().numericmodechars(".");
        builder.runs().numericnocontchars("abc");
        let indicator = builder.build().unwrap();

        assert_eq!(
            pairs(&indicator.precompute("1.2a")),
            vec![(0, "⠼".to_string()), (3, "⠰".to_string())]
        );
    }

    #[test]
    fn midend_numeric_mode_char_keeps_numeric_mode() {
        let mut builder = IndicatorBuilder::new();
        builder
            .runs()
            .numeric_characters(HashSet::from(['1', '2', '3']));
        builder.numsign("⠼", &rule("numsign 3456"));
        builder.runs().midendnumericmodechars("-");
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
        builder
            .runs()
            .numeric_characters(HashSet::from(['1', '2', '3']));
        builder.numsign("⠼", &rule("numsign 3456"));
        builder.runs().numericmodechars(".,");
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

    #[test]
    fn precompute_midendnumericmodechars() {
        let mut builder = IndicatorBuilder::new();
        builder
            .runs()
            .numeric_characters(HashSet::from(['1', '2', '3']));
        builder.numsign("⠼", &rule("numsign 3456"));
        builder.nonumsign("⠰", &rule("nonumsign 56"));
        builder.runs().numericnocontchars("abc");
        builder.runs().midendnumericmodechars("-");
        let indicator = builder.build().unwrap();

        assert_eq!(
            pairs(&indicator.precompute("1-2-a")),
            vec![(0, "⠼".to_string()), (4, "⠰".to_string())]
        );
        assert_eq!(
            pairs(&indicator.precompute("-1")),
            vec![(1, "⠼".to_string())]
        );
    }

    #[test]
    fn midnum_only_extends_a_run_without_numeric_mode() {
        let mut runs = NumericRuns::default();
        runs.numeric_characters(HashSet::from(['1', '2']));
        runs.midnum(".");
        let chars: Vec<char> = "1.2".chars().collect();
        assert_eq!(runs.events(&chars)[2], RunEvent::Inside);
        runs.numericnocontchars("a");
        assert_eq!(runs.events(&chars)[1], RunEvent::End { nonumsign: false });
    }
}
