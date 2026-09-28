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
//! * [`emphasis::Emphases`]: marks emphasis and capitals the way liblouis does, and
//!   inserts their indicators as the translation loop reaches each position
//! * [`numeric::Indicator`]: detects digit sequences and emits number-sign indicators
//! * [`lettersign::Indicator`]: detects contractions that require a letter sign
//! * [`nocontract::Indicator`]: detects contractions that require a no-contract sign

use crate::emphasis::EmphasisSpan;
use crate::translator::ResolvedTranslation;

pub mod computer_braille;
pub mod emphasis;
pub mod lettersign;
pub mod nocontract;
pub mod numeric;

#[derive(Debug, Clone)]
pub enum Indicator {
    ComputerBraille(computer_braille::Indicator),
    LetterSign(lettersign::Indicator),
    NoContract(nocontract::Indicator),
    Numeric(numeric::Indicator),
}

#[derive(Debug, Clone)]
pub struct Indicators {
    indicators: Vec<Indicator>,
    emphases: Option<emphasis::Emphases>,
}

/// The result of pre-computing indications for a given input.
///
/// Holds per-position braille translations to emit, indexed by
/// character position (with one extra slot at index `n` for translations that
/// must appear after all input characters, e.g. `endemph` at end of string).
pub struct PrecomputedIndications<'a> {
    /// The translations before the emphasis ones, between the emphasis and the capital
    /// start ones, and after the capital start ones
    translations: Vec<[Vec<ResolvedTranslation>; 3]>,
    marks: Option<emphasis::Marks<'a>>,
    /// liblouis's `insertEmphasesFrom`
    from: usize,
    /// A numeric indicator stepped as the translation loop visits positions
    live_numeric: Option<&'a numeric::Indicator>,
    in_numeric: bool,
    chars: Vec<char>,
}

impl PrecomputedIndications<'_> {
    /// The start indicators of the modes and capitals at `pos`, modes first
    fn mode_begin_at(&self, pos: usize) -> Vec<ResolvedTranslation> {
        self.marks
            .as_ref()
            .map(|m| m.caps_begin_at(pos))
            .unwrap_or_default()
    }

    /// The indications at `pos`. Call it each time the translation loop visits a
    /// position, in order; `revisit` when it stays at the position it visited last, where
    /// liblouis inserts only the mode and capital start indicators again.
    ///
    /// Like liblouis's `insertEmphasesAt`, each position since the last visit first
    /// gets its capital and mode ends and its emphasis, and those before `pos` their
    /// mode and capital starts too.
    pub fn translations_at(&mut self, pos: usize, revisit: bool) -> Vec<ResolvedTranslation> {
        let caps = self.mode_begin_at(pos);
        if revisit {
            return caps;
        }
        let mut reached = Vec::new();
        for at in self.from..=pos.min(self.chars.len()) {
            if let Some(m) = &self.marks {
                reached.extend(m.caps_end_at(at));
                reached.extend(m.emphasis_at(at));
            }
            if at < pos {
                reached.extend(self.mode_begin_at(at));
            }
        }
        self.from = pos + 1;
        let numeric = self
            .live_numeric
            .and_then(|n| n.at(&mut self.in_numeric, &self.chars, pos));
        let empty = Default::default();
        let [lead, mid, tail] = self.translations.get(pos).unwrap_or(&empty);
        lead.iter()
            .cloned()
            .chain(reached)
            .chain(mid.iter().cloned())
            .chain(caps)
            .chain(numeric)
            .chain(tail.iter().cloned())
            .collect()
    }

    /// Whether an emphasis or capital mark falls on `pos`
    pub fn marked_at(&self, pos: usize) -> bool {
        self.marks.as_ref().is_some_and(|m| m.marked_at(pos))
    }

    /// Whether an emphasis or capital mark falls inside `pos..pos + len`
    pub fn changes_within(&self, pos: usize, len: usize) -> bool {
        self.marks
            .as_ref()
            .is_some_and(|m| m.changes_within(pos, len))
    }
}

impl Indicators {
    pub fn new(indicators: Vec<Indicator>, emphases: Option<emphasis::Emphases>) -> Indicators {
        Indicators {
            indicators,
            emphases,
        }
    }

    pub fn precompute(&self, input: &str, spans: &[EmphasisSpan]) -> PrecomputedIndications<'_> {
        let chars: Vec<char> = input.chars().collect();
        let mut translations = vec![<[Vec<ResolvedTranslation>; 3]>::default(); chars.len() + 1];
        let mut live_numeric = None;

        for indicator in &self.indicators {
            let (bucket, pairs) = match indicator {
                Indicator::ComputerBraille(i) => (0, i.precompute(input, spans)),
                Indicator::Numeric(i) if i.is_live() => {
                    live_numeric = Some(i);
                    continue;
                }
                Indicator::Numeric(i) => (2, i.precompute(input)),
                Indicator::LetterSign(i) => (1, i.precompute(input)),
                Indicator::NoContract(i) => (1, i.precompute(input)),
            };
            for (pos, t) in pairs {
                translations[pos][bucket].push(t);
            }
        }

        PrecomputedIndications {
            translations,
            marks: self.emphases.as_ref().map(|e| e.mark(&chars, spans)),
            from: 0,
            live_numeric,
            in_numeric: false,
            chars,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        parser::{AnchoredRule, CharacterClass, CharacterClasses, RuleParser},
        translator::table::TableContext,
    };
    use std::collections::HashSet;

    fn rule(s: &str) -> AnchoredRule {
        AnchoredRule::new(RuleParser::new(s).rule().unwrap(), None, 0)
    }

    #[test]
    fn precompute_combines_indicators() {
        let mut num_builder = numeric::IndicatorBuilder::new();
        num_builder
            .runs()
            .numeric_characters(HashSet::from(['1', '2', '3']));
        num_builder.numsign("⠼", &rule("numsign 3456"));
        let numeric = Indicator::Numeric(num_builder.build().unwrap());

        let mut cap_builder = emphasis::IndicatorBuilder::new();
        cap_builder.capsletter("⠠", &rule("capsletter 6"));
        let ctx = TableContext::new(
            CharacterClasses::new(&[
                (CharacterClass::Uppercase, &['A', 'B', 'C']),
                (CharacterClass::Letter, &['a', 'b', 'c', 'A', 'B', 'C']),
            ]),
            CharacterClasses::default(),
            Default::default(),
        );
        let emphases = cap_builder.build(&ctx);

        let indicators = Indicators::new(vec![numeric], emphases);
        let mut indications = indicators.precompute("A1", &[]);

        let mut outputs_at = |pos| {
            indications
                .translations_at(pos, false)
                .into_iter()
                .map(|t| t.output().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(outputs_at(0), vec!["⠠"]); // capsletter for 'A'
        assert_eq!(outputs_at(1), vec!["⠼"]); // numsign for '1'
    }
}
