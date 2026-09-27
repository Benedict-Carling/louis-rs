//! Per-position translation constraints
//!
//! A [`Constrainers`] collection analyses the input text before translation begins and
//! produces a [`PositionConstraints`] value. The translation loop consults this
//! to decide which rules are eligible at each character position.
//!
//! The constrainer types are:
//! * [`Constrainer::Numeric`]: detects digit runs and sets [`Constraint::DontContract`]
//! * [`Constrainer::ComputerBraille`]: maps computer-braille spans to [`Constraint::UseComp6`]

use enumset::{EnumSet, EnumSetType};
use std::collections::HashSet;

use crate::emphasis::EmphasisSpan;
use crate::hyphenation::HyphenationTable;

/// A rule-selection constraint that can be set at a character position.
#[derive(EnumSetType, Debug)]
pub enum Constraint {
    /// Suppress multi-character (contraction) rules at this position.
    ///
    /// Set for every character inside a numeric run so that contractions are
    /// not applied to digit sequences.
    DontContract,
    /// Activate comp6 rules and suppress normal translation rules at this position.
    ///
    /// Set for every character inside a computer-braille span so that comp6
    /// mappings are used instead of the regular translation rules.
    UseComp6,
    /// Uppercase every character produced at this position, in backward translation.
    ///
    /// Set for every letter cell of a `begcapsword`/`endcapsword` span. A single cell can
    /// resolve to more than one character (e.g. a digraph rule), and inside a whole-word
    /// caps span every character of it is capitalized — unlike [`Constraint::TitleCase`].
    Uppercase,
    /// Uppercase only the first character produced at this position, in backward
    /// translation.
    ///
    /// Set for the cell following a `capsletter` indicator. `capsletter` marks a single
    /// capitalized *letter*, not a whole word — if that cell resolves to more than one
    /// character (e.g. the Hungarian `sz` digraph), only the first is capitalized
    /// (`Sz`, not `SZ`), matching liblouis.
    TitleCase,
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

    pub fn uppercase_at(&self, pos: usize) -> bool {
        self.0
            .get(pos)
            .is_some_and(|f| f.contains(Constraint::Uppercase))
    }

    pub fn titlecase_at(&self, pos: usize) -> bool {
        self.0
            .get(pos)
            .is_some_and(|f| f.contains(Constraint::TitleCase))
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

/// Detects numeric runs in the input and sets [`Constraint::DontContract`] at
/// every character position that falls inside one.
#[derive(Debug, Clone)]
pub struct NumericConstrainer {
    numeric_chars: HashSet<char>,
    extra_numeric_chars: HashSet<char>,
    mid_numeric_chars: HashSet<char>,
    /// Whether a number leaves the rest of its word uncontracted, and the characters that
    /// end such a word. liblouis' `checkNumericMode` sets `dontContract` on entering a
    /// number when the nocontractsign is the nonumsign, and only a space or a
    /// `seqdelimiter` clears it.
    word_end_chars: Option<HashSet<char>>,
}

impl NumericConstrainer {
    fn compute(&self, input: &str) -> Vec<Constraints> {
        let chars: Vec<char> = input.chars().collect();
        let mut constraints: Vec<Constraints> = vec![Constraints::empty(); chars.len()];
        let mut in_numeric = false;
        let mut uncontracted_word = false;

        for (pos, &c) in chars.iter().enumerate() {
            if uncontracted_word {
                constraints[pos].insert(Constraint::DontContract);
                if self.word_end_chars.as_ref().is_some_and(|w| w.contains(&c)) {
                    uncontracted_word = false;
                }
            }
            let is_numeric = self.numeric_chars.contains(&c);
            match (is_numeric, in_numeric) {
                (false, false) => {}
                (true, false) => {
                    in_numeric = true;
                    uncontracted_word = self.word_end_chars.is_some();
                    constraints[pos].insert(Constraint::DontContract);
                }
                (true, true) => {
                    constraints[pos].insert(Constraint::DontContract);
                }
                (false, true) if self.extra_numeric_chars.contains(&c) => {
                    constraints[pos].insert(Constraint::DontContract);
                }
                (false, true)
                    if self.extra_numeric_chars.is_empty()
                        && self.mid_numeric_chars.contains(&c)
                        && chars
                            .get(pos + 1)
                            .is_some_and(|nc| self.numeric_chars.contains(nc)) =>
                {
                    constraints[pos].insert(Constraint::DontContract);
                }
                (false, true) => {
                    in_numeric = false;
                }
            }
        }
        constraints
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

/// Returns true if `chars[pos..]` starts with `pattern`.
fn matches_at(chars: &[char], pos: usize, pattern: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    chars[pos..].starts_with(&pattern)
}

/// Detects `capsletter`/`begcapsword`/`endcapsword` indicator dots in backward
/// (braille → text) input and sets [`Constraint::TitleCase`]/[`Constraint::Uppercase`]
/// at every position whose decoded character(s) should be capitalized.
///
/// `capsletter` title-cases just the following cell (a cell can resolve to more than one
/// character, e.g. a digraph rule — only the first is capitalized). `begcapsword`
/// uppercases every character of every subsequent letter cell until `endcapsword` (if
/// defined) or the first non-letter cell, mirroring the forward word-boundary
/// auto-termination rule.
#[derive(Debug, Clone)]
pub struct BackwardCapsConstrainer {
    capsletter_dots: Option<String>,
    begcapsword_dots: Option<String>,
    endcapsword_dots: Option<String>,
    letter_cells: HashSet<char>,
}

impl BackwardCapsConstrainer {
    fn compute(&self, input: &str) -> Vec<Constraints> {
        let chars: Vec<char> = input.chars().collect();
        let n = chars.len();
        let mut constraints: Vec<Constraints> = vec![Constraints::empty(); n];
        let mut pos = 0;
        while pos < n {
            // Check begcapsword before capsletter: capsletter's dots are commonly a
            // prefix of begcapsword's (e.g. `46` vs `46-46`), so checking the shorter
            // pattern first would match a `begcapsword` occurrence one cell early.
            if let Some(dots) = &self.begcapsword_dots
                && matches_at(&chars, pos, dots)
            {
                pos += dots.chars().count();
                loop {
                    if let Some(end_dots) = &self.endcapsword_dots
                        && matches_at(&chars, pos, end_dots)
                    {
                        pos += end_dots.chars().count();
                        break;
                    }
                    if pos < n && self.letter_cells.contains(&chars[pos]) {
                        constraints[pos].insert(Constraint::Uppercase);
                        pos += 1;
                    } else {
                        break;
                    }
                }
                continue;
            }
            if let Some(dots) = &self.capsletter_dots
                && matches_at(&chars, pos, dots)
            {
                pos += dots.chars().count();
                if pos < n {
                    constraints[pos].insert(Constraint::TitleCase);
                }
                continue;
            }
            pos += 1;
        }
        constraints
    }
}

/// A single analysis pass that contributes to [`PositionConstraints`].
#[derive(Debug, Clone)]
pub enum Constrainer {
    /// Detects numeric runs and sets [`Constraint::DontContract`].
    Numeric(NumericConstrainer),
    /// Maps `computer_braille` emphasis spans to [`Constraint::UseComp6`].
    ComputerBraille(ComputerBrailleConstrainer),
    /// Detects backward `capsletter`/`begcapsword`/`endcapsword` dots and sets
    /// [`Constraint::Uppercase`].
    BackwardCaps(BackwardCapsConstrainer),
    /// Detects hyphenation-dictionary breaks and sets [`Constraint::HyphenationBreak`].
    Hyphenation(HyphenationConstrainer),
    /// Detects `prepunc`/`postpunc` scan eligibility and sets
    /// [`Constraint::PrepuncOk`]/[`Constraint::PostpuncOk`].
    Punctuation(PunctuationConstrainer),
}

impl Constrainer {
    fn compute(&self, input: &str, spans: &[EmphasisSpan]) -> Vec<Constraints> {
        match self {
            Constrainer::Numeric(c) => c.compute(input),
            Constrainer::ComputerBraille(c) => c.compute(input, spans),
            Constrainer::BackwardCaps(c) => c.compute(input),
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

/// A builder for [`Constrainer::Numeric`].
#[derive(Debug, Default)]
pub struct NumericConstrainerBuilder {
    numeric_chars: HashSet<char>,
    extra_numeric_chars: HashSet<char>,
    mid_numeric_chars: HashSet<char>,
    uses_numeric_mode: bool,
    nocontractsign: Option<String>,
    nonumsign: Option<String>,
}

impl NumericConstrainerBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn numeric_characters(&mut self, chars: HashSet<char>) {
        self.numeric_chars = chars;
    }

    pub fn numericmodechars(&mut self, s: &str) {
        self.extra_numeric_chars = HashSet::from_iter(s.chars());
        self.uses_numeric_mode = true;
    }

    /// `numericnocontchars` and `midendnumericmodechars` switch liblouis to its numeric
    /// mode handling too.
    pub fn uses_numeric_mode(&mut self) {
        self.uses_numeric_mode = true;
    }

    pub fn nocontractsign(&mut self, dots: &str) {
        self.nocontractsign = Some(dots.to_string());
    }

    pub fn nonumsign(&mut self, dots: &str) {
        self.nonumsign = Some(dots.to_string());
    }

    pub fn midnum(&mut self, s: &str) {
        self.mid_numeric_chars.extend(s.chars());
    }

    /// `word_end_chars` are the table's space and `seqdelimiter` characters.
    pub fn build(self, word_end_chars: HashSet<char>) -> Option<NumericConstrainer> {
        if self.numeric_chars.is_empty() {
            None
        } else {
            let uncontracted_word = self.uses_numeric_mode
                && self.nocontractsign.is_some()
                && self.nocontractsign == self.nonumsign;
            Some(NumericConstrainer {
                numeric_chars: self.numeric_chars,
                extra_numeric_chars: self.extra_numeric_chars,
                mid_numeric_chars: self.mid_numeric_chars,
                word_end_chars: uncontracted_word.then_some(word_end_chars),
            })
        }
    }
}

/// A builder for [`Constrainer::BackwardCaps`].
#[derive(Debug, Default)]
pub struct BackwardCapsConstrainerBuilder {
    capsletter_dots: Option<String>,
    begcapsword_dots: Option<String>,
    endcapsword_dots: Option<String>,
    letter_cells: HashSet<char>,
}

impl BackwardCapsConstrainerBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn capsletter(&mut self, dots: &str) {
        self.capsletter_dots = Some(dots.to_string());
    }

    pub fn begcapsword(&mut self, dots: &str) {
        self.begcapsword_dots = Some(dots.to_string());
    }

    pub fn endcapsword(&mut self, dots: &str) {
        self.endcapsword_dots = Some(dots.to_string());
    }

    pub fn letter_cells(&mut self, cells: HashSet<char>) {
        self.letter_cells = cells;
    }

    pub fn build(self) -> Option<BackwardCapsConstrainer> {
        if self.capsletter_dots.is_none() && self.begcapsword_dots.is_none() {
            None
        } else {
            Some(BackwardCapsConstrainer {
                capsletter_dots: self.capsletter_dots,
                begcapsword_dots: self.begcapsword_dots,
                endcapsword_dots: self.endcapsword_dots,
                letter_cells: self.letter_cells,
            })
        }
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

    fn numeric_constrainers(builder: NumericConstrainerBuilder) -> Constrainers {
        Constrainers::new(vec![Constrainer::Numeric(
            builder.build(HashSet::from([' ', '-'])).unwrap(),
        )])
    }

    #[test]
    fn numeric_run() {
        let mut builder = NumericConstrainerBuilder::new();
        builder.numeric_characters(HashSet::from(['1', '2', '3']));
        let c = numeric_constrainers(builder).precompute("ab12 a", &[]);
        assert_eq!(
            dont_contract(&c),
            vec![false, false, true, true, false, false]
        );
    }

    #[test]
    fn midnum_extends_run() {
        let mut builder = NumericConstrainerBuilder::new();
        builder.numeric_characters(HashSet::from(['1', '2', '3']));
        builder.midnum(".");
        let c = numeric_constrainers(builder).precompute("1.2", &[]);
        assert_eq!(dont_contract(&c), vec![true, true, true]);
    }

    #[test]
    fn number_leaves_rest_of_word_uncontracted() {
        let mut builder = NumericConstrainerBuilder::new();
        builder.numeric_characters(HashSet::from(['1', '2', '3']));
        builder.numericmodechars(".");
        builder.nocontractsign("56");
        builder.nonumsign("56");
        let c = numeric_constrainers(builder).precompute("a1bc d-1e-f", &[]);
        assert_eq!(
            dont_contract(&c),
            vec![
                false, true, true, true, true, false, false, true, true, true, false
            ]
        );
    }

    #[test]
    fn number_contracts_rest_of_word_without_numeric_mode() {
        let mut builder = NumericConstrainerBuilder::new();
        builder.numeric_characters(HashSet::from(['1', '2', '3']));
        builder.nocontractsign("56");
        builder.nonumsign("56");
        let c = numeric_constrainers(builder).precompute("1bc", &[]);
        assert_eq!(dont_contract(&c), vec![true, false, false]);
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

    fn titlecase(c: &PositionConstraints) -> Vec<bool> {
        c.0.iter()
            .map(|f| f.contains(Constraint::TitleCase))
            .collect()
    }

    fn uppercase(c: &PositionConstraints) -> Vec<bool> {
        c.0.iter()
            .map(|f| f.contains(Constraint::Uppercase))
            .collect()
    }

    fn backward_caps_constrainers(builder: BackwardCapsConstrainerBuilder) -> Constrainers {
        Constrainers::new(vec![Constrainer::BackwardCaps(builder.build().unwrap())])
    }

    #[test]
    fn capsletter_marks_next_cell_titlecase() {
        let mut builder = BackwardCapsConstrainerBuilder::new();
        builder.capsletter("C");
        let c = backward_caps_constrainers(builder).precompute("aCbc", &[]);
        assert_eq!(titlecase(&c), vec![false, false, true, false]);
    }

    #[test]
    fn begcapsword_marks_letters_until_non_letter_cell() {
        let mut builder = BackwardCapsConstrainerBuilder::new();
        builder.begcapsword("W");
        builder.letter_cells(HashSet::from(['a', 'b', 'c']));
        let c = backward_caps_constrainers(builder).precompute("Wabc ", &[]);
        assert_eq!(uppercase(&c), vec![false, true, true, true, false]);
    }

    #[test]
    fn begcapsword_stops_at_endcapsword() {
        let mut builder = BackwardCapsConstrainerBuilder::new();
        builder.begcapsword("W");
        builder.endcapsword("E");
        builder.letter_cells(HashSet::from(['a', 'b', 'c']));
        let c = backward_caps_constrainers(builder).precompute("WabEc", &[]);
        assert_eq!(uppercase(&c), vec![false, true, true, false, false]);
    }

    #[test]
    fn begcapsword_checked_before_capsletter_when_dots_share_a_prefix() {
        // Regression: capsletter's dots ("C") are a prefix of begcapsword's ("CC").
        // Checking capsletter first would match it twice instead of begcapsword once.
        let mut builder = BackwardCapsConstrainerBuilder::new();
        builder.capsletter("C");
        builder.begcapsword("CC");
        builder.letter_cells(HashSet::from(['a', 'b']));
        let c = backward_caps_constrainers(builder).precompute("CCab ", &[]);
        assert_eq!(uppercase(&c), vec![false, false, true, true, false]);
        assert_eq!(titlecase(&c), vec![false, false, false, false, false]);
    }
}
