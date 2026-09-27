//! Per-position translation constraints
//!
//! A [`Constrainers`] collection analyses the input text before translation begins and
//! produces a [`PositionConstraints`] value. The translation loop consults this
//! to decide which rules are eligible at each character position.
//!
//! The constrainer types are:
//! * [`Constrainer::DontContract`]: detects numbers and capitals that set [`Constraint::DontContract`]
//! * [`Constrainer::ComputerBraille`]: maps computer-braille spans to [`Constraint::UseComp6`]

use enumset::{EnumSet, EnumSetType};
use std::collections::HashSet;

use crate::emphasis::EmphasisSpan;
use crate::hyphenation::HyphenationTable;
use crate::translator::indication::numeric::{NumericRuns, RunEvent};

/// A rule-selection constraint that can be set at a character position.
#[derive(EnumSetType, Debug)]
pub enum Constraint {
    /// Suppress multi-character (contraction) rules at this position.
    ///
    /// Set from the character after a number's first digit up to the next space, in
    /// tables where liblouis's numeric mode switches contractions off.
    DontContract,
    /// Activate comp6 rules and suppress normal translation rules at this position.
    ///
    /// Set for every character inside a computer-braille span so that comp6
    /// mappings are used instead of the regular translation rules.
    UseComp6,
    /// There's a valid hyphenation break between this character and the next, in
    /// forward translation.
    ///
    /// Set for every internal gap of a word (as delimited by the table's `letter`
    /// class) that the word's hyphenation dictionary marks as a valid break point.
    /// Consulted to decide whether a `nocross` rule may cross this gap.
    HyphenationBreak,
    /// A `prepunc` rule may fire on the punctuation character at this position, in
    /// forward translation.
    PrepuncOk,
    /// A `postpunc` rule may fire on the punctuation character at this position, in
    /// forward translation.
    PostpuncOk,
}

pub type Constraints = EnumSet<Constraint>;

/// The result of pre-computing per-position constraints for a given input.
pub struct PositionConstraints(Vec<Constraints>);

impl PositionConstraints {
    pub fn empty() -> Self {
        Self(Vec::new())
    }

    pub fn dont_contract(&mut self, range: std::ops::Range<usize>) {
        for constraints in &mut self.0[range] {
            constraints.insert(Constraint::DontContract);
        }
    }

    pub fn dont_contract_at(&self, pos: usize) -> bool {
        self.0
            .get(pos)
            .is_some_and(|f| f.contains(Constraint::DontContract))
    }

    pub fn use_comp6_at(&self, pos: usize) -> bool {
        self.0
            .get(pos)
            .is_some_and(|f| f.contains(Constraint::UseComp6))
    }

    pub fn hyphenation_break_at(&self, pos: usize) -> bool {
        self.0
            .get(pos)
            .is_some_and(|f| f.contains(Constraint::HyphenationBreak))
    }

    pub fn prepunc_ok_at(&self, pos: usize) -> bool {
        self.0
            .get(pos)
            .is_some_and(|f| f.contains(Constraint::PrepuncOk))
    }

    pub fn postpunc_ok_at(&self, pos: usize) -> bool {
        self.0
            .get(pos)
            .is_some_and(|f| f.contains(Constraint::PostpuncOk))
    }
}

/// Sets [`Constraint::DontContract`] where liblouis sets `dontContract`: from a digit
/// starting a number when the table uses numeric mode and its nocontractsign is its
/// nonumsign (`checkNumericMode`), and from a run of two or more capitals when the table
/// has `capsnocont`. Either lasts up to and including the next space or `seqdelimiter`.
#[derive(Debug, Clone)]
pub struct DontContractConstrainer {
    runs: Option<NumericRuns>,
    capsnocont: Option<HashSet<char>>,
    reset_chars: HashSet<char>,
}

impl DontContractConstrainer {
    fn compute(&self, input: &str) -> Vec<Constraints> {
        let chars: Vec<char> = input.chars().collect();
        let mut starts = vec![false; chars.len()];
        // checkNumericMode runs after the rule at the first digit is selected
        let mut numeric_starts = vec![false; chars.len()];
        if let Some(runs) = &self.runs {
            for (start, event) in numeric_starts.iter_mut().zip(runs.events(&chars)) {
                *start = event == (RunEvent::Start { by_digit: true });
            }
        }
        if let Some(uppercase) = &self.capsnocont {
            for (pos, c) in chars.iter().enumerate() {
                let upper = |p: usize| chars.get(p).is_some_and(|c| uppercase.contains(c));
                starts[pos] |=
                    uppercase.contains(c) && (upper(pos + 1) || pos > 0 && upper(pos - 1));
            }
        }
        let mut dont_contract = false;
        starts
            .into_iter()
            .zip(numeric_starts)
            .zip(&chars)
            .map(|((start, numeric_start), c)| {
                dont_contract |= start;
                let constraints = if dont_contract {
                    Constraints::only(Constraint::DontContract)
                } else {
                    Constraints::empty()
                };
                dont_contract |= numeric_start;
                if self.reset_chars.contains(c) {
                    dont_contract = false;
                }
                constraints
            })
            .collect()
    }
}

/// Detects the table's `letter`-delimited words in the input and sets
/// [`Constraint::HyphenationBreak`] at every internal gap the hyphenation
/// dictionary marks as a valid break, mirroring liblouis's `syllableBreak`
/// (forward translation only -- back-translation never consults it).
#[derive(Debug, Clone)]
pub struct HyphenationConstrainer {
    hyphenator: HyphenationTable,
    letter_chars: HashSet<char>,
}

impl HyphenationConstrainer {
    fn compute(&self, input: &str) -> Vec<Constraints> {
        let chars: Vec<char> = input.chars().collect();
        let mut constraints: Vec<Constraints> = vec![Constraints::empty(); chars.len()];
        let mut word_start = None;
        for (pos, &c) in chars.iter().enumerate() {
            if self.letter_chars.contains(&c) {
                word_start.get_or_insert(pos);
            } else if let Some(start) = word_start.take() {
                self.mark_word(&mut constraints, &chars, start, pos);
            }
        }
        if let Some(start) = word_start {
            self.mark_word(&mut constraints, &chars, start, chars.len());
        }
        constraints
    }

    /// Marks the breaks of the word spanning `chars[start..end]` at their
    /// absolute positions in `constraints`.
    fn mark_word(&self, constraints: &mut [Constraints], chars: &[char], start: usize, end: usize) {
        let word: String = chars[start..end].iter().collect();
        for (i, is_break) in self.hyphenator.break_points(&word).into_iter().enumerate() {
            if is_break {
                constraints[start + i].insert(Constraint::HyphenationBreak);
            }
        }
    }
}

/// Detects, at every punctuation character position, whether a `prepunc`/`postpunc`
/// rule may fire there, mirroring liblouis's `CTO_PrePunc`/`CTO_PostPunc` (forward
/// translation only; back-translation never consults these opcodes).
///
/// Unlike every other boundary check (`begword`, `midword`, ...), this isn't a
/// fixed-position lookaround: `prepunc` requires scanning forward past any further
/// punctuation for a letter or digit before hitting whitespace (and the mirror image,
/// backward, for `postpunc`), which the trie's single-neighbor
/// `Transition::Start`/`End(Vec<CharacterClass>)` can't express. This is computed once
/// over the whole input, like `HyphenationConstrainer`, rather than as a trie
/// transition.
#[derive(Debug, Clone)]
pub struct PunctuationConstrainer {
    letter_chars: HashSet<char>,
    letter_or_digit_chars: HashSet<char>,
    space_chars: HashSet<char>,
}

impl PunctuationConstrainer {
    fn compute(&self, input: &str) -> Vec<Constraints> {
        let chars: Vec<char> = input.chars().collect();
        let n = chars.len();
        let mut constraints: Vec<Constraints> = vec![Constraints::empty(); n];
        for pos in 0..n {
            // prepunc: not immediately preceded by a letter, then scan forward (past
            // any further punctuation) for a letter/digit before hitting whitespace.
            if pos == 0 || !self.letter_chars.contains(&chars[pos - 1]) {
                for k in pos + 1..n {
                    if self.letter_or_digit_chars.contains(&chars[k]) {
                        constraints[pos].insert(Constraint::PrepuncOk);
                        break;
                    }
                    if self.space_chars.contains(&chars[k]) {
                        break;
                    }
                }
            }
            // postpunc: the mirror image, scanning backward from just before this
            // position.
            if pos == n - 1 || !self.letter_chars.contains(&chars[pos + 1]) {
                for k in (0..pos).rev() {
                    if self.letter_or_digit_chars.contains(&chars[k]) {
                        constraints[pos].insert(Constraint::PostpuncOk);
                        break;
                    }
                    if self.space_chars.contains(&chars[k]) {
                        break;
                    }
                }
            }
        }
        constraints
    }
}

#[derive(Debug, Clone)]
pub struct ComputerBrailleConstrainer {}

impl ComputerBrailleConstrainer {
    fn compute(&self, input: &str, spans: &[EmphasisSpan]) -> Vec<Constraints> {
        let n = input.chars().count();
        let mut constraints: Vec<Constraints> = vec![Constraints::empty(); n];
        for span in spans {
            if span.class == "computer_braille" {
                let end = span.range.end.min(n);
                for pos in span.range.start..end {
                    constraints[pos].insert(Constraint::UseComp6);
                }
            }
        }
        constraints
    }
}

/// A single analysis pass that contributes to [`PositionConstraints`].
#[derive(Debug, Clone)]
pub enum Constrainer {
    /// Detects numbers and runs of capitals and sets [`Constraint::DontContract`].
    DontContract(DontContractConstrainer),
    /// Maps `computer_braille` emphasis spans to [`Constraint::UseComp6`].
    ComputerBraille(ComputerBrailleConstrainer),
    /// Detects hyphenation-dictionary breaks and sets [`Constraint::HyphenationBreak`].
    Hyphenation(HyphenationConstrainer),
    /// Detects `prepunc`/`postpunc` scan eligibility and sets
    /// [`Constraint::PrepuncOk`]/[`Constraint::PostpuncOk`].
    Punctuation(PunctuationConstrainer),
}

impl Constrainer {
    fn compute(&self, input: &str, spans: &[EmphasisSpan]) -> Vec<Constraints> {
        match self {
            Constrainer::DontContract(c) => c.compute(input),
            Constrainer::ComputerBraille(c) => c.compute(input, spans),
            Constrainer::Hyphenation(c) => c.compute(input),
            Constrainer::Punctuation(c) => c.compute(input),
        }
    }
}

/// A collection of [`Constrainer`]s that are applied together.
#[derive(Debug, Clone)]
pub struct Constrainers(Vec<Constrainer>);

impl Constrainers {
    pub fn new(constrainers: Vec<Constrainer>) -> Self {
        Constrainers(constrainers)
    }

    pub fn precompute(&self, input: &str, spans: &[EmphasisSpan]) -> PositionConstraints {
        let n = input.chars().count();
        let mut constraints: Vec<Constraints> = vec![Constraints::empty(); n];
        for constrainer in &self.0 {
            for (accum, computed) in constraints
                .iter_mut()
                .zip(constrainer.compute(input, spans))
            {
                *accum |= computed;
            }
        }
        PositionConstraints(constraints)
    }
}

/// A builder for [`Constrainer::DontContract`].
#[derive(Debug, Default)]
pub struct DontContractConstrainerBuilder {
    nocontractsign: Option<String>,
    nonumsign: Option<String>,
    capsnocont: bool,
    uppercase_chars: HashSet<char>,
    reset_chars: HashSet<char>,
}

impl DontContractConstrainerBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn nocontractsign(&mut self, dots: &str) {
        self.nocontractsign = Some(dots.to_string());
    }

    pub fn nonumsign(&mut self, dots: &str) {
        self.nonumsign = Some(dots.to_string());
    }

    pub fn capsnocont(&mut self) {
        self.capsnocont = true;
    }

    pub fn uppercase_chars(&mut self, chars: HashSet<char>) {
        self.uppercase_chars = chars;
    }

    /// Space and `seqdelimiter` characters, which end the no-contraction span
    pub fn reset_chars(&mut self, chars: impl IntoIterator<Item = char>) {
        self.reset_chars.extend(chars);
    }

    pub fn build(self, runs: NumericRuns) -> Option<DontContractConstrainer> {
        let runs = (runs.uses_numeric_mode()
            && self.nocontractsign.is_some()
            && self.nocontractsign == self.nonumsign)
            .then_some(runs);
        let capsnocont = self.capsnocont.then_some(self.uppercase_chars);
        (runs.is_some() || capsnocont.is_some()).then_some(DontContractConstrainer {
            runs,
            capsnocont,
            reset_chars: self.reset_chars,
        })
    }
}

/// A builder for [`Constrainer::Hyphenation`].
#[derive(Debug, Default)]
pub struct HyphenationConstrainerBuilder {
    hyphenator: Option<HyphenationTable>,
    letter_chars: HashSet<char>,
}

impl HyphenationConstrainerBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn hyphenator(&mut self, hyphenator: HyphenationTable) {
        self.hyphenator = Some(hyphenator);
    }

    pub fn letter_characters(&mut self, chars: HashSet<char>) {
        self.letter_chars = chars;
    }

    pub fn build(self) -> Option<HyphenationConstrainer> {
        Some(HyphenationConstrainer {
            hyphenator: self.hyphenator?,
            letter_chars: self.letter_chars,
        })
    }
}

/// A builder for [`Constrainer::Punctuation`].
#[derive(Debug, Default)]
pub struct PunctuationConstrainerBuilder {
    has_prepunc_or_postpunc: bool,
    letter_chars: HashSet<char>,
    letter_or_digit_chars: HashSet<char>,
    space_chars: HashSet<char>,
}

impl PunctuationConstrainerBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks that the table declares at least one `prepunc`/`postpunc` rule, so
    /// `build()` produces a constrainer. Tables without either opcode skip this
    /// pass entirely.
    pub fn mark_present(&mut self) {
        self.has_prepunc_or_postpunc = true;
    }

    pub fn letter_characters(&mut self, chars: HashSet<char>) {
        self.letter_chars = chars;
    }

    pub fn letter_or_digit_characters(&mut self, chars: HashSet<char>) {
        self.letter_or_digit_chars = chars;
    }

    pub fn space_characters(&mut self, chars: HashSet<char>) {
        self.space_chars = chars;
    }

    pub fn build(self) -> Option<PunctuationConstrainer> {
        if self.has_prepunc_or_postpunc {
            Some(PunctuationConstrainer {
                letter_chars: self.letter_chars,
                letter_or_digit_chars: self.letter_or_digit_chars,
                space_chars: self.space_chars,
            })
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emphasis::EmphasisSpan;

    fn dont_contract(c: &PositionConstraints) -> Vec<bool> {
        c.0.iter()
            .map(|f| f.contains(Constraint::DontContract))
            .collect()
    }

    fn use_comp6(c: &PositionConstraints) -> Vec<bool> {
        c.0.iter()
            .map(|f| f.contains(Constraint::UseComp6))
            .collect()
    }

    fn dont_contract_constrainers(runs: NumericRuns) -> Option<Constrainers> {
        let mut builder = DontContractConstrainerBuilder::new();
        builder.nocontractsign("56");
        builder.nonumsign("56");
        builder.reset_chars([' ', '-']);
        let constrainer = builder.build(runs)?;
        Some(Constrainers::new(vec![Constrainer::DontContract(
            constrainer,
        )]))
    }

    fn digits() -> NumericRuns {
        let mut runs = NumericRuns::default();
        runs.numeric_characters(HashSet::from(['1', '2', '3']));
        runs
    }

    #[test]
    fn number_leaves_rest_of_word_uncontracted() {
        let mut runs = digits();
        runs.numericmodechars(".");
        let c = dont_contract_constrainers(runs)
            .unwrap()
            .precompute("a1bc d-1e-f", &[]);
        assert_eq!(
            dont_contract(&c),
            vec![
                false, false, true, true, true, false, false, false, true, true, false
            ]
        );
    }

    #[test]
    fn number_started_by_numericmodechars_stays_contractable() {
        let mut runs = digits();
        runs.numericmodechars(".");
        let c = dont_contract_constrainers(runs)
            .unwrap()
            .precompute("a.1b", &[]);
        assert_eq!(dont_contract(&c), vec![false, false, false, false]);
    }

    #[test]
    fn number_contracts_without_numeric_mode() {
        assert!(dont_contract_constrainers(digits()).is_none());
    }

    #[test]
    fn number_contracts_when_nocontractsign_is_not_nonumsign() {
        let mut runs = digits();
        runs.numericnocontchars("abc");
        let mut builder = DontContractConstrainerBuilder::new();
        builder.nocontractsign("56");
        builder.nonumsign("6");
        assert!(builder.build(runs).is_none());
    }

    #[test]
    fn capitals_leave_the_rest_of_the_word_uncontracted() {
        let mut builder = DontContractConstrainerBuilder::new();
        builder.capsnocont();
        builder.uppercase_chars(HashSet::from(['A', 'B']));
        builder.reset_chars([' ']);
        let constrainers = Constrainers::new(vec![Constrainer::DontContract(
            builder.build(NumericRuns::default()).unwrap(),
        )]);
        let c = constrainers.precompute("aAB a Aa", &[]);
        assert_eq!(
            dont_contract(&c),
            vec![false, true, true, true, false, false, false, false]
        );
    }

    #[test]
    fn empty_returns_false() {
        let c = PositionConstraints::empty();
        assert!(!c.dont_contract_at(0));
        assert!(!c.dont_contract_at(99));
    }

    #[test]
    fn computer_braille_span_sets_use_comp6() {
        let spans = vec![EmphasisSpan::new("computer_braille", 1..4)];
        let constrainers = Constrainers::new(vec![Constrainer::ComputerBraille(
            ComputerBrailleConstrainer {},
        )]);
        let c = constrainers.precompute("hello", &spans);
        assert_eq!(use_comp6(&c), vec![false, true, true, true, false]);
        assert_eq!(dont_contract(&c), vec![false, false, false, false, false]);
    }
}
