use std::collections::{HashMap, HashSet};

use log::warn;

use crate::{
    Direction,
    emphasis::EmphasisSpan,
    parser::{
        AnchoredRule, Braille, BrailleChars, CharacterClass, CharacterClasses, HasNocross,
        HasPrecedence, Precedence, WithClass,
    },
    translator::{
        CharacterDefinition, ClassConstraint, ResolvedTranslation, Rule, TranslationError,
        TranslationOptions, TranslationStage,
        context_pattern::{ContextPatterns, ContextPatternsBuilder},
        effect::Environment,
        indication::{
            Indicator, Indicators, computer_braille, emphasis, lettersign, mode, nocontract,
            numeric, uppercase,
        },
        match_pattern::{MatchPatterns, MatchPatternsBuilder},
        position_constraints::{
            BackwardCapsConstrainerBuilder, BackwardNumericConstrainerBuilder,
            BackwardWordEndConstrainer, ComputerBrailleConstrainer, Constrainer, Constrainers,
            DontContractConstrainerBuilder, HyphenationConstrainerBuilder, PositionConstraints,
            PunctuationConstrainerBuilder,
        },
        sequence::{Sequences, whole_word_boundaries},
        table::TableContext,
        translation::TranslationSubset,
        trie::{Transition, Trie},
    },
};

/// Detects compbrl-triggered computer braille regions and adds `computer_braille` spans.
///
/// When a table contains `compbrl` rules, any word in the input that contains one
/// of the trigger strings is treated as a computer braille span.
#[derive(Debug, Clone)]
struct CompbrlScanner {
    triggers: Vec<String>,
    character_classes: CharacterClasses,
}

impl CompbrlScanner {
    fn enrich(&self, input: &str, caller_spans: &[EmphasisSpan]) -> Vec<EmphasisSpan> {
        let chars: Vec<char> = input.chars().collect();
        let n = chars.len();
        let mut spans = caller_spans.to_vec();

        let mut pos = 0;
        while pos < n {
            // Check if any trigger matches at this character position.
            let remaining: String = chars[pos..].iter().collect();
            let matched = self
                .triggers
                .iter()
                .any(|t| remaining.starts_with(t.as_str()));
            if matched {
                // Expand to surrounding word: scan backward for whitespace.
                let mut word_start = pos;
                while word_start > 0 && !self.character_classes.is_whitespace(chars[word_start - 1])
                {
                    word_start -= 1;
                }
                // Scan forward for whitespace.
                let mut word_end = pos;
                while word_end < n && !self.character_classes.is_whitespace(chars[word_end]) {
                    word_end += 1;
                }
                let new_span = EmphasisSpan::new("computer_braille", word_start..word_end);
                // Avoid exact duplicates.
                if !spans.contains(&new_span) {
                    spans.push(new_span);
                }
                // Skip to end of word to avoid adding duplicate spans for the same word.
                pos = word_end;
            } else {
                pos += 1;
            }
        }

        spans
    }
}

#[derive(Debug)]
/// Character Translations map characters to a [`ResolvedTranslation`]. This cannot be done using
/// the [`Trie`] as the trie provides a case insensitive mapping only. The [`CharacterTranslation`]
/// on the other hand is case sensitive
struct CharacterTranslation(HashMap<char, ResolvedTranslation>);
impl CharacterTranslation {
    fn new() -> Self {
        Self(HashMap::new())
    }

    fn insert(&mut self, from: char, translation: ResolvedTranslation) {
        if cfg!(feature = "backwards_compatibility") {
            // first rule wins
            self.0.entry(from).or_insert(translation);
        } else {
            // last rule wins
            self.0.insert(from, translation);
        }
    }

    fn get(&self, from: &char) -> Option<&ResolvedTranslation> {
        self.0.get(from)
    }
}

#[derive(Debug)]
pub struct PrimaryTable {
    undefined: Option<String>,
    /// Fallback character definitions that are used as a last resort to translate unknown
    /// characters to their hex representation
    fallback_definitions: CharacterDefinition,
    /// A mapping of characters and their respective [`ResolvedTranslation`]
    character_translations: CharacterTranslation,
    /// A prefix tree that contains all the translation rules and their
    /// [`ResolvedTranslations`](ResolvedTranslation)
    trie: Trie,
    /// A prefix tree that contains comp6 (computer braille) translation rules.
    /// These are only consulted when `UseComp6` is set at a character position.
    comp6_trie: Trie,
    match_patterns: MatchPatterns,
    context_patterns: ContextPatterns,
    /// All the nocross translation rules are stored in a separate trie
    nocross_trie: Trie,
    indicators: Indicators,
    constrainers: Constrainers,
    /// Detects compbrl-triggered computer braille regions.
    compbrl_scanner: Option<CompbrlScanner>,
    direction: Direction,
    /// Backward-only: overrides the trie's default (letter) reading for a braille cell
    /// that is shared between a digit and a letter, keyed by the cell itself. Consulted
    /// only inside a `numsign` span (see [`Constraint::PreferDigit`]).
    ///
    /// [`Constraint::PreferDigit`]: crate::translator::position_constraints::Constraint::PreferDigit
    backward_digit_overrides: HashMap<char, ResolvedTranslation>,
    /// Forward-only: the standing-alone check for `word` and `contraction` rules.
    sequences: Option<Sequences>,
    /// The text character classes, for liblouis's backward `isBegWord`.
    text_classes: CharacterClasses,
}

/// A builder for [`PrimaryTable`]
#[derive(Debug)]
struct PrimaryTableBuilder {
    undefined: Option<String>,
    character_translations: CharacterTranslation,
    trie: Trie,
    comp6_trie: Trie,
    nocross_trie: Trie,
    match_patterns: MatchPatternsBuilder,
    context_patterns: ContextPatternsBuilder,
    numeric_indicator: numeric::IndicatorBuilder,
    uppercase_indicator: uppercase::IndicatorBuilder,
    mode_indicator: mode::IndicatorBuilder,
    lettersign_indicator: lettersign::IndicatorBuilder,
    nocontract_indicator: nocontract::IndicatorBuilder,
    emphasis_indicator: emphasis::IndicatorBuilder,
    computer_braille_indicator: computer_braille::IndicatorBuilder,
    backward_caps_constrainer: BackwardCapsConstrainerBuilder,
    dont_contract_constrainer: DontContractConstrainerBuilder,
    backward_numeric_constrainer: BackwardNumericConstrainerBuilder,
    backward_word_end_constrainer: Option<BackwardWordEndConstrainer>,
    hyphenation_constrainer: HyphenationConstrainerBuilder,
    punctuation_constrainer: PunctuationConstrainerBuilder,
    backward_digit_overrides: HashMap<char, ResolvedTranslation>,
    compbrl_triggers: Vec<String>,
    sequences: Option<Sequences>,
}

impl PrimaryTableBuilder {
    /// `trie_ctx` must be the character classes to resolve lookahead boundary checks
    /// against: text classes for forward translation, dots classes for backward.
    /// `before_ctx` is the same for lookbehind checks, always the text classes. The tries
    /// need them from construction on, since `Trie::insert` resolves those checks against
    /// its own context immediately rather than deferring to match time.
    fn new(trie_ctx: CharacterClasses, before_ctx: CharacterClasses) -> Self {
        let trie = || {
            Trie::new()
                .with_context(trie_ctx.clone())
                .with_before_context(before_ctx.clone())
        };
        Self {
            undefined: None,
            character_translations: CharacterTranslation::new(),
            trie: trie(),
            comp6_trie: trie().case_sensitive(),
            nocross_trie: trie(),
            match_patterns: MatchPatternsBuilder::new(),
            context_patterns: ContextPatternsBuilder::new(),
            numeric_indicator: numeric::IndicatorBuilder::new(),
            uppercase_indicator: uppercase::IndicatorBuilder::new(),
            mode_indicator: mode::IndicatorBuilder::new(),
            lettersign_indicator: lettersign::IndicatorBuilder::new(),
            nocontract_indicator: nocontract::IndicatorBuilder::new(),
            emphasis_indicator: emphasis::IndicatorBuilder::new(),
            computer_braille_indicator: computer_braille::IndicatorBuilder::new(),
            backward_caps_constrainer: BackwardCapsConstrainerBuilder::new(),
            dont_contract_constrainer: DontContractConstrainerBuilder::new(),
            backward_numeric_constrainer: BackwardNumericConstrainerBuilder::new(),
            backward_word_end_constrainer: None,
            hyphenation_constrainer: HyphenationConstrainerBuilder::new(),
            punctuation_constrainer: PunctuationConstrainerBuilder::new(),
            backward_digit_overrides: HashMap::new(),
            compbrl_triggers: Vec::new(),
            sequences: None,
        }
    }

    fn whole_word_boundaries(&self) -> (Transition, Option<Transition>) {
        whole_word_boundaries(self.sequences.as_ref())
    }

    fn get_trie_mut(&mut self, rule: &AnchoredRule) -> &mut Trie {
        if rule.is_nocross() {
            &mut self.nocross_trie
        } else {
            &mut self.trie
        }
    }

    fn insert_character(&mut self, c: char, dots: &str, direction: Direction, rule: &AnchoredRule) {
        match direction {
            Direction::Forward => {
                // Forward translation is case sensitive, so we cannot solely rely on the trie which
                // is case insensitive. We also need to keep the rules in a case sensitive storage
                let translation = ResolvedTranslation::new(
                    c.to_string().as_str(),
                    dots,
                    1,
                    TranslationStage::Main,
                    rule.clone(),
                );
                self.character_translations.insert(c, translation);
            }
            Direction::Backward => {
                // Backward translation is not case sensitive, so we do not need a separate case
                // sensitive storage for translations. We can handle them like other rules via the
                // trie
                self.trie.insert_char(
                    c,
                    dots,
                    direction,
                    rule.precedence(),
                    TranslationStage::Main,
                    rule,
                )
            }
        }
    }

    /// Inserts a zero-output entry for a backward indicator opcode (`capsletter`,
    /// `begcapsword`, `numsign`, ...), so its dots are recognized in backward
    /// translation instead of falling through to a coincidentally-identical
    /// character definition. Only meaningful for `Direction::Backward`.
    fn insert_backward_indicator(&mut self, dots: &str, rule: &AnchoredRule) {
        self.trie.insert(
            "",
            dots,
            None,
            None,
            Direction::Backward,
            rule.precedence(),
            vec![],
            TranslationStage::Main,
            rule,
        );
    }

    /// Like [`Self::insert_backward_indicator`], but only consumes the cell when the
    /// *next* cell is a letter or sign cell.
    ///
    /// `letsign`'s dots are routinely shared with an ordinary punctuation character
    /// (English defines both `letsign 56` and `punctuation ; 56`), so an unguarded
    /// backward indicator would swallow every `;`.
    fn insert_backward_indicator_before_letter(&mut self, dots: &str, rule: &AnchoredRule) {
        self.trie.insert(
            "",
            dots,
            None,
            // liblouis resolves the same ambiguity with `!(beforeAttributes &
            // CTC_Letter) && (afterAttributes & (CTC_Letter | CTC_Sign))`
            // (lou_backTranslateString.c), which it labels "just a heuristic test".
            // Only the lookahead half is reproduced here: there is no negated
            // `CharacterClass`, so the lookbehind has no `Transition` counterpart.
            Some(Transition::End(vec![
                CharacterClass::Letter,
                CharacterClass::Sign,
            ])),
            Direction::Backward,
            rule.precedence(),
            vec![],
            TranslationStage::Main,
            rule,
        );
    }

    /// Returns whether `opcode`'s `dots` are safe to register as a backward indicator:
    /// some tables include a shared subtable purely for other opcodes (e.g.
    /// Malayalam/Punjabi including `en-in-g1.ctb`), whose `capsletter`/`begcapsword`/
    /// `numsign` happen to coincide with one of the table's own script letters. Since
    /// the table itself doesn't actually intend a conflict — the collision is an
    /// accident of composing independently-authored subtables — this is reported as a
    /// warning (the opcode is ignored for backward translation, the letter wins) rather
    /// than a hard error, mirroring how a missing `letsign`/`nocontractionsign`
    /// counterpart is already handled.
    fn backward_indicator_dots_available(
        opcode: &str,
        dots: &str,
        letter_dots: &HashSet<String>,
    ) -> bool {
        if letter_dots.contains(dots) {
            warn!(
                "{opcode} dots {dots} collide with a letter definition in this table; \
                 ignoring {opcode} for backward translation"
            );
            false
        } else {
            true
        }
    }

    /// If `dots` is shared with a letter definition (common in six-dot literary codes,
    /// where digits 1-9,0 conventionally reuse letter cells a-j), records the digit
    /// reading as a backward override for that cell — consulted only inside a
    /// `numsign` span (see `Constraint::PreferDigit`). Only meaningful for
    /// `Direction::Backward`.
    ///
    /// A digit is always a single braille cell, so `dots` normally has length 1; this
    /// is skipped (rather than assumed) for any rule where that doesn't hold, mirroring
    /// `CharacterClasses::insert_dots`.
    fn register_backward_digit_override(
        &mut self,
        character: char,
        dots: &BrailleChars,
        letter_dots: &HashSet<String>,
        rule: &AnchoredRule,
    ) {
        let dots_str = dots.to_string();
        if dots.len() == 1 && letter_dots.contains(&dots_str) {
            let cell = dots.iter().next().unwrap().to_unicode();
            self.backward_digit_overrides.insert(
                cell,
                ResolvedTranslation::new(
                    &dots_str,
                    &character.to_string(),
                    1,
                    TranslationStage::Main,
                    rule.clone(),
                ),
            );
        }
    }

    fn build(mut self, direction: Direction, ctx: &TableContext) -> PrimaryTable {
        let triggers = self.compbrl_triggers;
        let numeric_mode_chars: Vec<char> = self
            .numeric_indicator
            .runs()
            .continuation_chars()
            .copied()
            .collect();
        self.mode_indicator.numeric_mode_chars(numeric_mode_chars);
        let dont_contract_constrainer = self
            .dont_contract_constrainer
            .build(self.numeric_indicator.runs().clone());
        let indicators = [
            // Computer braille must be first so begcomp appears before emphasis.
            self.computer_braille_indicator
                .build()
                .map(Indicator::ComputerBraille),
            // Emphasis must be before capsletter so begemphword appears before capsletter.
            self.emphasis_indicator.build(ctx).map(Indicator::Emphasis),
            self.numeric_indicator.build().map(Indicator::Numeric),
            self.lettersign_indicator
                .build(ctx, self.sequences.clone())
                .map(Indicator::LetterSign),
            self.uppercase_indicator
                .build(ctx)
                .map(Indicator::Uppercase),
            self.mode_indicator.build(ctx).map(Indicator::Mode),
            self.nocontract_indicator
                .build(ctx, self.sequences.clone())
                .map(Indicator::NoContract),
        ]
        .into_iter()
        .flatten()
        .collect();
        PrimaryTable {
            undefined: self.undefined,
            character_translations: self.character_translations,
            direction,
            fallback_definitions: ctx.character_definitions().clone(),
            trie: self.trie,
            comp6_trie: self.comp6_trie,
            nocross_trie: self.nocross_trie,
            match_patterns: self.match_patterns.build(),
            context_patterns: self.context_patterns.build(),
            indicators: Indicators::new(indicators),
            constrainers: Constrainers::new(
                [
                    Some(Constrainer::ComputerBraille(ComputerBrailleConstrainer {})),
                    dont_contract_constrainer.map(Constrainer::DontContract),
                    self.backward_caps_constrainer
                        .build()
                        .map(Constrainer::BackwardCaps),
                    self.backward_numeric_constrainer
                        .build()
                        .map(Constrainer::BackwardNumeric),
                    self.backward_word_end_constrainer
                        .map(Constrainer::BackwardWordEnd),
                    // liblouis's `nocross`/hyphenation-based syllable-break check is a
                    // forward-translation-only mechanism (lou_backTranslateString.c never
                    // consults it); a nocross rule is always an unrestricted candidate
                    // during backward translation.
                    (direction == Direction::Forward)
                        .then(|| self.hyphenation_constrainer.build())
                        .flatten()
                        .map(Constrainer::Hyphenation),
                    // Backward, liblouis checks `prepunc`/`postpunc` with `isBegWord`/
                    // `isEndWord` instead of this scan.
                    (direction == Direction::Forward)
                        .then(|| self.punctuation_constrainer.build())
                        .flatten()
                        .map(Constrainer::Punctuation),
                ]
                .into_iter()
                .flatten()
                .collect(),
            ),
            compbrl_scanner: (!triggers.is_empty()).then(|| CompbrlScanner {
                triggers,
                character_classes: ctx.character_classes.clone(),
            }),
            backward_digit_overrides: self.backward_digit_overrides,
            sequences: self.sequences,
            text_classes: ctx.character_classes.clone(),
        }
    }
}

impl PrimaryTable {
    /// liblouis's backward `isBegWord`: looking back through the output, a space comes
    /// before any letter, digit, math or sign character.
    fn backward_begins_word(&self, translations: &[ResolvedTranslation]) -> bool {
        let is = |class: &CharacterClass, c: char| {
            self.text_classes
                .get(class)
                .is_some_and(|chars| chars.contains(&c))
        };
        let word = [
            CharacterClass::Letter,
            CharacterClass::Digit,
            CharacterClass::Math,
            CharacterClass::Sign,
        ];
        for translation in translations.iter().rev() {
            for c in translation.output().chars().rev() {
                if is(&CharacterClass::Space, c) {
                    return true;
                }
                if word.iter().any(|class| is(class, c)) {
                    return false;
                }
            }
        }
        true
    }

    /// Breaks a tie in rank in backward translation the way liblouis orders the rules of a
    /// cell: a translation rule before any other, and on a single cell every other rule
    /// before a character definition.
    fn backward_order(&self, translation: &ResolvedTranslation) -> (Precedence, bool) {
        if self.direction == Direction::Forward {
            return (Precedence::Default, false);
        }
        let precedence = translation
            .rule()
            .map(|rule| rule.precedence())
            .unwrap_or_default();
        let definition = matches!(
            translation.rule(),
            Some(
                Rule::Space { .. }
                    | Rule::Digit { .. }
                    | Rule::Punctuation { .. }
                    | Rule::Math { .. }
                    | Rule::Sign { .. }
                    | Rule::Letter { .. }
                    | Rule::Uppercase { .. }
                    | Rule::Lowercase { .. }
                    | Rule::Grouping { .. }
            )
        );
        (precedence, !(definition && translation.length() == 1))
    }

    /// Classifies cells the way liblouis's backward `isEndWord` does: by the attributes of
    /// the single-cell character definitions and by the backward rules translating just that
    /// cell.
    fn backward_word_end_constrainer(
        rules: &[AnchoredRule],
        ctx: &TableContext,
    ) -> Result<BackwardWordEndConstrainer, TranslationError> {
        let single_cell = |dots: &str| {
            let mut cells = dots.chars();
            match (cells.next(), cells.next()) {
                (Some(cell), None) => Some(cell),
                _ => None,
            }
        };
        let mut constrainer = BackwardWordEndConstrainer::default();
        for rule in rules {
            let (chars, dots) = match &rule.rule {
                Rule::Letter { dots, .. }
                | Rule::Lowercase { dots, .. }
                | Rule::Uppercase { dots, .. } => {
                    if let Some(cell) = single_cell(&dots.to_string()) {
                        constrainer.letter_cells.insert(cell);
                    }
                    continue;
                }
                Rule::Space { dots, .. } => {
                    if let Some(cell) = single_cell(&dots.to_string()) {
                        constrainer.space_cells.insert(cell);
                    }
                    continue;
                }
                Rule::Hyphen { dots, .. } => {
                    if let Some(cell) = single_cell(&dots.to_string()) {
                        constrainer.hyphen_cells.insert(cell);
                    }
                    continue;
                }
                Rule::Postpunc { chars, dots, .. } => {
                    if let Some(cell) = single_cell(&dots.to_string()) {
                        constrainer.postpunc_cells.insert(cell);
                    }
                    (chars, dots.to_string())
                }
                Rule::Attribute { name, chars }
                    if matches!(name.as_str(), "letter" | "lowercase" | "uppercase") =>
                {
                    for c in chars.chars() {
                        if let Some(cell) = ctx
                            .character_definitions()
                            .get(&c)
                            .and_then(|dots| single_cell(dots))
                        {
                            constrainer.letter_cells.insert(cell);
                        }
                    }
                    continue;
                }
                Rule::Always { chars, dots, .. }
                | Rule::Word { chars, dots, .. }
                | Rule::Sufword { chars, dots, .. }
                | Rule::Prfword { chars, dots, .. }
                | Rule::Begmidword { chars, dots, .. }
                | Rule::Midendword { chars, dots, .. }
                | Rule::Endword { chars, dots, .. }
                | Rule::Partword { chars, dots, .. }
                | Rule::Endnum { chars, dots, .. } => (
                    chars,
                    ctx.character_definitions()
                        .braille_to_unicode(dots, chars)?,
                ),
                Rule::Repeated { chars, dots, .. }
                | Rule::Largesign { chars, dots }
                | Rule::Prepunc { chars, dots, .. }
                | Rule::Begnum { chars, dots, .. }
                | Rule::Midnum { chars, dots, .. } => (chars, dots.to_string()),
                Rule::Lowword { chars, dots, .. } | Rule::Joinword { chars, dots, .. } => {
                    (chars, dots.to_string())
                }
                _ => continue,
            };
            if chars.chars().count() > 1
                && let Some(cell) = single_cell(&dots)
            {
                constrainer.contraction_cells.insert(cell);
            }
        }
        Ok(constrainer)
    }

    /// Resolves `before CLASS`/`after CLASS` rule modifiers into the trie's
    /// `ClassConstraint`s. `before` is a lookahead (the character *after* the match must be
    /// in the class); `after` is a lookbehind (the character *before* the match must be in
    /// the class).
    fn resolve_class_constraints(
        ctx: &TableContext,
        with_classes: &[WithClass],
    ) -> Vec<ClassConstraint> {
        with_classes
            .iter()
            .filter_map(|wc| match wc {
                WithClass::Before { class } => {
                    let key = CharacterClass::from(class.as_str());
                    ctx.character_classes.get(&key)?;
                    Some(ClassConstraint::End(key))
                }
                WithClass::After { class } => {
                    let key = CharacterClass::from(class.as_str());
                    ctx.character_classes.get(&key)?;
                    Some(ClassConstraint::Start(key))
                }
            })
            .collect()
    }

    pub fn compile(
        rules: &[AnchoredRule],
        direction: Direction,
        _stage: TranslationStage,
        ctx: &TableContext,
    ) -> Result<Self, TranslationError> {
        // Word/number boundary checks (`Transition::Start`/`End`, `ClassConstraint`) must
        // consult the same character classes the compiled trie itself is built over:
        // text-based classes for forward translation, but braille-based (dots) classes for
        // backward translation, since the trie's neighbor characters are dots in that
        // direction.
        let trie_ctx = match direction {
            Direction::Forward => ctx.character_classes.clone(),
            Direction::Backward => ctx.dots_classes().clone(),
        };
        // Lookbehind checks always classify text: in backward translation liblouis looks at
        // the output produced so far (`back_setBefore`), not at the preceding cell.
        let mut builder = PrimaryTableBuilder::new(trie_ctx, ctx.character_classes.clone());
        if direction == Direction::Forward {
            builder.sequences = Sequences::new(ctx);
        }

        // What a word-family rule checks after its match. Backward, liblouis scans on
        // through the rest of the word (`isEndWord`) instead of looking at the next cell.
        let backward = direction == Direction::Backward;
        let word_ends = || {
            if backward {
                Transition::EndWord(true)
            } else {
                Transition::End(vec![CharacterClass::Space, CharacterClass::Punctuation])
            }
        };
        let word_continues = || {
            if backward {
                Transition::EndWord(false)
            } else {
                Transition::End(vec![CharacterClass::Letter])
            }
        };

        // In literary braille tables, digit and litdigit serve distinct roles:
        //
        //   digit    — defines the "computer" dot pattern for a digit character, primarily
        //              used for *backward* translation (matching a dot pattern back to a digit).
        //              Some tables (e.g. digits6DotsPlusDot6.uti) add an extra dot so the
        //              pattern is unambiguous in backward translation.
        //
        //   litdigit — defines the *literary* dot pattern used in *forward* translation when
        //              the numeric indicator (numsign) is active.  Tables that target literary
        //              braille include a separate litdigit file alongside the digit file.
        //
        // When both are defined for the same character (a common pattern), litdigit must win
        // for forward translation.  We collect the set of characters that have a litdigit rule
        // so that we can skip the digit rule for those characters in the forward direction.
        let litdigit_chars: HashSet<char> = if direction == Direction::Forward {
            rules
                .iter()
                .filter_map(|r| {
                    if let Rule::Litdigit { character, .. } = &r.rule {
                        Some(*character)
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            HashSet::new()
        };

        // Backward-only: digit dots can coincide with a letter's dots (six-dot literary
        // codes conventionally reuse letter cells a-j for digits 1-9,0). Collect the set
        // of dots used by letter definitions so digit/litdigit rules below can detect the
        // collision and register a numsign-scoped override (see
        // `register_backward_digit_override`), without disturbing the trie's normal
        // (letter) reading used outside a numeric run.
        let letter_dots: HashSet<String> = if direction == Direction::Backward {
            rules
                .iter()
                .filter_map(|r| match &r.rule {
                    Rule::Letter { dots, .. }
                    | Rule::Lowercase { dots, .. }
                    | Rule::Uppercase { dots, .. } => Some(dots.to_string()),
                    _ => None,
                })
                .collect()
        } else {
            HashSet::new()
        };

        // decpoint and hyphen take precedence over other character definition rules, so insert
        // them first
        for rule in rules {
            match &rule.rule {
                Rule::Decpoint {
                    character, dots, ..
                } => {
                    builder.insert_character(*character, &dots.to_string(), direction, rule);
                    // A decimal point extends the numeric run: digits that follow it
                    // must not get a second numsign.  Registering it as a midnum
                    // character achieves this via the numeric indicator and constrainer.
                    let s = character.to_string();
                    builder.numeric_indicator.runs().midnum(&s);
                }
                Rule::Hyphen {
                    character, dots, ..
                } => {
                    builder.insert_character(*character, &dots.to_string(), direction, rule);
                }
                _ => (),
            }
        }
        for rule in rules {
            match &rule.rule {
                Rule::Undefined { dots } => {
                    builder.undefined = Some(dots.to_string());
                }
                Rule::Space {
                    character, dots, ..
                } => {
                    builder.insert_character(*character, &dots.to_string(), direction, rule);
                }
                Rule::Punctuation {
                    character, dots, ..
                } => {
                    builder.insert_character(*character, &dots.to_string(), direction, rule);
                }
                Rule::Digit {
                    character, dots, ..
                } => {
                    // Skip forward translation when a litdigit rule exists for this character;
                    // the litdigit rule is handled below and takes precedence.
                    if !litdigit_chars.contains(character) {
                        builder.insert_character(*character, &dots.to_string(), direction, rule);
                    }
                    if direction == Direction::Backward {
                        builder.register_backward_digit_override(
                            *character,
                            dots,
                            &letter_dots,
                            rule,
                        );
                    }
                }
                Rule::Litdigit {
                    character, dots, ..
                } => {
                    builder.insert_character(*character, &dots.to_string(), direction, rule);
                    if direction == Direction::Backward {
                        builder.register_backward_digit_override(
                            *character,
                            dots,
                            &letter_dots,
                            rule,
                        );
                    }
                }
                Rule::Letter {
                    character, dots, ..
                } => {
                    builder.insert_character(*character, &dots.to_string(), direction, rule);
                }
                Rule::Lowercase {
                    character, dots, ..
                } => {
                    builder.insert_character(*character, &dots.to_string(), direction, rule);
                }
                Rule::Uppercase {
                    character, dots, ..
                } => {
                    builder.insert_character(*character, &dots.to_string(), direction, rule);
                }
                Rule::Sign {
                    character, dots, ..
                } => {
                    builder.insert_character(*character, &dots.to_string(), direction, rule);
                }
                Rule::Math {
                    character, dots, ..
                } => {
                    builder.insert_character(*character, &dots.to_string(), direction, rule);
                }
                Rule::Numsign { dots } => {
                    builder.numeric_indicator.numsign(&dots.to_string(), rule);
                    // See `backward_indicator_dots_available` for why this can be skipped.
                    if direction == Direction::Backward
                        && PrimaryTableBuilder::backward_indicator_dots_available(
                            "numsign",
                            &dots.to_string(),
                            &letter_dots,
                        )
                    {
                        builder.insert_backward_indicator(&dots.to_string(), rule);
                        builder
                            .backward_numeric_constrainer
                            .numsign(&dots.to_string());
                    }
                }
                Rule::Nonumsign { dots, .. } => {
                    builder.numeric_indicator.nonumsign(&dots.to_string(), rule);
                    builder
                        .dont_contract_constrainer
                        .nonumsign(&dots.to_string());
                    if direction == Direction::Backward
                        && PrimaryTableBuilder::backward_indicator_dots_available(
                            "nonumsign",
                            &dots.to_string(),
                            &letter_dots,
                        )
                    {
                        builder.insert_backward_indicator(&dots.to_string(), rule);
                        builder
                            .backward_numeric_constrainer
                            .nonumsign(&dots.to_string());
                    }
                }
                Rule::Numericnocontchars { chars } => {
                    builder.numeric_indicator.runs().numericnocontchars(chars);
                }
                Rule::Capsnocont {} => {
                    builder.dont_contract_constrainer.capsnocont();
                }
                Rule::Numericmodechars { chars } => {
                    builder.numeric_indicator.runs().numericmodechars(chars);
                }
                Rule::Midendnumericmodechars { chars } => {
                    builder
                        .numeric_indicator
                        .runs()
                        .midendnumericmodechars(chars);
                }
                Rule::Capsletter { dots, .. } => {
                    builder
                        .uppercase_indicator
                        .capsletter(&dots.to_string(), rule);
                    // See `backward_indicator_dots_available` for why this can be skipped.
                    if direction == Direction::Backward
                        && PrimaryTableBuilder::backward_indicator_dots_available(
                            "capsletter",
                            &dots.to_string(),
                            &letter_dots,
                        )
                    {
                        builder.insert_backward_indicator(&dots.to_string(), rule);
                        builder
                            .backward_caps_constrainer
                            .capsletter(&dots.to_string());
                    }
                }
                Rule::Begcapsword { dots, .. } => {
                    builder
                        .uppercase_indicator
                        .begcapsword(&dots.to_string(), rule);
                    if direction == Direction::Backward
                        && PrimaryTableBuilder::backward_indicator_dots_available(
                            "begcapsword",
                            &dots.to_string(),
                            &letter_dots,
                        )
                    {
                        builder.insert_backward_indicator(&dots.to_string(), rule);
                        builder
                            .backward_caps_constrainer
                            .begcapsword(&dots.to_string());
                    }
                }
                Rule::Endcapsword { dots, .. } => {
                    builder
                        .uppercase_indicator
                        .endcapsword(&dots.to_string(), rule);
                    if direction == Direction::Backward
                        && PrimaryTableBuilder::backward_indicator_dots_available(
                            "endcapsword",
                            &dots.to_string(),
                            &letter_dots,
                        )
                    {
                        builder.insert_backward_indicator(&dots.to_string(), rule);
                        builder
                            .backward_caps_constrainer
                            .endcapsword(&dots.to_string());
                    }
                }
                Rule::Begcaps { dots } => {
                    builder.uppercase_indicator.begcaps(&dots.to_string(), rule);
                }
                Rule::Endcaps { dots } => {
                    builder.uppercase_indicator.endcaps(&dots.to_string(), rule);
                }
                Rule::Capsmodechars { chars } => {
                    builder.uppercase_indicator.capsmodechars(chars);
                }
                Rule::Begcapsphrase { dots } => {
                    builder
                        .uppercase_indicator
                        .begcapsphrase(&dots.to_string(), rule);
                }
                Rule::Endcapsphrase { dots, position } => {
                    builder
                        .uppercase_indicator
                        .endcapsphrase(&dots.to_string(), position, rule);
                }
                Rule::Lencapsphrase { number } => {
                    builder.uppercase_indicator.lencapsphrase(*number as usize);
                }
                Rule::Modeletter { name, dots, .. } => {
                    builder
                        .mode_indicator
                        .modeletter(name, &dots.to_string(), rule);
                }
                Rule::Begmodeword { name, dots, .. } => {
                    builder
                        .mode_indicator
                        .begmodeword(name, &dots.to_string(), rule);
                }
                Rule::Endmodeword { name, dots, .. } => {
                    builder
                        .mode_indicator
                        .endmodeword(name, &dots.to_string(), rule);
                }
                Rule::Begmode { name, dots } => {
                    builder
                        .mode_indicator
                        .begmode(name, &dots.to_string(), rule);
                }
                Rule::Endmode { name, dots } => {
                    builder
                        .mode_indicator
                        .endmode(name, &dots.to_string(), rule);
                }
                Rule::Begmodephrase { name, dots } => {
                    builder
                        .mode_indicator
                        .begmodephrase(name, &dots.to_string(), rule);
                }
                Rule::Endmodephrase {
                    name,
                    dots,
                    position,
                } => {
                    builder
                        .mode_indicator
                        .endmodephrase(name, &dots.to_string(), position, rule);
                }
                Rule::Lenmodephrase { name, number } => {
                    builder.mode_indicator.lenmodephrase(name, *number as usize);
                }
                Rule::Emphclass { name } => {
                    builder.emphasis_indicator.emphclass(name);
                }
                Rule::Emphletter { name, dots } => {
                    builder
                        .emphasis_indicator
                        .emphletter(name, &dots.to_string(), rule);
                }
                Rule::Begemphword { name, dots } => {
                    builder
                        .emphasis_indicator
                        .begemphword(name, &dots.to_string(), rule);
                }
                Rule::Endemphword { name, dots } => {
                    builder
                        .emphasis_indicator
                        .endemphword(name, &dots.to_string(), rule);
                }
                Rule::Begemphphrase { name, dots } => {
                    builder
                        .emphasis_indicator
                        .begemphphrase(name, &dots.to_string(), rule);
                }
                Rule::Endemphphrase {
                    name,
                    dots,
                    position,
                } => {
                    builder.emphasis_indicator.endemphphrase(
                        name,
                        &dots.to_string(),
                        position,
                        rule,
                    );
                }
                Rule::Lenemphphrase { name, number } => {
                    builder
                        .emphasis_indicator
                        .lenemphphrase(name, *number as usize);
                }
                Rule::Begemph { name, dots, .. } => {
                    builder
                        .emphasis_indicator
                        .begemph(name, &dots.to_string(), rule);
                }
                Rule::Endemph { name, dots, .. } => {
                    builder
                        .emphasis_indicator
                        .endemph(name, &dots.to_string(), rule);
                }
                Rule::Emphmodechars { name, chars } => {
                    builder.emphasis_indicator.emphmodechars(name, chars);
                }
                Rule::Noemphchars { name, chars } => {
                    builder.emphasis_indicator.noemphchars(name, chars);
                }
                Rule::Multind { dots, .. } if direction == Direction::Backward => {
                    builder.insert_backward_indicator(&dots.to_string(), rule);
                }
                Rule::Letsign { dots } => {
                    builder
                        .lettersign_indicator
                        .letsign(&dots.to_string(), rule);
                    if direction == Direction::Backward
                        && PrimaryTableBuilder::backward_indicator_dots_available(
                            "letsign",
                            &dots.to_string(),
                            &letter_dots,
                        )
                    {
                        builder.insert_backward_indicator_before_letter(&dots.to_string(), rule);
                    }
                }
                Rule::Noletsign { chars } => {
                    builder.lettersign_indicator.noletsign(chars);
                }
                Rule::Noletsignafter { chars } => {
                    builder.lettersign_indicator.noletsignafter(chars);
                }
                Rule::Noletsignbefore { chars } => {
                    builder.lettersign_indicator.noletsignbefore(chars);
                }
                // Treat a contraction rule similarly to a word rule, but forward only
                Rule::Contraction { chars } => {
                    if direction == Direction::Forward {
                        let dots = ctx
                            .character_definitions()
                            .braille_to_unicode(&Braille::Implicit, chars)?;
                        let (start, end) = builder.whole_word_boundaries();
                        builder.get_trie_mut(rule).insert(
                            chars,
                            &dots,
                            Some(start),
                            end,
                            direction,
                            rule.precedence(),
                            vec![],
                            TranslationStage::Main,
                            rule,
                        );
                    }
                    builder.lettersign_indicator.contraction(chars, rule);
                    builder.nocontract_indicator.contraction(chars, rule);
                }
                Rule::Nocontractsign { dots, .. } => {
                    builder
                        .dont_contract_constrainer
                        .nocontractsign(&dots.to_string());
                    builder
                        .nocontract_indicator
                        .nocontractsign(&dots.to_string(), rule);
                    builder
                        .dont_contract_constrainer
                        .nocontractsign(&dots.to_string());
                    if direction == Direction::Backward
                        && PrimaryTableBuilder::backward_indicator_dots_available(
                            "nocontractsign",
                            &dots.to_string(),
                            &letter_dots,
                        )
                    {
                        builder.insert_backward_indicator_before_letter(&dots.to_string(), rule);
                    }
                }
                Rule::Base { derived, base, .. } => {
                    builder.mode_indicator.base(*derived, *base);
                    if let Some(translation) = ctx.character_definitions().get(base).cloned() {
                        builder.insert_character(*derived, &translation, direction, rule);
                    } else {
                        // hm, there is no character definition for the base character.
                        // If we are backwards compatible ignore the problem, otherwise
                        // throw an error
                        if !cfg!(feature = "backwards_compatibility") {
                            return Err(TranslationError::BaseCharacterNotDefined {
                                base: *base,
                                derived: *derived,
                            });
                        }
                    }
                }
                Rule::Comp6 { character, dots } => {
                    let character = character.to_string();
                    let dots = ctx
                        .character_definitions()
                        .braille_to_unicode(dots, &character)?;
                    builder.comp6_trie.insert(
                        &character,
                        &dots,
                        None,
                        None,
                        direction,
                        rule.precedence(),
                        vec![],
                        TranslationStage::Main,
                        rule,
                    );
                }
                Rule::Always {
                    chars,
                    dots,
                    with_classes,
                    ..
                } => {
                    let dots = ctx
                        .character_definitions()
                        .braille_to_unicode(dots, chars)?;
                    let class_constraints = Self::resolve_class_constraints(ctx, with_classes);
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots,
                        None,
                        None,
                        direction,
                        rule.precedence(),
                        class_constraints,
                        TranslationStage::Main,
                        rule,
                    );
                }
                // A syllable is an unconditional substitution, like `always`. However it also
                // restricts other contractions from crossing a syllable's boundaries, which is
                // not implemented here.
                //
                // TODO: Implement the cross-boundary restriction.
                Rule::Syllable { chars, dots } => {
                    let dots = ctx
                        .character_definitions()
                        .braille_to_unicode(dots, chars)?;
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots,
                        None,
                        None,
                        direction,
                        rule.precedence(),
                        vec![],
                        TranslationStage::Main,
                        rule,
                    );
                }
                Rule::Begcomp { dots, .. } => {
                    builder
                        .computer_braille_indicator
                        .begcomp(&dots.to_string(), rule);
                }
                Rule::Endcomp { dots, .. } => {
                    builder
                        .computer_braille_indicator
                        .endcomp(&dots.to_string(), rule);
                }
                Rule::Compbrl { chars, .. } => {
                    builder.compbrl_triggers.push(chars.clone());
                }
                Rule::Largesign { chars, dots } => {
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots.to_string(),
                        None,
                        None,
                        direction,
                        rule.precedence(),
                        vec![],
                        TranslationStage::Main,
                        rule,
                    );
                }
                Rule::Word {
                    chars,
                    dots,
                    with_classes,
                    ..
                } => {
                    let dots = ctx
                        .character_definitions()
                        .braille_to_unicode(dots, chars)?;
                    let class_constraints = Self::resolve_class_constraints(ctx, with_classes);
                    let (start, end) = builder.whole_word_boundaries();
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots,
                        Some(start),
                        if backward { Some(word_ends()) } else { end },
                        direction,
                        rule.precedence(),
                        class_constraints,
                        TranslationStage::Main,
                        rule,
                    );
                }
                Rule::Begword {
                    chars,
                    dots,
                    with_classes,
                    ..
                } => {
                    let dots = ctx
                        .character_definitions()
                        .braille_to_unicode(dots, chars)?;
                    let class_constraints = Self::resolve_class_constraints(ctx, with_classes);
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots,
                        Some(Transition::Start(vec![
                            CharacterClass::Space,
                            CharacterClass::Punctuation,
                        ])),
                        Some(word_continues()),
                        direction,
                        rule.precedence(),
                        class_constraints,
                        TranslationStage::Main,
                        rule,
                    )
                }
                Rule::Midword {
                    chars,
                    dots,
                    with_classes,
                    ..
                } => {
                    let dots = ctx
                        .character_definitions()
                        .braille_to_unicode(dots, chars)?;
                    let class_constraints = Self::resolve_class_constraints(ctx, with_classes);
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots,
                        Some(Transition::Start(vec![CharacterClass::Letter])),
                        Some(word_continues()),
                        direction,
                        rule.precedence(),
                        class_constraints,
                        TranslationStage::Main,
                        rule,
                    )
                }
                Rule::Partword {
                    chars,
                    dots,
                    with_classes,
                    ..
                } => {
                    // partword fires when preceded OR followed by a letter (i.e. not standalone).
                    // Two insertions cover all non-standalone positions:
                    // 1. preceded by word char (midword + endword)
                    // 2. at word-start followed by word char (begword)
                    let dots = ctx
                        .character_definitions()
                        .braille_to_unicode(dots, chars)?;
                    let class_constraints = Self::resolve_class_constraints(ctx, with_classes);
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots,
                        Some(Transition::Start(vec![CharacterClass::Letter])),
                        None,
                        direction,
                        rule.precedence(),
                        class_constraints.clone(),
                        TranslationStage::Main,
                        rule,
                    );
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots,
                        None,
                        Some(word_continues()),
                        direction,
                        rule.precedence(),
                        class_constraints,
                        TranslationStage::Main,
                        rule,
                    );
                }
                Rule::Midendword {
                    chars,
                    dots,
                    with_classes,
                    ..
                } => {
                    let dots = ctx
                        .character_definitions()
                        .braille_to_unicode(dots, chars)?;
                    let class_constraints = Self::resolve_class_constraints(ctx, with_classes);
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots,
                        Some(Transition::Start(vec![CharacterClass::Letter])),
                        (!backward).then(|| {
                            Transition::End(vec![
                                CharacterClass::Letter,
                                CharacterClass::Space,
                                CharacterClass::Punctuation,
                            ])
                        }),
                        direction,
                        rule.precedence(),
                        class_constraints,
                        TranslationStage::Main,
                        rule,
                    );
                }
                Rule::Endword {
                    chars,
                    dots,
                    with_classes,
                    ..
                } => {
                    let dots = ctx
                        .character_definitions()
                        .braille_to_unicode(dots, chars)?;
                    let class_constraints = Self::resolve_class_constraints(ctx, with_classes);
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots,
                        Some(Transition::Start(vec![CharacterClass::Letter])),
                        Some(word_ends()),
                        direction,
                        rule.precedence(),
                        class_constraints,
                        TranslationStage::Main,
                        rule,
                    );
                }
                Rule::Prfword {
                    chars,
                    dots,
                    with_classes,
                    ..
                } => {
                    let dots = ctx
                        .character_definitions()
                        .braille_to_unicode(dots, chars)?;
                    let class_constraints = Self::resolve_class_constraints(ctx, with_classes);
                    // a prfword is basically syntactic sugar for a word rule combined with an
                    // endword rule. So just make the two appropriate insertions in the trie
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots,
                        Some(Transition::Start(vec![
                            CharacterClass::Space,
                            CharacterClass::Punctuation,
                        ])),
                        Some(word_ends()),
                        direction,
                        rule.precedence(),
                        class_constraints.clone(),
                        TranslationStage::Main,
                        rule,
                    );
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots,
                        Some(Transition::Start(vec![CharacterClass::Letter])),
                        Some(word_ends()),
                        direction,
                        rule.precedence(),
                        class_constraints,
                        TranslationStage::Main,
                        rule,
                    );
                }
                Rule::Sufword {
                    chars,
                    dots,
                    with_classes,
                    ..
                } => {
                    let dots = ctx
                        .character_definitions()
                        .braille_to_unicode(dots, chars)?;
                    let class_constraints = Self::resolve_class_constraints(ctx, with_classes);
                    // a sufword is basically syntactic sugar for a word rule combined with an
                    // begword rule. So just make the two appropriate insertions in the trie
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots,
                        Some(Transition::Start(vec![
                            CharacterClass::Space,
                            CharacterClass::Punctuation,
                        ])),
                        (!backward).then(|| {
                            Transition::End(vec![
                                CharacterClass::Space,
                                CharacterClass::Punctuation,
                            ])
                        }),
                        direction,
                        rule.precedence(),
                        class_constraints.clone(),
                        TranslationStage::Main,
                        rule,
                    );
                    // liblouis checks nothing after a sufword in backward translation
                    if !backward {
                        builder.get_trie_mut(rule).insert(
                            chars,
                            &dots,
                            Some(Transition::Start(vec![
                                CharacterClass::Space,
                                CharacterClass::Punctuation,
                            ])),
                            Some(Transition::End(vec![CharacterClass::Letter])),
                            direction,
                            rule.precedence(),
                            class_constraints,
                            TranslationStage::Main,
                            rule,
                        );
                    }
                }
                Rule::Begmidword {
                    chars,
                    dots,
                    with_classes,
                    ..
                } => {
                    let dots = ctx
                        .character_definitions()
                        .braille_to_unicode(dots, chars)?;
                    let class_constraints = Self::resolve_class_constraints(ctx, with_classes);
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots,
                        Some(Transition::Start(vec![
                            CharacterClass::Letter,
                            CharacterClass::Space,
                            CharacterClass::Punctuation,
                        ])),
                        Some(word_continues()),
                        direction,
                        rule.precedence(),
                        class_constraints,
                        TranslationStage::Main,
                        rule,
                    );
                }
                Rule::Lowword { chars, dots, .. } => builder.get_trie_mut(rule).insert(
                    chars,
                    &dots.to_string(),
                    Some(Transition::Start(vec![CharacterClass::Space])),
                    Some(Transition::End(vec![CharacterClass::Space])),
                    direction,
                    rule.precedence(),
                    vec![],
                    TranslationStage::Main,
                    rule,
                ),
                Rule::Joinword { chars, dots, .. } => {
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots.to_string(),
                        // FIXME: joinword, prepunc and postpunc need more advanced
                        // scaning of the input. Just checking for before and after
                        // classes as below is too simplistic
                        Some(Transition::Start(vec![
                            CharacterClass::Space,
                            CharacterClass::Punctuation,
                        ])),
                        Some(Transition::End(vec![
                            CharacterClass::Space,
                            CharacterClass::Punctuation,
                        ])),
                        direction,
                        rule.precedence(),
                        vec![],
                        TranslationStage::Main,
                        rule,
                    )
                }
                Rule::Begnum { chars, dots, .. } => builder.get_trie_mut(rule).insert(
                    chars,
                    &dots.to_string(),
                    Some(Transition::Start(vec![
                        CharacterClass::Space,
                        CharacterClass::Punctuation,
                    ])),
                    Some(Transition::End(vec![CharacterClass::Digit])),
                    direction,
                    rule.precedence(),
                    vec![],
                    TranslationStage::Main,
                    rule,
                ),
                Rule::Midnum { chars, dots, .. } => {
                    builder.numeric_indicator.runs().midnum(chars);
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots.to_string(),
                        Some(Transition::Start(vec![CharacterClass::Digit])),
                        Some(Transition::End(vec![CharacterClass::Digit])),
                        direction,
                        rule.precedence(),
                        vec![],
                        TranslationStage::Main,
                        rule,
                    );
                }
                Rule::Endnum { chars, dots, .. } => {
                    let dots = ctx
                        .character_definitions()
                        .braille_to_unicode(dots, chars)?;
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots,
                        Some(Transition::Start(vec![CharacterClass::Digit])),
                        // liblouis ends a number backward only before a non-digit cell
                        backward.then(|| Transition::NotEnd(vec![CharacterClass::Litdigit])),
                        direction,
                        rule.precedence(),
                        vec![],
                        TranslationStage::Main,
                        rule,
                    );
                }
                Rule::Prepunc { chars, dots, .. } => {
                    // are all the characters punctuation?
                    if chars.chars().all(|c| {
                        ctx.character_classes
                            .get(&CharacterClass::Punctuation)
                            .is_some_and(|class| class.contains(&c))
                    }) {
                        // Eligibility isn't a fixed-position lookaround (see
                        // `PunctuationConstrainer`), so the trie itself imposes no
                        // boundary; `constraints.prepunc_ok_at()` gates it in `trace()`.
                        // Backward, `backward_begins_word()` does; the unweighted check
                        // only keeps it apart from a same-cell character definition.
                        builder.punctuation_constrainer.mark_present();
                        builder.get_trie_mut(rule).insert(
                            chars,
                            &dots.to_string(),
                            backward.then(|| {
                                Transition::Unweighted(Box::new(Transition::Start(vec![
                                    CharacterClass::Space,
                                    CharacterClass::Punctuation,
                                ])))
                            }),
                            None,
                            direction,
                            rule.precedence(),
                            vec![],
                            TranslationStage::Main,
                            rule,
                        );
                    }
                }
                Rule::Postpunc { chars, dots, .. } => {
                    // are all the characters punctuation?
                    if chars.chars().all(|c| {
                        ctx.character_classes
                            .get(&CharacterClass::Punctuation)
                            .is_some_and(|class| class.contains(&c))
                    }) {
                        // See the comment in the `Rule::Prepunc` arm above. Backward,
                        // liblouis checks `isEndWord`, which does not make it more
                        // specific than a same-cell rule.
                        builder.punctuation_constrainer.mark_present();
                        builder.get_trie_mut(rule).insert(
                            chars,
                            &dots.to_string(),
                            None,
                            backward.then(|| {
                                Transition::Unweighted(Box::new(Transition::EndWord(true)))
                            }),
                            direction,
                            rule.precedence(),
                            vec![],
                            TranslationStage::Main,
                            rule,
                        );
                    }
                }
                Rule::Match {
                    pre,
                    chars,
                    post,
                    dots,
                    ..
                } => {
                    let dots = ctx
                        .character_definitions()
                        .braille_to_unicode(dots, chars)?;
                    builder.match_patterns.insert(
                        pre,
                        chars,
                        post,
                        &dots,
                        rule,
                        ctx.character_classes(),
                    );
                }
                Rule::Context { test, action, .. } => {
                    builder.context_patterns.insert(
                        test,
                        action,
                        rule,
                        TranslationStage::Main,
                        ctx,
                    )?;
                }
                Rule::IncludeHyphenation { table, .. } => {
                    builder.hyphenation_constrainer.hyphenator(table.clone());
                }
                _ => (),
            }
        }
        let numeric_chars = ctx
            .character_classes()
            .get(&CharacterClass::Litdigit)
            .unwrap_or_default();
        builder
            .numeric_indicator
            .runs()
            .numeric_characters(numeric_chars);
        builder.dont_contract_constrainer.uppercase_chars(
            ctx.character_classes()
                .get(&CharacterClass::Uppercase)
                .unwrap_or_default(),
        );
        for class in [CharacterClass::Space, CharacterClass::Seqdelimiter] {
            builder
                .dont_contract_constrainer
                .reset_chars(ctx.character_classes().get(&class).unwrap_or_default());
        }
        builder.uppercase_indicator.uppercase_characters(
            ctx.character_classes()
                .get(&CharacterClass::Uppercase)
                .unwrap_or_default(),
        );
        builder.uppercase_indicator.letter_characters(
            ctx.character_classes()
                .get(&CharacterClass::Letter)
                .unwrap_or_default(),
        );
        builder.hyphenation_constrainer.letter_characters(
            ctx.character_classes()
                .get(&CharacterClass::Letter)
                .unwrap_or_default(),
        );
        let letter_chars = ctx
            .character_classes()
            .get(&CharacterClass::Letter)
            .unwrap_or_default();
        builder
            .punctuation_constrainer
            .letter_characters(letter_chars.clone());
        builder.punctuation_constrainer.letter_or_digit_characters(
            letter_chars
                .union(
                    &ctx.character_classes()
                        .get(&CharacterClass::Digit)
                        .unwrap_or_default(),
                )
                .copied()
                .collect(),
        );
        builder.punctuation_constrainer.space_characters(
            ctx.character_classes()
                .get(&CharacterClass::Space)
                .unwrap_or_default(),
        );
        if direction == Direction::Backward {
            let mut letter_cells = ctx
                .dots_classes()
                .get(&CharacterClass::Letter)
                .unwrap_or_default();
            // Contractions spelling letters (e.g. Hungarian "sz"/"cs" digraphs) define
            // their own dots, which are no letter by definition; a begcapsword span or a
            // numeric run must still treat them as letters.
            for rule in rules {
                match &rule.rule {
                    Rule::Always { chars, dots, .. }
                    | Rule::Word { chars, dots, .. }
                    | Rule::Sufword { chars, dots, .. }
                    | Rule::Prfword { chars, dots, .. }
                    | Rule::Begword { chars, dots, .. }
                    | Rule::Begmidword { chars, dots, .. }
                    | Rule::Midword { chars, dots, .. }
                    | Rule::Midendword { chars, dots, .. }
                    | Rule::Endword { chars, dots, .. }
                    | Rule::Partword { chars, dots, .. }
                        if chars.chars().all(|c| c.is_alphabetic()) =>
                    {
                        letter_cells.extend(dots.to_string().chars());
                    }
                    Rule::Lowword { chars, dots, .. } | Rule::Joinword { chars, dots, .. }
                        if chars.chars().all(|c| c.is_alphabetic()) =>
                    {
                        letter_cells.extend(dots.to_string().chars());
                    }
                    _ => (),
                }
            }
            builder
                .backward_numeric_constrainer
                .letter_cells(letter_cells.clone());
            builder.backward_caps_constrainer.letter_cells(letter_cells);
            builder.backward_word_end_constrainer =
                Some(Self::backward_word_end_constrainer(rules, ctx)?);
            let litdigit_cells = ctx
                .dots_classes()
                .get(&CharacterClass::Litdigit)
                .unwrap_or_default();
            let mut digit_cells = ctx
                .dots_classes()
                .get(&CharacterClass::Digit)
                .unwrap_or_default();
            digit_cells.extend(litdigit_cells.iter().copied());
            builder
                .backward_numeric_constrainer
                .litdigit_cells(litdigit_cells);
            builder
                .backward_numeric_constrainer
                .digit_cells(digit_cells);
            let continuation_dots = builder
                .numeric_indicator
                .runs()
                .continuation_chars()
                .filter_map(|c| ctx.character_definitions().get(c).cloned())
                .collect();
            builder
                .backward_numeric_constrainer
                .continuation_dots(continuation_dots);
        }
        Ok(builder.build(direction, ctx))
    }

    fn update_offsets(
        &self,
        translations: Vec<ResolvedTranslation>,
        decrement: usize,
    ) -> Vec<ResolvedTranslation> {
        translations
            .into_iter()
            // drop translations where the offet is smaller than the decrement
            .filter(|t| t.offset() >= decrement)
            .map(|t| t.decrement_offset(decrement))
            .collect()
    }

    pub fn translate(&self, input: &str) -> String {
        self.trace(input, &TranslationOptions::default())
            .iter()
            .map(|t| t.output())
            .collect()
    }

    pub fn translate_with_options(&self, input: &str, options: &TranslationOptions) -> String {
        self.trace(input, options)
            .iter()
            .map(|t| t.output())
            .collect()
    }

    fn translation_candidates(
        &self,
        input: &str,
        prev: Option<char>,
        word_ends: &[bool],
    ) -> (Vec<ResolvedTranslation>, Vec<ResolvedTranslation>) {
        self.trie
            .find_translations_with_word_ends(input, prev, word_ends)
            .into_iter()
            .partition(|t| t.offset() == 0)
    }

    /// A nocross candidate is valid only if it doesn't cross an internal
    /// hyphenation break of the word it's part of, mirroring liblouis's
    /// `syllableBreak`. `char_pos` is the candidate's starting position in
    /// the whole input, needed to look up the precomputed breaks.
    fn nocross_candidates(
        &self,
        input: &str,
        prev: Option<char>,
        word_ends: &[bool],
        char_pos: usize,
        constraints: &PositionConstraints,
    ) -> Vec<ResolvedTranslation> {
        self.nocross_trie
            .find_translations_with_word_ends(input, prev, word_ends)
            .into_iter()
            .filter(|t| {
                !(char_pos..char_pos + t.length().saturating_sub(1))
                    .any(|pos| constraints.hyphenation_break_at(pos))
            })
            .collect()
    }

    fn match_candidates(
        &self,
        input: &str,
        at_start: bool,
    ) -> (Vec<ResolvedTranslation>, Vec<ResolvedTranslation>) {
        self.match_patterns
            .find(input, at_start)
            .into_iter()
            .partition(|t| t.offset() == 0)
    }

    fn context_candidates(
        &self,
        input: &str,
        env: &Environment,
        at_start: bool,
    ) -> (Vec<ResolvedTranslation>, Vec<ResolvedTranslation>) {
        self.context_patterns
            .find(input, env, at_start)
            .into_iter()
            .partition(|t| t.offset() == 0)
    }

    /// liblouis matches a `match` rule's `pre` pattern backwards from the rule's position, so
    /// it may begin inside the `len` characters at the start of `consumed` that a translation
    /// just used up, positions the loop never visits. Returns those candidates, with offsets
    /// relative to the position after them.
    fn match_candidates_within(&self, consumed: &str, len: usize) -> Vec<ResolvedTranslation> {
        consumed
            .char_indices()
            .enumerate()
            .take(len)
            .skip(1)
            .flat_map(|(i, (byte_pos, _))| {
                self.match_patterns
                    .find_with_pre(&consumed[byte_pos..])
                    .into_iter()
                    .filter(move |t| t.offset() >= len - i)
                    .map(move |t| t.decrement_offset(len - i))
            })
            .collect()
    }

    fn partition_delayed_translations(
        &self,
        delayed: Vec<ResolvedTranslation>,
    ) -> (Vec<ResolvedTranslation>, Vec<ResolvedTranslation>) {
        delayed.into_iter().partition(|t| t.offset() == 0)
    }

    pub fn trace(&self, input: &str, options: &TranslationOptions) -> Vec<ResolvedTranslation> {
        let mut translations: Vec<ResolvedTranslation> = Vec::new();
        let mut delayed_translations: Vec<ResolvedTranslation> = Vec::new();

        let mut env = Environment::new();
        let mut chars = input.chars();
        let mut prev: Option<char> = None;
        let mut seen: HashSet<TranslationSubset> = HashSet::default();
        let mut char_pos: usize = 0;
        // A zero-length candidate (e.g. a zero-width `context`/`match` action) re-enters
        // the loop at the same char_pos without advancing, so indications must only be
        // emitted the first time a given position is visited, not on every re-entry.
        let mut last_indications_pos: Option<usize> = None;
        let mut no_contract = NoContractMode::Off;

        // Enrich spans with compbrl-detected regions.
        let enriched_spans = match &self.compbrl_scanner {
            Some(scanner) => scanner.enrich(input, options.emphasis()),
            None => options.emphasis().to_vec(),
        };

        let mut indications = self.indicators.precompute(input, &enriched_spans);
        let constraints = self.constrainers.precompute(input, &enriched_spans);
        let word_ends: Vec<bool> = match self.direction {
            Direction::Forward => Vec::new(),
            Direction::Backward => (0..=input.chars().count())
                .map(|pos| constraints.end_word_at(pos))
                .collect(),
        };
        let input_chars: Vec<char> = match self.sequences {
            Some(_) => input.chars().collect(),
            None => Vec::new(),
        };
        let stands_alone = |t: &ResolvedTranslation, pos: usize| match &self.sequences {
            Some(sequences)
                if matches!(t.rule(), Some(Rule::Word { .. } | Rule::Contraction { .. })) =>
            {
                sequences.in_sequence(&input_chars, pos, t.length())
            }
            _ => true,
        };

        loop {
            if last_indications_pos != Some(char_pos) {
                translations.extend(indications.translations_at(char_pos));
                last_indications_pos = Some(char_pos);
            }

            // whether we're about to match starting at the true beginning of the whole input,
            // needed by match/context rules anchored with liblouis' `^`/`` ` `` boundary markers
            let at_start = chars.as_str().len() == input.len();

            let before = match self.direction {
                Direction::Forward => prev,
                Direction::Backward => translations
                    .iter()
                    .rev()
                    .find_map(|t| t.output().chars().last()),
            };

            // First check for nocross candidates
            // the checks of liblouis's `for_selectRule` that depend on indication state
            let dont_contract = constraints.dont_contract_at(char_pos);
            let selectable = |t: &ResolvedTranslation| match t.rule() {
                Some(rule) => {
                    !(dont_contract && is_contraction(rule)
                        || checks_caps_change(rule)
                            && indications.caps_change_within(char_pos, t.length()))
                }
                None => true,
            };

            let nocross_candidate = self
                .nocross_candidates(
                    chars.as_str(),
                    before,
                    word_ends.get(char_pos..).unwrap_or_default(),
                    char_pos,
                    &constraints,
                )
                .into_iter()
                .filter(|t| stands_alone(t, char_pos))
                .filter(selectable)
                .max_by_key(|t| t.rank());

            // given an input query the trie for matching translations. Then split off the
            // translations that are delayed, i.e. have an offset because they have a pre-pattern
            let (mut candidates, delayed) = self.translation_candidates(
                chars.as_str(),
                before,
                word_ends.get(char_pos..).unwrap_or_default(),
            );
            delayed_translations.extend(delayed);

            // then search for matching match patterns. Unless they have empty pre patterns they will all have
            // an offset. Split those off.
            let (match_candidates, match_delayed) = self.match_candidates(chars.as_str(), at_start);
            delayed_translations.extend(match_delayed);
            // merge the candidates from the match patters with the candidates from the plain translations
            candidates.extend(match_candidates);

            // then search for context patterns. Unless they have empty pre patterns they will all
            // have an offset. Split those off.
            let (context_candidates, context_delayed) =
                self.context_candidates(chars.as_str(), &env, at_start);
            delayed_translations.extend(context_delayed);
            // merge the candidates from the context patters with the candidates from the plain translations
            candidates.extend(context_candidates);

            // move delayed_translations with zero offset into candidates
            let (current, delayed) = self.partition_delayed_translations(delayed_translations);
            delayed_translations = delayed;
            candidates.extend(current);

            // `prepunc`/`postpunc` need a variable-length scan the trie can't express
            // (see `PunctuationConstrainer`), so they're inserted with no trie-level
            // boundary and gated here instead. Rejected candidates are simply dropped,
            // leaving the position to fall through to the plain punctuation character
            // definition (`character_translations`), matching liblouis.
            candidates.retain(|t| match t.rule() {
                Some(Rule::Prepunc { .. }) => match self.direction {
                    Direction::Forward => constraints.prepunc_ok_at(char_pos),
                    Direction::Backward => self.backward_begins_word(&translations),
                },
                Some(Rule::Postpunc { .. }) => {
                    self.direction == Direction::Backward || constraints.postpunc_ok_at(char_pos)
                }
                _ => stands_alone(t, char_pos),
            });

            // Backward numeric mode: prefer the digit reading of a cell shared with a
            // letter (common in six-dot literary codes). Rather than out-weighing the
            // trie's single-cell letter candidate, replace it outright — any genuinely
            // longer (multi-cell) candidate at this position is left alone to compete
            // on its own weight, same as usual.
            if self.direction == Direction::Backward
                && constraints.prefer_digit_at(char_pos)
                && let Some(next_char) = chars.clone().next()
                && let Some(digit_translation) = self.backward_digit_overrides.get(&next_char)
            {
                candidates.retain(|t| t.length() != 1);
                candidates.push(digit_translation.clone());
            }

            candidates.retain(selectable);

            // Backward no-contract mode: liblouis suppresses `begword` while it is active
            // and `word`/`sufword`/`prfword` until it is off again, and leaves `always`,
            // `partword` and the mid-word opcodes eligible.
            if no_contract != NoContractMode::Off {
                candidates.retain(|t| match t.rule() {
                    Some(Rule::Word { .. } | Rule::Sufword { .. } | Rule::Prfword { .. }) => false,
                    Some(Rule::Begword { .. }) => no_contract != NoContractMode::Active,
                    _ => true,
                });
            }

            // In computer braille mode, discard normal candidates and use comp6 rules only.
            // comp6 rules have no pre-pattern so offset() is always 0 — no delayed handling needed.
            if constraints.use_comp6_at(char_pos) {
                candidates.clear();
                candidates.extend(self.comp6_trie.find_translations_with_word_ends(
                    chars.as_str(),
                    before,
                    word_ends.get(char_pos..).unwrap_or_default(),
                ));
            }

            // Backward caps mode: capitalize the output of whichever candidate wins below.
            // `Uppercase` (begcapsword span) capitalizes every character a cell resolves
            // to; `TitleCase` (capsletter) capitalizes only the first — a cell can
            // resolve to more than one character (e.g. a digraph rule), and capsletter
            // marks a single capitalized letter, not a whole capitalized word.
            let is_backward = self.direction == Direction::Backward;
            let uppercase_here = is_backward && constraints.uppercase_at(char_pos);
            let titlecase_here = is_backward && constraints.titlecase_at(char_pos);
            let uppercase_if_needed =
                move |translation: ResolvedTranslation| -> ResolvedTranslation {
                    if uppercase_here {
                        let upper = translation.output().to_uppercase();
                        translation.with_output(&upper)
                    } else if titlecase_here {
                        let output = translation.output();
                        let mut chars = output.chars();
                        let titled = match chars.next() {
                            Some(c) => c.to_uppercase().chain(chars).collect::<String>(),
                            None => String::new(),
                        };
                        translation.with_output(&titled)
                    } else {
                        translation
                    }
                };

            // use the longest translation
            let candidate = candidates
                .iter()
                // drop translation candidates that we have applied already at this position in the
                // input
                .filter(|t| !seen.contains(&TranslationSubset::from(*t)))
                .max_by_key(|translation| (translation.rank(), self.backward_order(translation)));
            // A nocross rule wins if there is no plain candidate to beat (many, e.g.
            // liblouis' `nocross always en`, have no plain counterpart at all).
            if let Some(nocross) = &nocross_candidate
                && candidate.is_none_or(|t| nocross.rank() >= t.rank())
            {
                let translation = nocross.clone();
                let translation = uppercase_if_needed(translation);
                let consumed = chars.as_str();
                // move the iterator forward by the number of characters in the translation
                chars.nth(nocross.length() - 1);
                prev = translation.input().chars().last();
                char_pos += nocross.length();
                no_contract = no_contract.after(&translation);
                translations.push(translation);
                delayed_translations = self.update_offsets(delayed_translations, nocross.length());
                delayed_translations
                    .extend(self.match_candidates_within(consumed, nocross.length()));
            } else if let Some(t) = candidate {
                let consumed = chars.as_str();
                if t.length() == 0 {
                    // if there is a zero-length translation candiate we run the risk of an infinite
                    // loop, so remember the current translation so we only apply it once
                    seen.insert(TranslationSubset::from(t));
                } else {
                    seen.clear();
                    // move the iterator forward by the number of characters in the translation
                    chars.nth(t.length() - 1);
                    prev = t.input().chars().last();
                    char_pos += t.length();
                }
                // there is a matching translation rule
                let translation = t.clone();
                let translation = uppercase_if_needed(translation);
                if self.direction == Direction::Backward {
                    no_contract = no_contract.after(&translation);
                }
                translations.push(translation);
                delayed_translations = self.update_offsets(delayed_translations, t.length());
                delayed_translations.extend(self.match_candidates_within(consumed, t.length()));

                // update the environment if needed
                if !t.effects().is_empty() {
                    for effect in t.effects() {
                        env.apply(effect);
                    }
                }
            } else if let Some(next_char) = chars.next() {
                prev = Some(next_char);
                char_pos += 1;
                // no translation rule found
                if let Some(translation) = self.character_translations.get(&next_char) {
                    translations.push(translation.clone());
                    delayed_translations = self.update_offsets(delayed_translations, 1);
                } else if let Some(ref replacement) = self.undefined {
                    // there is a rule for undefined characters
                    let translation = ResolvedTranslation::new(
                        &next_char.to_string(),
                        replacement,
                        1,
                        TranslationStage::Main,
                        None, // FIXME: add the undefined rule here
                    );
                    translations.push(translation);
                    delayed_translations = self.update_offsets(delayed_translations, 1);
                } else {
                    // otherwise handle it as a undefined character
                    let translation = ResolvedTranslation::new(
                        &next_char.to_string(),
                        &self.handle_undefined_char(next_char),
                        1,
                        TranslationStage::Main,
                        None,
                    );
                    translations.push(translation);
                    delayed_translations = self.update_offsets(delayed_translations, 1);
                }
            } else {
                // the chars iterator is exhausted — slot `n` translations were
                // already emitted by the translations_at call at the top of this
                // iteration, so just break here.
                break;
            }
        }
        translations
    }

    fn handle_undefined_char(&self, ch: char) -> String {
        ch.escape_unicode()
            .to_string()
            .replace(r"\u", r"\x") // replace \u by \x
            .replace(['{', '}'], "") // drop the curly braces
            .chars()
            .map(|c| {
                if let Some(t) = self.fallback_definitions.get(&c) {
                    t.clone()
                } else {
                    fallback(c).to_string()
                }
            })
            .collect()
    }
}

/// The opcodes liblouis's `for_selectRule` rejects when an indication falls inside them
/// (`checkEmphasisChange`)
fn checks_caps_change(rule: &Rule) -> bool {
    matches!(
        rule,
        Rule::Always { .. } | Rule::Syllable { .. } | Rule::Word { .. } | Rule::Match { .. }
    )
}

/// The opcodes liblouis's `for_selectRule` skips while `dontContract` is set
fn is_contraction(rule: &Rule) -> bool {
    matches!(
        rule,
        Rule::Repeated { .. }
            | Rule::Repword { .. }
            | Rule::Rependword { .. }
            | Rule::Nocont { .. }
            | Rule::Always { .. }
            | Rule::Largesign { .. }
            | Rule::Word { .. }
            | Rule::Partword { .. }
            | Rule::Joinnum { .. }
            | Rule::Lowword { .. }
            | Rule::Joinword { .. }
            | Rule::Sufword { .. }
            | Rule::Prfword { .. }
            | Rule::Begword { .. }
            | Rule::Begmidword { .. }
            | Rule::Midword { .. }
            | Rule::Midendword { .. }
            | Rule::Endword { .. }
            | Rule::Match { .. }
    )
}

/// liblouis's backward `noContractMode`: set by a `letsign`, `nonumsign` or
/// `nocontractsign`, lapsed by anything but a letter, and reset by a space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoContractMode {
    Off,
    Active,
    Lapsed,
}

impl NoContractMode {
    fn after(self, translation: &ResolvedTranslation) -> Self {
        match translation.rule() {
            Some(Rule::Letsign { .. } | Rule::Nonumsign { .. } | Rule::Nocontractsign { .. }) => {
                NoContractMode::Active
            }
            Some(Rule::Multind { names, .. })
                if names.iter().any(|name| {
                    matches!(name.as_str(), "letsign" | "nonumsign" | "nocontractsign")
                }) =>
            {
                NoContractMode::Active
            }
            Some(Rule::Space { .. }) => NoContractMode::Off,
            Some(Rule::Letter { .. } | Rule::Lowercase { .. } | Rule::Uppercase { .. }) => self,
            _ if translation.output().is_empty() => self,
            _ if self == NoContractMode::Active => NoContractMode::Lapsed,
            _ => self,
        }
    }
}

/// Map a char to dots according to the North American Braille Computer Code (NABCC)
///
/// A fallback mapping for character to braille in case the table does
/// not provide a mapping. This is used as a last resort when printing
/// unicode escapes for undefined characters when the table does not
/// define the character mappings that are needed. Only the characters
/// that [`PrimaryTable::handle_undefined_char`] can produce are mapped.
fn fallback(ch: char) -> BrailleChars {
    let dots = match ch {
        '0' => "356",
        '1' => "2",
        '2' => "23",
        '3' => "25",
        '4' => "256",
        '5' => "26",
        '6' => "235",
        '7' => "2356",
        '8' => "236",
        '9' => "35",
        'a' => "1",
        'b' => "12",
        'c' => "14",
        'd' => "145",
        'e' => "15",
        'f' => "124",
        '\\' => "1256",
        'x' => "1346",
        _ => unreachable!("no NABCC fallback for {ch:?}"),
    };
    BrailleChars::try_from(dots).expect("NABCC dot pattern is valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    use search_path::SearchPath;

    use crate::parser::{RuleParser, expand_includes};

    #[test]
    fn fallback_covers_every_escaped_char() {
        // handle_undefined_char emits `\x` plus lowercase hex digits, so
        // fallback must map all of them without hitting the unreachable arm
        for c in r"\x0123456789abcdef".chars() {
            assert_eq!(fallback(c).len(), 1);
        }
        assert_eq!(fallback('a').to_string(), "\u{2801}"); // dot 1
        assert_eq!(fallback('9').to_string(), "\u{2814}"); // dots 35, not 25
    }

    fn parse_rule(source: &str) -> AnchoredRule {
        RuleParser::new(source).rule().unwrap().into()
    }

    /// A [`SearchPath`] rooted at the crate directory, for tests that `include` one of the
    /// fixtures under `dictionaries/`.
    fn fixture_search_path() -> SearchPath {
        SearchPath::from(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")))
    }

    #[test]
    fn translate() {
        let rules = [
            parse_rule("always foo 123"),
            parse_rule("always bar 456"),
            parse_rule("space \\s 0"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("foobar"), "⠇⠸");
        assert_eq!(table.translate("  "), "⠀⠀");
        assert_eq!(table.translate("🐂"), "⠳⠭⠂⠋⠲⠴⠆");
    }

    #[test]
    fn midword() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 2"),
            parse_rule("lowercase f 3"),
            parse_rule("lowercase o 4"),
            parse_rule("lowercase r 5"),
            parse_rule("always foo 14"),
            parse_rule("midword bar 15"),
            parse_rule("space \\s 0"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("bar"), "⠂⠁⠐"); // should not contract
        assert_eq!(table.translate("foobar"), "⠉⠂⠁⠐"); // only foo should be contracted
        assert_eq!(table.translate("foobarfoo"), "⠉⠑⠉"); // foo and bar should be contracted
        assert_eq!(table.translate("foobar foo"), "⠉⠂⠁⠐⠀⠉"); // only foo should be contracted
        assert_eq!(table.translate("foo bar foo"), "⠉⠀⠂⠁⠐⠀⠉"); // only foo should be contracted
    }

    #[test]
    fn midword_with_precedence() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 2"),
            parse_rule("lowercase f 3"),
            parse_rule("lowercase o 4"),
            parse_rule("lowercase r 5"),
            parse_rule("always foo 14"),
            parse_rule("always bar 24"),
            parse_rule("midword bar 26"),
            parse_rule("space \\s 0"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("bar"), "⠊"); // bar should contract with 24
        assert_eq!(table.translate("foobar"), "⠉⠊"); // bar should contract with 24
        assert_eq!(table.translate("foobarfoo"), "⠉⠢⠉"); // bar should contract with 26
        assert_eq!(table.translate("foobar foo"), "⠉⠊⠀⠉"); // bar should contract with 24
        assert_eq!(table.translate("foo bar foo"), "⠉⠀⠊⠀⠉"); // bar should contract with 24
    }

    #[test]
    fn endword() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 2"),
            parse_rule("lowercase f 3"),
            parse_rule("lowercase o 4"),
            parse_rule("lowercase r 5"),
            parse_rule("punctuation . 6"),
            parse_rule("always foo 14"),
            parse_rule("endword bar 15"),
            parse_rule("space \\s 0"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        // liblouis's real endword requires a letter before the match (CTO_EndWord:
        // beforeAttributes & CTC_Letter), so "bar" alone (nothing precedes it) is
        // spelled out, not contracted.
        assert_eq!(table.translate("bar"), "⠂⠁⠐");
        assert_eq!(table.translate("foobar"), "⠉⠑"); // both should be contracted
        assert_eq!(table.translate("foobar."), "⠉⠑⠠"); // both should be contracted
        assert_eq!(table.translate("foobarfoo"), "⠉⠂⠁⠐⠉"); // only foo should be contracted
        assert_eq!(table.translate("foobar foo"), "⠉⠑⠀⠉"); // both should be contracted
        // "bar" is preceded by a space here, not a letter, so it's its own standalone
        // word rather than embedded in a larger one - endword requires a letter before
        // the match, so it's spelled out, not contracted.
        assert_eq!(table.translate("foo bar foo"), "⠉⠀⠂⠁⠐⠀⠉");
    }

    #[test]
    fn partword() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 2"),
            parse_rule("lowercase f 3"),
            parse_rule("lowercase o 4"),
            parse_rule("lowercase r 5"),
            parse_rule("punctuation . 6"),
            parse_rule("always foo 14"),
            parse_rule("partword bar 15"),
            parse_rule("space \\s 0"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        // partword fires when either side is a letter (not standalone)
        assert_eq!(table.translate("bar"), "⠂⠁⠐"); // standalone: NOT contracted
        assert_eq!(table.translate("foobar"), "⠉⠑"); // word-end (prev=letter): contracted
        assert_eq!(table.translate("foobar."), "⠉⠑⠠"); // word-end (prev=letter): contracted
        assert_eq!(table.translate("foobarfoo"), "⠉⠑⠉"); // midword: contracted
        assert_eq!(table.translate("foobar foo"), "⠉⠑⠀⠉"); // word-end: contracted
        assert_eq!(table.translate("foobar. foo"), "⠉⠑⠠⠀⠉"); // word-end: contracted
        assert_eq!(table.translate("foo bar foo"), "⠉⠀⠂⠁⠐⠀⠉"); // standalone: NOT contracted
    }

    #[test]
    fn sufword() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 2"),
            parse_rule("lowercase f 3"),
            parse_rule("lowercase o 4"),
            parse_rule("lowercase r 5"),
            parse_rule("punctuation . 6"),
            parse_rule("always foo 14"),
            parse_rule("sufword bar 15"),
            parse_rule("space \\s 0"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("bar"), "⠑"); // bar should be contracted
        assert_eq!(table.translate("foobar"), "⠉⠂⠁⠐"); // bar should not be contracted
        assert_eq!(table.translate("barfoo"), "⠑⠉"); // bar should be contracted
        assert_eq!(table.translate("foobar."), "⠉⠂⠁⠐⠠"); // bar should not be contracted
        assert_eq!(table.translate("foobarfoo"), "⠉⠂⠁⠐⠉"); // bar should not be contracted
        assert_eq!(table.translate("foobar foo"), "⠉⠂⠁⠐⠀⠉"); // bar should not be contracted
        assert_eq!(table.translate("foobar. foo"), "⠉⠂⠁⠐⠠⠀⠉"); // bar should not be contracted
        assert_eq!(table.translate("foo bar foo"), "⠉⠀⠑⠀⠉"); // bar should be contracted
    }

    #[test]
    fn word_in_sequence() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("punctuation ! 235"),
            parse_rule("punctuation ( 5-126"),
            parse_rule("punctuation ) 5-345"),
            parse_rule("punctuation - 36"),
            parse_rule("punctuation ' 3"),
            parse_rule("lowercase s 234"),
            parse_rule("space \\s 0"),
            parse_rule("seqdelimiter -"),
            parse_rule("seqbeforechars ("),
            parse_rule("seqafterchars )'"),
            parse_rule("seqafterpattern 's"),
            parse_rule("word ab 1456"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("ab"), "⠹");
        assert_eq!(table.translate("(ab)"), "⠐⠣⠹⠐⠜");
        assert_eq!(table.translate("-ab-"), "⠤⠹⠤");
        assert_eq!(table.translate("ab's"), "⠹⠄⠎");
        // punctuation that is not a sequence character keeps the word from standing alone
        assert_eq!(table.translate("!ab"), "⠖⠁⠃");
        assert_eq!(table.translate("ab("), "⠁⠃⠐⠣");
        assert_eq!(table.translate("ab'b"), "⠁⠃⠄⠃");
    }

    #[test]
    fn prfword() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 2"),
            parse_rule("lowercase f 3"),
            parse_rule("lowercase o 4"),
            parse_rule("lowercase r 5"),
            parse_rule("punctuation . 6"),
            parse_rule("always foo 14"),
            parse_rule("prfword bar 15"),
            parse_rule("space \\s 0"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("bar"), "⠑"); // bar should be contracted
        assert_eq!(table.translate("foobar"), "⠉⠑"); // bar should be contracted
        assert_eq!(table.translate("barfoo"), "⠂⠁⠐⠉"); // bar should be not contracted
        assert_eq!(table.translate("foobar."), "⠉⠑⠠"); // bar should be contracted
        assert_eq!(table.translate("foobarfoo"), "⠉⠂⠁⠐⠉"); // bar should not be contracted
        assert_eq!(table.translate("foobar foo"), "⠉⠑⠀⠉"); // bar should be contracted
        assert_eq!(table.translate("foobar. foo"), "⠉⠑⠠⠀⠉"); // bar should be contracted
        assert_eq!(table.translate("foo bar foo"), "⠉⠀⠑⠀⠉"); // bar should be contracted
    }

    #[test]
    fn begnum() {
        let rules = [
            parse_rule("digit 1 1"),
            parse_rule("lowercase a 3456"),
            parse_rule("begnum a 4"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("1"), "⠁");
        assert_eq!(table.translate("a"), "⠼");
        assert_eq!(table.translate("a1"), "⠈⠁");
    }

    #[test]
    fn endnum() {
        let rules = [
            parse_rule("digit 1 1"),
            parse_rule("lowercase h 125"),
            parse_rule("lowercase t 2345"),
            parse_rule("punctuation . 6"),
            parse_rule("always th 14"),
            parse_rule("endnum th 15"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("th"), "⠉");
        assert_eq!(table.translate("1th"), "⠁⠑");
        assert_eq!(table.translate("1th."), "⠁⠑⠠");
    }

    #[test]
    fn begnum_punctuation_indicator() {
        // '#' is punctuation, not a word char. The old Start(Word) boundary would have
        // missed it; Start(AfterSpaceOrPunct) correctly fires when preceded by
        // punctuation or at start-of-string.
        let rules = [
            parse_rule("digit 1 1"),
            parse_rule("punctuation # 3456"),
            parse_rule("punctuation . 256"),
            parse_rule("lowercase a 14"),
            parse_rule("begnum # 4"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        // fires at start of string (prev = None)
        assert_eq!(table.translate("#1"), "⠈⠁");
        // fires when preceded by punctuation
        assert_eq!(table.translate(".#1"), "⠲⠈⠁");
        // does NOT fire when preceded by a letter
        assert_eq!(table.translate("a#1"), "⠉⠼⠁");
        // does NOT fire when not followed by a digit
        assert_eq!(table.translate("#a"), "⠼⠉");
    }

    #[test]
    fn endnum_punctuation_indicator() {
        // '#' is punctuation. End(Number) only checks that the preceding char is a
        // digit; there is no constraint on what follows (unlike the old End(Word)).
        let rules = [
            parse_rule("digit 1 1"),
            parse_rule("lowercase a 14"),
            parse_rule("punctuation # 3456"),
            parse_rule("endnum # 4"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        // fires when preceded by a digit
        assert_eq!(table.translate("1#"), "⠁⠈");
        // does NOT fire when preceded by a letter
        assert_eq!(table.translate("a#"), "⠉⠼");
        // does NOT fire at start of string
        assert_eq!(table.translate("#"), "⠼");
        // no constraint on what follows the indicator
        assert_eq!(table.translate("1#1"), "⠁⠈⠁");
        assert_eq!(table.translate("1#a"), "⠁⠈⠉");
    }

    #[test]
    fn prepunc_postpunc() {
        let rules = [
            parse_rule("lowercase f 3"),
            parse_rule("lowercase o 4"),
            parse_rule("punctuation ( 23678"),
            parse_rule("prepunc     ( 2356"),
            parse_rule("punctuation ) 35678"),
            parse_rule("postpunc    ) 2356"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate(")"), "⣴");
        assert_eq!(table.translate("("), "⣦");
        assert_eq!(table.translate("()"), "⣦⣴");
        assert_eq!(table.translate("foo)"), "⠄⠈⠈⠶");
        assert_eq!(table.translate("(foo"), "⠶⠄⠈⠈");
        assert_eq!(table.translate("(foo)"), "⠶⠄⠈⠈⠶");
    }

    #[test]
    fn lowword_needs_space_on_both_sides() {
        let rules = [
            parse_rule("lowercase t 2345"),
            parse_rule("lowercase e 15"),
            parse_rule("punctuation . 256"),
            parse_rule("space \\s 0"),
            parse_rule("lowword te 235"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let forward =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(forward.translate("te"), "⠖");
        assert_eq!(forward.translate(" te "), "⠀⠖⠀");
        assert_eq!(forward.translate("te."), "⠞⠑⠲");
        let backward = backward_table(&rules);
        assert_eq!(backward.translate("⠖"), "te");
    }

    #[test]
    fn base() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("base uppercase A a"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("a"), "⠁");
        assert_eq!(table.translate("A"), "⠁");
        assert_eq!(table.translate("ab"), "⠁⠃");
        assert_eq!(table.translate("Ab"), "⠁⠃");
    }

    #[test]
    fn match_simple() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 2"),
            parse_rule("lowercase f 3"),
            parse_rule("lowercase o 4"),
            parse_rule("lowercase r 5"),
            parse_rule("match a foo b 14"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("foo"), "⠄⠈⠈");
        assert_eq!(table.translate("afoo"), "⠁⠄⠈⠈");
        assert_eq!(table.translate("afoob"), "⠁⠉⠂");
    }

    #[test]
    fn match_with_any() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 2"),
            parse_rule("lowercase f 3"),
            parse_rule("lowercase o 4"),
            parse_rule("lowercase r 5"),
            parse_rule("match . foo b 14"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("foo"), "⠄⠈⠈");
        assert_eq!(table.translate("afoo"), "⠁⠄⠈⠈");
        assert_eq!(table.translate("afoob"), "⠁⠉⠂");
        assert_eq!(table.translate("ffoob"), "⠄⠉⠂");
    }

    #[test]
    fn match_pre_inside_consumed_translation() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 2"),
            parse_rule("punctuation . 256"),
            parse_rule("always ab 12"),
            parse_rule("match b . - 3"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        // the pre pattern starts inside "ab", which the loop never visits
        assert_eq!(table.translate("ab."), "⠃⠄");
        assert_eq!(table.translate("b."), "⠂⠄");
        assert_eq!(table.translate("a."), "⠁⠲");
    }

    #[test]
    fn match_negated_attribute_or_boundary() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 2"),
            parse_rule("space \\s 0"),
            parse_rule("match !%[^_] b - 3"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("ab"), "⠁⠄");
        assert_eq!(table.translate("b"), "⠂");
        assert_eq!(table.translate("a b"), "⠁⠀⠂");
    }

    #[test]
    fn match_with_set() {
        let rules = [
            parse_rule("lowercase f 3"),
            parse_rule("lowercase o 4"),
            parse_rule("lowercase s 7"),
            parse_rule("lowercase z 5"),
            parse_rule("match [fz] oo [fz] 14"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("oo"), "⠈⠈");
        assert_eq!(table.translate("soo"), "⡀⠈⠈");
        assert_eq!(table.translate("foof"), "⠄⠉⠄");
        assert_eq!(table.translate("zoof"), "⠐⠉⠄");
        assert_eq!(table.translate("soof"), "⡀⠈⠈⠄");
    }

    #[test]
    fn numeric_indication() {
        let rules = [
            parse_rule("lowercase f 3"),
            parse_rule("lowercase o 4"),
            parse_rule("litdigit 1 1"),
            parse_rule("litdigit 2 12"),
            parse_rule("litdigit 3 14"),
            parse_rule("numsign 3456"),
            parse_rule("nonumsign 56"),
            parse_rule("numericnocontchars abcdefghij"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("123"), "⠼⠁⠃⠉");
        assert_eq!(table.translate("123foo"), "⠼⠁⠃⠉⠰⠄⠈⠈");
        assert_eq!(table.translate("foof"), "⠄⠈⠈⠄");
    }

    #[test]
    fn number_suppresses_contractions_but_not_number_rules() {
        let rules = [
            parse_rule("lowercase f 3"),
            parse_rule("lowercase o 4"),
            parse_rule("punctuation - 36"),
            parse_rule("digit 1 1"),
            parse_rule("litdigit 1 1"),
            parse_rule("numsign 3456"),
            parse_rule("nonumsign 56"),
            parse_rule("nocontractsign 56"),
            parse_rule("numericnocontchars abcdefghij"),
            parse_rule("midendnumericmodechars -"),
            parse_rule("always oo 1246"),
            parse_rule("endnum -fo 36-2"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("oo"), "⠫");
        assert_eq!(table.translate("1oo"), "⠼⠁⠈⠈");
        assert_eq!(table.translate("1-foo"), "⠼⠁⠤⠂⠈");
        assert_eq!(table.translate("1-1"), "⠼⠁⠤⠁");
    }

    #[test]
    fn contraction_not_across_caps_indication() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("lowercase c 14"),
            parse_rule("base uppercase A a"),
            parse_rule("base uppercase B b"),
            parse_rule("base uppercase C c"),
            parse_rule("capsletter 46"),
            parse_rule("begcapsword 6-6"),
            parse_rule("always bc 2345"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("abc"), "⠁⠞");
        assert_eq!(table.translate("aBC"), "⠁⠠⠠⠞");
        assert_eq!(table.translate("abC"), "⠁⠃⠨⠉");
    }

    #[test]
    fn uppercase_indication() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("lowercase c 14"),
            parse_rule("base uppercase A a"),
            parse_rule("base uppercase B b"),
            parse_rule("base uppercase C c"),
            parse_rule("capsletter 46"),
            parse_rule("begcapsword 6-6"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("abc"), "⠁⠃⠉");
        assert_eq!(table.translate("Abc"), "⠨⠁⠃⠉");
        assert_eq!(table.translate("ABC"), "⠠⠠⠁⠃⠉");
    }

    #[test]
    fn lettersign_indication() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("lowercase c 14"),
            parse_rule("lowercase d 145"),
            parse_rule("lowercase e 15"),
            parse_rule("lowercase f 124"),
            parse_rule("lowercase o 135"),
            parse_rule("lowercase u 136"),
            parse_rule("lowercase t 2345"),
            parse_rule("letsign 6"),
            parse_rule("word about 1-12"),
            parse_rule("contraction ab"),
            parse_rule("contraction cd"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("about"), "⠁⠃");
        assert_eq!(table.translate("ab"), "⠠⠁⠃");
        assert_eq!(table.translate("cd"), "⠠⠉⠙");
        assert_eq!(table.translate("abcd"), "⠁⠃⠉⠙");
        assert_eq!(table.translate("ef"), "⠑⠋");
    }

    #[test]
    fn nocontractsign_indication() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("lowercase c 14"),
            parse_rule("lowercase d 145"),
            parse_rule("lowercase e 15"),
            parse_rule("lowercase f 124"),
            parse_rule("lowercase o 135"),
            parse_rule("lowercase u 136"),
            parse_rule("lowercase t 2345"),
            parse_rule("nocontractsign 6"),
            parse_rule("word about 1-12"),
            parse_rule("contraction ab"),
            parse_rule("contraction cd"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("about"), "⠁⠃");
        assert_eq!(table.translate("ab"), "⠠⠁⠃");
        assert_eq!(table.translate("cd"), "⠠⠉⠙");
        assert_eq!(table.translate("abcd"), "⠁⠃⠉⠙");
        assert_eq!(table.translate("ef"), "⠑⠋");
    }

    #[test]
    fn nocross() {
        let rules = vec![
            parse_rule("include dictionaries/nocross-test.dic"),
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("lowercase h 125"),
            parse_rule("lowercase o 135"),
            parse_rule("lowercase s 234"),
            parse_rule("lowercase t 2345"),
            parse_rule("lowercase u 136"),
            parse_rule("always hausboot 123"),
            parse_rule("nocross always hausboot 456"),
            parse_rule("always fff 123"),
            parse_rule("nocross always fff 456"),
            parse_rule("space \\s 0"),
        ];
        let rules = expand_includes(rules, &fixture_search_path(), &[]).unwrap();
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("hausboot"), "⠇");
        assert_eq!(table.translate("fff"), "⠸");
    }

    #[test]
    fn nocross_without_plain_counterpart() {
        // liblouis tables routinely define a `nocross` rule with no plain counterpart
        // (e.g. da-dk-g28.ctb's `nocross always en`) — it must still win when it's the
        // only candidate at a position, not just when it out-weighs a competing plain rule.
        let rules = vec![
            parse_rule("include dictionaries/nocross-test.dic"),
            parse_rule("lowercase f 124"),
            parse_rule("nocross always fff 456"),
            parse_rule("space \\s 0"),
        ];
        let rules = expand_includes(rules, &fixture_search_path(), &[]).unwrap();
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("fff"), "⠸");
    }

    #[test]
    fn nocross_precedence_follows_word_hyphenation() {
        // da-dk-g28.ctb defines `nocross always er` and `nocross partword re` with no
        // plain counterparts. For the word "ere" the real hyphenation break falls
        // between 'e' and 'r' ("e-re"), which disqualifies `er` (it would cross that
        // break) but not `re` (fully inside the second syllable) -- so `re` must win,
        // even though `er` starts earlier in the word.
        let rules = vec![
            parse_rule("include dictionaries/nocross-precedence-test.dic"),
            parse_rule("lowercase e 1"),
            parse_rule("lowercase r 12"),
            parse_rule("nocross always er 6"),
            parse_rule("nocross partword re 56"),
        ];
        let rules = expand_includes(rules, &fixture_search_path(), &[]).unwrap();
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        // literal 'e' (⠁) + the `re` contraction (⠰), not the `er` contraction (⠠) + 'e'
        assert_eq!(table.translate("ere"), "⠁⠰");
    }

    #[test]
    fn backtranslation() {
        let rules = [
            parse_rule("lowercase f 3"),
            parse_rule("lowercase o 4"),
            parse_rule("always foo 123"),
            parse_rule("always bar 456"),
            parse_rule("space \\s 0"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table = PrimaryTable::compile(
            &rules,
            Direction::Backward,
            TranslationStage::Main,
            &context,
        )
        .unwrap();
        assert_eq!(table.translate("⠇⠸"), "foobar");
        assert_eq!(table.translate("⠀⠀"), "  ");
        assert_eq!(table.translate("⠄⠈⠈"), "foo");
    }

    #[test]
    fn indication_not_reemitted_at_zero_length_candidate() {
        // A zero-length `context` action (e.g. liblouis' `$d[]$l`, used to insert a
        // letsign between a digit and a single trailing letter) legitimately overlaps
        // with the letsign indicator that also fires there, producing two signs — a
        // table author relies on a later pass rule to collapse that expected overlap
        // (see da-dk-g28.ctb's "Remove any superfluous letsigns" pass3 rule). What must
        // not happen is a *third* sign: the translation loop revisits this position
        // without advancing (the context action is zero-length), and the indicator
        // must not be re-emitted on that second visit.
        let rules = [
            parse_rule("digit 0 1"),
            parse_rule("lowercase i 24"),
            parse_rule("letsign 6"),
            parse_rule("context $d[]$l @6"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("0i"), "⠁⠠⠠⠊");
    }

    #[test]
    fn context_with_variable() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("sign : 123"),
            parse_rule("sign ; 456"),
            parse_rule("context \":\" @123#1=1"),
            parse_rule("context #1=1\"a\" @3#1=1"),
            parse_rule("context \";\" @456#1=0"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("aa"), "⠁⠁");
        assert_eq!(table.translate("aa;aa"), "⠁⠁⠸⠁⠁");
        assert_eq!(table.translate("aa:aa;aa"), "⠁⠁⠇⠄⠄⠸⠁⠁");
    }

    #[test]
    fn always_with_uppercase() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("lowercase c 14"),
            parse_rule("base uppercase A a"),
            parse_rule("base uppercase B b"),
            parse_rule("base uppercase C c"),
            parse_rule("capsletter 46"),
            parse_rule("begcapsword 6-6"),
            parse_rule("always abc 78"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("abc"), "⣀");
        assert_eq!(table.translate("Abc"), "⠨⣀");
        assert_eq!(table.translate("ABC"), "⠠⠠⣀");
    }

    #[test]
    fn backward_capsletter_beats_colliding_punctuation_and_titlecases_next_letter() {
        // `capsletter` and `punctuation $` share dots 46 here, mirroring hu-hu-g1.ctb's
        // real collision (GitHub issue #4). capsletter must win and title-case the
        // following letter, not fall through to the punctuation definition.
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("capsletter 46"),
            parse_rule("punctuation $ 46"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table = PrimaryTable::compile(
            &rules,
            Direction::Backward,
            TranslationStage::Main,
            &context,
        )
        .unwrap();
        assert_eq!(table.translate("⠨⠃"), "B");
    }

    #[test]
    fn backward_numsign_prefers_digit_reading_of_shared_dots() {
        // `litdigit 1` shares dots with `lowercase a` (as in six-dot literary codes,
        // where digits conventionally reuse letter cells). Inside a numsign span the
        // digit reading must win; outside it, the letter reading is unaffected.
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("litdigit 1 1"),
            parse_rule("numsign 3456"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table = PrimaryTable::compile(
            &rules,
            Direction::Backward,
            TranslationStage::Main,
            &context,
        )
        .unwrap();
        assert_eq!(table.translate("⠼⠁"), "1");
        assert_eq!(table.translate("⠁"), "a");
    }

    #[test]
    fn backward_begcapsword_uppercases_whole_word() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("begcapsword 46-46"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table = PrimaryTable::compile(
            &rules,
            Direction::Backward,
            TranslationStage::Main,
            &context,
        )
        .unwrap();
        assert_eq!(table.translate("⠨⠨⠁⠃"), "AB");
    }

    #[test]
    fn backward_capsletter_does_not_shadow_a_real_letter_sharing_its_dots() {
        // Regression: some tables (e.g. Malayalam, Punjabi) include a shared subtable
        // (en-in-g1.ctb) purely for other opcodes, whose capsletter/numsign happen to
        // share dots with one of the table's own script letters (Malayalam "ഃ" and
        // capsletter both use dots 6). Unlike the `$`/capsletter collision, the letter
        // must win here — capsletter should not be recognized as an indicator at all
        // for dots already claimed by a real letter.
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("letter b 6"),
            parse_rule("capsletter 6"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table = PrimaryTable::compile(
            &rules,
            Direction::Backward,
            TranslationStage::Main,
            &context,
        )
        .unwrap();
        assert_eq!(table.translate("⠠⠁"), "ba");
    }

    fn backward_table(rules: &[AnchoredRule]) -> PrimaryTable {
        let context = TableContext::compile(rules).unwrap();
        PrimaryTable::compile(rules, Direction::Backward, TranslationStage::Main, &context).unwrap()
    }

    #[test]
    fn backward_word_start_is_judged_by_the_output() {
        // `midword aa 2` shares its cell with the comma. The cell before `b` has been
        // translated to letters, so `word liefde 123` must not fire there.
        let table = backward_table(&[
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("lowercase l 123"),
            parse_rule("punctuation , 2"),
            parse_rule("space \\s 0"),
            parse_rule("midword aa 2"),
            parse_rule("word liefde 123"),
        ]);
        assert_eq!(table.translate("⠃⠂⠇"), "baal");
        assert_eq!(table.translate("⠇"), "liefde");
        assert_eq!(table.translate("⠂⠀⠇"), ", liefde");
    }

    #[test]
    fn backward_word_end_scans_past_punctuation_to_a_letter() {
        let table = backward_table(&[
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("punctuation , 2"),
            parse_rule("space \\s 0"),
            parse_rule("word baie 12"),
        ]);
        assert_eq!(table.translate("⠃⠂⠁"), "b,a");
        assert_eq!(table.translate("⠃⠂"), "baie,");
        assert_eq!(table.translate("⠃⠂⠀⠁"), "baie, a");
    }

    #[test]
    fn backward_word_end_stops_at_a_contraction_cell() {
        // `always ing 346` makes its cell word-continuation, `begword` and `midword`
        // do not (liblouis#360).
        let table = backward_table(&[
            parse_rule("lowercase b 12"),
            parse_rule("punctuation ; 346"),
            parse_rule("punctuation ! 235"),
            parse_rule("always ing 346"),
            parse_rule("midword ea 235"),
            parse_rule("word but 12"),
        ]);
        assert_eq!(table.translate("⠃⠬"), "bing");
        assert_eq!(table.translate("⠃⠖"), "but!");
    }

    #[test]
    fn backward_letsign_suppresses_whole_words_up_to_a_space() {
        let table = backward_table(&[
            parse_rule("lowercase b 12"),
            parse_rule("punctuation - 36"),
            parse_rule("letsign 56"),
            parse_rule("space \\s 0"),
            parse_rule("word but 12"),
        ]);
        assert_eq!(table.translate("⠰⠃⠤⠃"), "b-b");
        assert_eq!(table.translate("⠰⠃⠀⠃"), "b but");
    }

    #[test]
    fn backward_endnum_only_before_a_non_digit() {
        let table = backward_table(&[
            parse_rule("numsign 3456"),
            parse_rule("digit 1 1"),
            parse_rule("litdigit 1 1"),
            parse_rule("lowercase h 125"),
            parse_rule("punctuation - 36"),
            parse_rule("punctuation \\x2014 36-36"),
            parse_rule("endnum \\x2013 36-36"),
        ]);
        assert_eq!(table.translate("⠼⠁⠤⠤⠓"), "1–h");
        assert_eq!(table.translate("⠼⠁⠤⠤⠁"), "1—1");
    }

    #[test]
    fn backward_prepunc_at_the_start_of_a_word() {
        let table = backward_table(&[
            parse_rule("lowercase a 1"),
            parse_rule("digit 8 236"),
            parse_rule("punctuation ( 1456"),
            parse_rule("prepunc ( 236"),
            parse_rule("space \\s 0"),
        ]);
        assert_eq!(table.translate("⠦⠁"), "(a");
        assert_eq!(table.translate("⠁⠀⠦⠁"), "a (a");
        assert_eq!(table.translate("⠁⠦⠁"), "a8a");
    }

    #[test]
    fn backward_postpunc_at_the_end_of_a_word() {
        let table = backward_table(&[
            parse_rule("lowercase a 1"),
            parse_rule("digit 8 236"),
            parse_rule("punctuation ? 1456"),
            parse_rule("postpunc ? 236"),
            parse_rule("space \\s 0"),
        ]);
        assert_eq!(table.translate("⠁⠦"), "a?");
        assert_eq!(table.translate("⠁⠦⠀⠁"), "a? a");
        assert_eq!(table.translate("⠁⠦⠁"), "a8a");
    }

    #[test]
    fn backward_always_beats_postpunc_on_the_same_cell() {
        let table = backward_table(&[
            parse_rule("letter \\x0A1D 356"),
            parse_rule("always \\x0A1D 356"),
            parse_rule("punctuation \" 236"),
            parse_rule("postpunc \" 356"),
        ]);
        assert_eq!(table.translate("⠴"), "\u{0A1D}");
    }

    #[test]
    fn backward_multind_consumes_its_indicators() {
        let table = backward_table(&[
            parse_rule("lowercase a 1"),
            parse_rule("capsletter 6"),
            parse_rule("letsign 56"),
            parse_rule("endcapsword 56"),
            parse_rule("multind 56-6 letsign capsletter"),
            parse_rule("word at 1"),
        ]);
        assert_eq!(table.translate("⠰⠠⠁"), "A");
    }
}
