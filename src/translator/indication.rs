//! Braille indication
//!
//! Braille indicators are dot patterns which are inserted into the braille text
//! to indicate such things as capitalization, italic type, computer braille,
//! etc.
//!
//! Braille indication is handled by a preprocessing pass over the input text.
//! Before translation begins each indicator analyses the full input and records,
//! at each character position, which translations to emit and which rule-selection
//! flags apply. The result is a [`PrecomputedIndications`] value that the
//! translation loop consults by index rather than running stateful logic in
//! parallel with rule selection.
//!
//! The indicator types are:
//! * [`computer_braille::Indicator`]: wraps computer-braille spans with begcomp/endcomp
//! * [`emphasis::Indicator`]: detects emphasis runs and emits letter/word/phrase indicators
//! * [`numeric::Indicator`]: detects digit sequences and emits number-sign indicators
//! * [`uppercase::Indicator`]: detects uppercase runs and emits capitalisation indicators
//! * [`mode::Indicator`]: detects character class runs and emits mode indicators
//! * [`lettersign::Indicator`]: detects contractions that require a letter sign
//! * [`nocontract::Indicator`]: detects contractions that require a no-contract sign

use crate::emphasis::EmphasisSpan;
use crate::parser::Rule;
use crate::translator::ResolvedTranslation;

mod class_indicator;
pub mod computer_braille;
pub mod emphasis;
pub mod lettersign;
pub mod mode;
pub mod nocontract;
pub mod numeric;
pub mod uppercase;

/// Finds maximal spans within `chars[start..end)` where `predicate` holds.
///
/// Used by indicators that need to segment a range into alternating "matching" and
/// "non-matching" runs — e.g. [`uppercase::Indicator`] splitting text into
/// letter/capsmodechar words, or [`emphasis::Indicator`] splitting an active span into
/// words separated by whitespace (or `emphmodechars`).
fn find_spans(
    chars: &[char],
    start: usize,
    end: usize,
    predicate: impl Fn(char) -> bool,
) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut pos = start;
    while pos < end {
        while pos < end && !predicate(chars[pos]) {
            pos += 1;
        }
        if pos >= end {
            break;
        }
        let span_start = pos;
        while pos < end && predicate(chars[pos]) {
            pos += 1;
        }
        spans.push((span_start, pos));
    }
    spans
}

#[derive(Debug, Clone)]
pub enum Indicator {
    ComputerBraille(computer_braille::Indicator),
    Emphasis(emphasis::Indicator),
    LetterSign(lettersign::Indicator),
    Mode(mode::Indicator),
    NoContract(nocontract::Indicator),
    Numeric(numeric::Indicator),
    Uppercase(uppercase::Indicator),
}

#[derive(Debug, Clone)]
pub struct Indicators(Vec<Indicator>);

/// An indicator's translation, whether it is inserted even where a rule spans its
/// position, and its place among the indicators at that position
type Precomputed = (ResolvedTranslation, bool, u8);

fn is_end_indicator(rule: Option<&Rule>) -> bool {
    matches!(
        rule,
        Some(
            Rule::Endcapsword { .. }
                | Rule::Endcaps { .. }
                | Rule::Endcapsphrase { .. }
                | Rule::Endmodeword { .. }
                | Rule::Endmode { .. }
                | Rule::Endmodephrase { .. }
        )
    )
}

/// The result of pre-computing indications for a given input.
///
/// Holds per-position braille translations to emit, indexed by
/// character position (with one extra slot at index `n` for translations that
/// must appear after all input characters, e.g. `endemph` at end of string).
pub struct PrecomputedIndications<'a> {
    /// The translations of the indicators before the numeric one, and after it
    translations: Vec<(Vec<Precomputed>, Vec<Precomputed>)>,
    /// The position after the last one visited
    next: usize,
    /// A numeric indicator stepped as the translation loop visits positions
    live_numeric: Option<&'a numeric::Indicator>,
    in_numeric: bool,
    chars: Vec<char>,
}

impl PrecomputedIndications<'_> {
    /// The indications at `pos`. Call it once for each position the translation loop
    /// visits, in order.
    ///
    /// Like liblouis, the indicators of capitals, modes and emphasis at positions a rule
    /// spanned come first (`insertEmphasesAt` for each position since the last one).
    pub fn translations_at(&mut self, pos: usize) -> Vec<ResolvedTranslation> {
        let skipped = self.next.min(pos)..pos;
        self.next = pos + 1;
        let mut result: Vec<ResolvedTranslation> = self.translations[skipped]
            .iter()
            .flat_map(|(before, after)| before.iter().chain(after))
            .filter(|(_, spanned, _)| *spanned)
            .map(|(t, ..)| t.clone())
            .collect();
        let numeric = self
            .live_numeric
            .and_then(|n| n.at(&mut self.in_numeric, &self.chars, pos));
        if let Some((before, after)) = self.translations.get(pos) {
            result.extend(before.iter().map(|(t, ..)| t.clone()));
            result.extend(numeric);
            result.extend(after.iter().map(|(t, ..)| t.clone()));
        } else {
            result.extend(numeric);
        }
        result
    }

    /// The precomputed indications at `pos`, without the live numeric one
    pub fn precomputed_at(&self, pos: usize) -> impl Iterator<Item = &ResolvedTranslation> {
        self.translations
            .get(pos)
            .into_iter()
            .flat_map(|(before, after)| before.iter().chain(after))
            .map(|(t, ..)| t)
    }
}

impl Indicators {
    pub fn new(indicators: Vec<Indicator>) -> Indicators {
        Indicators(indicators)
    }

    /// Where liblouis's emphasis buffer marks anything for an emphasis class
    pub fn emphasis_marks(&self, input: &str, spans: &[EmphasisSpan]) -> Vec<bool> {
        self.0
            .iter()
            .find_map(|indicator| match indicator {
                Indicator::Emphasis(i) => Some(i.marked(input, spans)),
                _ => None,
            })
            .unwrap_or_else(|| vec![false; input.chars().count() + 1])
    }

    pub fn precompute(&self, input: &str, spans: &[EmphasisSpan]) -> PrecomputedIndications<'_> {
        let n = input.chars().count();
        let mut translations = vec![(vec![], vec![]); n + 1];
        let mut live_numeric = None;

        for indicator in &self.0 {
            let spanned = matches!(indicator, Indicator::Emphasis(_) | Indicator::Mode(_));
            let pairs: Vec<(usize, ResolvedTranslation)> = match indicator {
                Indicator::ComputerBraille(i) => i.precompute(input, spans),
                Indicator::Emphasis(i) => i.precompute(input, spans),
                Indicator::Numeric(i) if i.is_live() => {
                    live_numeric = Some(i);
                    continue;
                }
                Indicator::Numeric(i) => i.precompute(input),
                Indicator::Uppercase(i) => i.precompute(input),
                Indicator::Mode(i) => i.precompute(input),
                Indicator::LetterSign(i) => i.precompute(input),
                Indicator::NoContract(i) => i.precompute(input),
            };
            for (pos, t) in pairs {
                // liblouis ends capitals, then the other modes, before any other indicator
                // but computer braille
                let phase = match indicator {
                    Indicator::ComputerBraille(_) => 0,
                    Indicator::Uppercase(_) if is_end_indicator(t.rule()) => 1,
                    Indicator::Mode(_) if is_end_indicator(t.rule()) => 2,
                    _ => 3,
                };
                let (before, after) = &mut translations[pos];
                if phase < 3 {
                    let at = before
                        .iter()
                        .position(|(_, _, p)| *p > phase)
                        .unwrap_or(before.len());
                    before.insert(at, (t, spanned, phase));
                    continue;
                }
                if live_numeric.is_some() {
                    after.push((t, spanned, phase));
                } else {
                    before.push((t, spanned, phase));
                }
            }
        }

        PrecomputedIndications {
            translations,
            next: 0,
            live_numeric,
            in_numeric: false,
            chars: input.chars().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        parser::{AnchoredRule, RuleParser},
        translator::table::TableContext,
    };
    use std::collections::HashSet;

    fn rule(s: &str) -> AnchoredRule {
        AnchoredRule::new(RuleParser::new(s).rule().unwrap(), None, 0)
    }

    #[test]
    fn find_spans_returns_maximal_matching_runs() {
        let chars: Vec<char> = "  ab cd  e".chars().collect();
        assert_eq!(
            find_spans(&chars, 0, chars.len(), |c| c != ' '),
            vec![(2, 4), (5, 7), (9, 10)]
        );
    }

    #[test]
    fn find_spans_respects_the_given_range() {
        let chars: Vec<char> = "aabbaabb".chars().collect();
        assert_eq!(find_spans(&chars, 2, 6, |c| c == 'b'), vec![(2, 4)]);
    }

    #[test]
    fn precompute_combines_indicators() {
        let mut num_builder = numeric::IndicatorBuilder::new();
        num_builder
            .runs()
            .numeric_characters(HashSet::from(['1', '2', '3']));
        num_builder.numsign("⠼", &rule("numsign 3456"));
        let numeric = Indicator::Numeric(num_builder.build().unwrap());

        let mut cap_builder = uppercase::IndicatorBuilder::new();
        cap_builder.capsletter("⠠", &rule("capsletter 6"));
        cap_builder.uppercase_characters(HashSet::from(['A', 'B', 'C']));
        cap_builder.letter_characters(HashSet::from(['a', 'b', 'c', 'A', 'B', 'C']));
        let uppercase = Indicator::Uppercase(cap_builder.build(&TableContext::default()).unwrap());

        let indicators = Indicators::new(vec![numeric, uppercase]);
        let mut indications = indicators.precompute("A1", &[]);

        let mut outputs_at = |pos| {
            indications
                .translations_at(pos)
                .into_iter()
                .map(|t| t.output().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(outputs_at(0), vec!["⠠"]); // capsletter for 'A'
        assert_eq!(outputs_at(1), vec!["⠼"]); // numsign for '1'
    }

    #[test]
    fn emphasis_at_a_spanned_position_comes_before_the_next_rule() {
        let mut builder = emphasis::IndicatorBuilder::new();
        builder.emphletter("italic", "⠨", &rule("emphletter italic 46"));
        let emphasis = Indicator::Emphasis(builder.build(&TableContext::default()).unwrap());
        let indicators = Indicators::new(vec![emphasis]);
        let mut indications = indicators.precompute("abc", &[EmphasisSpan::new("italic", 1..2)]);
        assert!(indications.translations_at(0).is_empty());
        let outputs: Vec<String> = indications
            .translations_at(2)
            .into_iter()
            .map(|t| t.output().to_string())
            .collect();
        assert_eq!(outputs, vec!["⠨"]);
    }
}
