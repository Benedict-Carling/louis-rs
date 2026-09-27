use std::collections::{HashMap, HashSet};

use crate::{
    Direction,
    emphasis::EmphasisSpan,
    parser::{
        AnchoredRule, Braille, BrailleChars, CharacterClass, CharacterClasses, HasDirection,
        HasNocross, HasPrecedence, WithClass, WithMatch,
    },
    translator::{
        CharacterDefinition, ClassConstraint, ResolvedTranslation, Rule, TranslationError,
        TranslationMode, TranslationOptions, TranslationStage,
        case::CaseClasses,
        context_pattern::{ContextPatterns, ContextPatternsBuilder},
        effect::Environment,
        indication::{
            Indicator, Indicators, PrecomputedIndications, computer_braille, emphasis, lettersign,
            mode, nocontract, numeric,
        },
        match_pattern::{MatchPatterns, MatchPatternsBuilder},
        position_constraints::{
            ComputerBrailleConstrainer, Constrainer, Constrainers, DontContractConstrainerBuilder,
            HyphenationConstrainerBuilder, PositionConstraints, PunctuationConstrainerBuilder,
        },
        sequence::{Sequences, whole_word_boundaries},
        table::{
            TableContext,
            backward::{self, BackwardSelector, Situation},
        },
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
    /// The text classes `joinnum` looks at
    space_chars: HashSet<char>,
    digit_chars: HashSet<char>,
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
    prepunc_trie: Trie,
    postpunc_trie: Trie,
    nocross_trie: Trie,
    /// `prepunc` and `postpunc` rules, kept apart from the other rules and from each
    /// other, as they often share characters or dots
    indicators: Indicators,
    constrainers: Constrainers,
    /// Detects compbrl-triggered computer braille regions.
    compbrl_scanner: Option<CompbrlScanner>,
    direction: Direction,
    /// Forward-only: the standing-alone check for `word` and `contraction` rules.
    sequences: Option<Sequences>,
    /// Backward-only: picks among the rules the trie finds at a position.
    backward: Option<BackwardSelector>,
    case: CaseClasses,
    capsletter_defined: bool,
    syllables: Vec<Vec<char>>,
    letter_or_litdigit_chars: HashSet<char>,
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
    mode_indicator: mode::IndicatorBuilder,
    lettersign_indicator: lettersign::IndicatorBuilder,
    nocontract_indicator: nocontract::IndicatorBuilder,
    emphasis_indicator: emphasis::IndicatorBuilder,
    computer_braille_indicator: computer_braille::IndicatorBuilder,
    dont_contract_constrainer: DontContractConstrainerBuilder,
    hyphenation_constrainer: HyphenationConstrainerBuilder,
    punctuation_constrainer: PunctuationConstrainerBuilder,
    compbrl_triggers: Vec<String>,
    sequences: Option<Sequences>,
    backward: Option<BackwardSelector>,
    direction: Direction,
    prepunc_trie: Trie,
    postpunc_trie: Trie,
    capsletter_defined: bool,
    syllables: Vec<Vec<char>>,
}

impl PrimaryTableBuilder {
    /// finalizeTable: a letter with a single-letter `word` or `largesign` rule never takes
    /// the letter sign.
    fn noletsign_if_single_letter(&mut self, rule: &AnchoredRule, chars: &str, ctx: &TableContext) {
        let mut letters = chars.chars();
        if rule.is_direction(Direction::Forward)
            && let (Some(c), None) = (letters.next(), letters.next())
            && ctx
                .character_classes()
                .get(&CharacterClass::Letter)
                .is_some_and(|letters| letters.contains(&c))
        {
            self.lettersign_indicator.noletsign(chars);
        }
    }

    /// `trie_ctx` must be the character classes to resolve `Transition`/`ClassConstraint`
    /// boundary checks against: text classes for forward translation, dots classes for
    /// backward. The tries need it from construction on, since `Trie::insert` resolves those
    /// checks against its own context immediately rather than deferring to match time.
    fn new(
        trie_ctx: CharacterClasses,
        direction: Direction,
        case_folding: HashMap<char, char>,
    ) -> Self {
        let trie = Trie::new()
            .with_context(trie_ctx.clone())
            .with_case_folding(case_folding.clone());
        let trie = match direction {
            Direction::Forward => trie,
            Direction::Backward => trie.keep_all(),
        };
        Self {
            undefined: None,
            character_translations: CharacterTranslation::new(),
            trie,
            comp6_trie: Trie::new().with_context(trie_ctx.clone()).case_sensitive(),
            prepunc_trie: Trie::new()
                .with_context(trie_ctx.clone())
                .with_case_folding(case_folding.clone()),
            postpunc_trie: Trie::new()
                .with_context(trie_ctx.clone())
                .with_case_folding(case_folding.clone()),
            nocross_trie: Trie::new()
                .with_context(trie_ctx)
                .with_case_folding(case_folding),
            match_patterns: MatchPatternsBuilder::new(),
            context_patterns: ContextPatternsBuilder::new().for_direction(direction),
            numeric_indicator: numeric::IndicatorBuilder::new(),
            mode_indicator: mode::IndicatorBuilder::new(),
            lettersign_indicator: lettersign::IndicatorBuilder::new(),
            nocontract_indicator: nocontract::IndicatorBuilder::new(),
            emphasis_indicator: emphasis::IndicatorBuilder::new(),
            computer_braille_indicator: computer_braille::IndicatorBuilder::new(),
            dont_contract_constrainer: DontContractConstrainerBuilder::new(),
            hyphenation_constrainer: HyphenationConstrainerBuilder::new(),
            punctuation_constrainer: PunctuationConstrainerBuilder::new(),
            compbrl_triggers: Vec::new(),
            sequences: None,
            backward: None,
            direction,
            capsletter_defined: false,
            syllables: Vec::new(),
        }
    }

    fn whole_word_boundaries(&self) -> (Transition, Option<Transition>) {
        whole_word_boundaries(self.sequences.as_ref())
    }

    /// Forward `prepunc` and `postpunc` rules live in tries of their own, so a rule of
    /// another opcode for the same characters does not replace them
    fn prepunc_trie_mut(&mut self, rule: &AnchoredRule) -> &mut Trie {
        match self.direction {
            Direction::Forward => &mut self.prepunc_trie,
            Direction::Backward => self.get_trie_mut(rule),
        }
    }

    fn postpunc_trie_mut(&mut self, rule: &AnchoredRule) -> &mut Trie {
        match self.direction {
            Direction::Forward => &mut self.postpunc_trie,
            Direction::Backward => self.get_trie_mut(rule),
        }
    }

    fn get_trie_mut(&mut self, rule: &AnchoredRule) -> &mut Trie {
        // back-translation ignores nocross
        if rule.is_nocross() && self.direction == Direction::Forward {
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
            // liblouis inserts nocontractsign, letsign, the caps start indicator and
            // numsign in this order (`translateString`)
            self.nocontract_indicator
                .build(ctx, self.sequences.clone())
                .map(Indicator::NoContract),
            self.lettersign_indicator
                .build(ctx, self.sequences.clone())
                .map(Indicator::LetterSign),
            self.numeric_indicator.build().map(Indicator::Numeric),
            self.mode_indicator.build(ctx).map(Indicator::Mode),
        ]
        .into_iter()
        .flatten()
        .collect();
        let class = |class| ctx.character_classes().get(&class).unwrap_or_default();
        PrimaryTable {
            undefined: self.undefined,
            space_chars: class(CharacterClass::Space),
            digit_chars: class(CharacterClass::Digit),
            character_translations: self.character_translations,
            direction,
            fallback_definitions: ctx.character_definitions().clone(),
            case: CaseClasses::from_context(ctx),
            capsletter_defined: self.capsletter_defined,
            syllables: self.syllables,
            letter_or_litdigit_chars: [CharacterClass::Letter, CharacterClass::Litdigit]
                .iter()
                .filter_map(|class| ctx.character_classes().get(class))
                .flatten()
                .collect(),
            trie: self.trie,
            comp6_trie: self.comp6_trie,
            nocross_trie: self.nocross_trie,
            prepunc_trie: self.prepunc_trie,
            postpunc_trie: self.postpunc_trie,
            match_patterns: self.match_patterns.build(),
            context_patterns: self.context_patterns.build(),
            indicators: Indicators::new(indicators, self.emphasis_indicator.build(ctx)),
            constrainers: Constrainers::new(
                [
                    Some(Constrainer::ComputerBraille(ComputerBrailleConstrainer {})),
                    dont_contract_constrainer.map(Constrainer::DontContract),
                    // liblouis's `nocross`/hyphenation-based syllable-break check is a
                    // forward-translation-only mechanism (lou_backTranslateString.c never
                    // consults it); a nocross rule is always an unrestricted candidate
                    // during backward translation.
                    (direction == Direction::Forward)
                        .then(|| self.hyphenation_constrainer.build())
                        .flatten()
                        .map(Constrainer::Hyphenation),
                    // liblouis's `CTO_PrePunc`/`CTO_PostPunc` are forward-translation-only
                    // opcodes (never consulted by `lou_backTranslateString.c`).
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
            sequences: self.sequences,
            backward: self.backward,
        }
    }
}

impl PrimaryTable {
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
        let mut builder = PrimaryTableBuilder::new(trie_ctx, direction, ctx.case_folding().clone());
        if direction == Direction::Forward {
            builder.sequences = Sequences::new(ctx);
        }

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

        let defined_chars: HashSet<char> = rules
            .iter()
            .filter_map(|rule| match &rule.rule {
                Rule::Space { character, .. }
                | Rule::Punctuation { character, .. }
                | Rule::Digit { character, .. }
                | Rule::Litdigit { character, .. }
                | Rule::Letter { character, .. }
                | Rule::Lowercase { character, .. }
                | Rule::Uppercase { character, .. }
                | Rule::Sign { character, .. }
                | Rule::Math { character, .. } => Some(*character),
                _ => None,
            })
            .collect();

        for (name, derived, base) in super::kept_bases(rules) {
            builder.emphasis_indicator.base(&name, derived, base);
        }

        // decpoint and hyphen take precedence over other character definition rules, so insert
        // them first
        for rule in rules {
            match &rule.rule {
                Rule::Decpoint {
                    character, dots, ..
                } => {
                    if direction == Direction::Forward {
                        builder.insert_character(*character, &dots.to_string(), direction, rule);
                    }
                    // A decimal point extends the numeric run: digits that follow it
                    // must not get a second numsign.  Registering it as a midnum
                    // character achieves this via the numeric indicator and constrainer.
                    let s = character.to_string();
                    builder.numeric_indicator.runs().midnum(&s);
                }
                Rule::Hyphen {
                    character, dots, ..
                } if direction == Direction::Forward => {
                    builder.insert_character(*character, &dots.to_string(), direction, rule);
                }
                _ => (),
            }
        }
        for rule in rules {
            match &rule.rule {
                Rule::Decpoint {
                    character, dots, ..
                }
                | Rule::Hyphen {
                    character, dots, ..
                } if direction == Direction::Backward => {
                    builder.insert_character(*character, &dots.to_string(), direction, rule);
                }
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
                }
                Rule::Litdigit {
                    character, dots, ..
                } => {
                    builder.insert_character(*character, &dots.to_string(), direction, rule);
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
                    if direction == Direction::Backward {
                        builder.insert_backward_indicator(&dots.to_string(), rule);
                    }
                }
                Rule::Nonumsign { dots, .. } => {
                    builder.numeric_indicator.nonumsign(&dots.to_string(), rule);
                    builder
                        .dont_contract_constrainer
                        .nonumsign(&dots.to_string());
                    if direction == Direction::Backward {
                        builder.insert_backward_indicator(&dots.to_string(), rule);
                    }
                }
                Rule::Numericnocontchars { chars } => {
                    builder.numeric_indicator.runs().numericnocontchars(chars);
                }
                Rule::Midendnumericmodechars { chars } => {
                    builder
                        .numeric_indicator
                        .runs()
                        .midendnumericmodechars(chars);
                }
                Rule::Capsnocont {} => {
                    builder.dont_contract_constrainer.capsnocont();
                }
                Rule::Numericmodechars { chars } => {
                    builder.numeric_indicator.runs().numericmodechars(chars);
                }
                Rule::Capsletter { dots, .. } => {
                    builder.capsletter_defined = true;
                    builder
                        .emphasis_indicator
                        .capsletter(&dots.to_string(), rule);
                    if direction == Direction::Backward {
                        builder.insert_backward_indicator(&dots.to_string(), rule);
                    }
                }
                Rule::Begcapsword { dots, .. } => {
                    builder
                        .emphasis_indicator
                        .begcapsword(&dots.to_string(), rule);
                    if direction == Direction::Backward {
                        builder.insert_backward_indicator(&dots.to_string(), rule);
                    }
                }
                Rule::Endcapsword { dots, .. } => {
                    builder
                        .emphasis_indicator
                        .endcapsword(&dots.to_string(), rule);
                    if direction == Direction::Backward {
                        builder.insert_backward_indicator(&dots.to_string(), rule);
                    }
                }
                Rule::Begcaps { dots } => {
                    builder.emphasis_indicator.begcaps(&dots.to_string(), rule);
                    if direction == Direction::Backward {
                        builder.insert_backward_indicator(&dots.to_string(), rule);
                    }
                }
                Rule::Endcaps { dots } => {
                    builder.emphasis_indicator.endcaps(&dots.to_string(), rule);
                    if direction == Direction::Backward {
                        builder.insert_backward_indicator(&dots.to_string(), rule);
                    }
                }
                Rule::Capsmodechars { chars } => {
                    builder.emphasis_indicator.capsmodechars(chars);
                }
                Rule::Begcapsphrase { dots } => {
                    builder
                        .emphasis_indicator
                        .begcapsphrase(&dots.to_string(), rule);
                }
                Rule::Endcapsphrase { dots, position } => {
                    builder
                        .emphasis_indicator
                        .endcapsphrase(&dots.to_string(), position, rule);
                }
                Rule::Lencapsphrase { number } => {
                    builder.emphasis_indicator.lencapsphrase(*number as usize);
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
                Rule::Letsign { dots } => {
                    builder
                        .lettersign_indicator
                        .letsign(&dots.to_string(), rule);
                    if direction == Direction::Backward {
                        builder.insert_backward_indicator(&dots.to_string(), rule);
                    }
                }
                Rule::Multind { dots, .. } if direction == Direction::Backward => {
                    builder.insert_backward_indicator(&dots.to_string(), rule);
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
                    if direction == Direction::Backward {
                        builder.insert_backward_indicator(&dots.to_string(), rule);
                    }
                }
                // liblouis defines no back-translation rule for a base character
                Rule::Base { .. } if direction == Direction::Backward => {}
                // like liblouis, a character's own definition wins over its base
                Rule::Base { derived, .. } if defined_chars.contains(derived) => {}
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
                    if direction == Direction::Forward {
                        builder
                            .syllables
                            .push(chars.to_lowercase().chars().collect());
                    }
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
                // liblouis never selects a `replace` rule when back-translating
                Rule::Replace { chars, replacement } if direction == Direction::Forward => {
                    let replacement = replacement.as_deref().unwrap_or_default();
                    if let Ok(dots) = ctx
                        .character_definitions()
                        .resolve_implicit_dots(replacement)
                    {
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
                }
                Rule::Begcomp { dots, .. } => {
                    builder
                        .computer_braille_indicator
                        .begcomp(&dots.to_string(), rule);
                    if direction == Direction::Backward {
                        builder.insert_backward_indicator(&dots.to_string(), rule);
                    }
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
                    builder.noletsign_if_single_letter(rule, chars, ctx);
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
                    builder.noletsign_if_single_letter(rule, chars, ctx);
                    let dots = ctx
                        .character_definitions()
                        .braille_to_unicode(dots, chars)?;
                    let class_constraints = Self::resolve_class_constraints(ctx, with_classes);
                    let (start, end) = builder.whole_word_boundaries();
                    builder.get_trie_mut(rule).insert(
                        chars,
                        &dots,
                        Some(start),
                        end,
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
                        Some(Transition::End(vec![CharacterClass::Letter])),
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
                        Some(Transition::End(vec![CharacterClass::Letter])),
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
                        Some(Transition::End(vec![CharacterClass::Letter])),
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
                        Some(Transition::End(vec![
                            CharacterClass::Letter,
                            CharacterClass::Space,
                            CharacterClass::Punctuation,
                        ])),
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
                        Some(Transition::End(vec![
                            CharacterClass::Space,
                            CharacterClass::Punctuation,
                        ])),
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
                        Some(Transition::End(vec![
                            CharacterClass::Space,
                            CharacterClass::Punctuation,
                        ])),
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
                        Some(Transition::End(vec![
                            CharacterClass::Space,
                            CharacterClass::Punctuation,
                        ])),
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
                        Some(Transition::End(vec![
                            CharacterClass::Space,
                            CharacterClass::Punctuation,
                        ])),
                        direction,
                        rule.precedence(),
                        class_constraints.clone(),
                        TranslationStage::Main,
                        rule,
                    );
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
                        Some(Transition::End(vec![CharacterClass::Letter])),
                        direction,
                        rule.precedence(),
                        class_constraints,
                        TranslationStage::Main,
                        rule,
                    );
                }
                Rule::Joinword { chars, dots, .. } => builder.get_trie_mut(rule).insert(
                    chars,
                    &dots.to_string(),
                    Some(Transition::Start(vec![
                        CharacterClass::Space,
                        CharacterClass::Punctuation,
                    ])),
                    // the rest of liblouis's forward check is in `joins_ahead`
                    Some(Transition::End(match direction {
                        Direction::Forward => vec![CharacterClass::Space],
                        Direction::Backward => {
                            vec![CharacterClass::Space, CharacterClass::Punctuation]
                        }
                    })),
                    direction,
                    rule.precedence(),
                    vec![],
                    TranslationStage::Main,
                    rule,
                ),
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
                Rule::Joinnum { chars, dots, .. } => builder.get_trie_mut(rule).insert(
                    chars,
                    &dots.to_string(),
                    Some(Transition::Start(vec![
                        CharacterClass::Space,
                        CharacterClass::Punctuation,
                    ])),
                    Some(Transition::End(vec![CharacterClass::Space])),
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
                        None,
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
                        ctx.direction_character_classes()
                            .get(&CharacterClass::Punctuation)
                            .is_some_and(|class| class.contains(&c))
                    }) {
                        // Eligibility isn't a fixed-position lookaround (see
                        // `PunctuationConstrainer`), so the trie itself imposes no
                        // boundary; `constraints.prepunc_ok_at()` gates it in `trace()`.
                        builder.punctuation_constrainer.mark_present();
                        builder.prepunc_trie_mut(rule).insert(
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
                }
                Rule::Postpunc { chars, dots, .. } => {
                    // are all the characters punctuation?
                    if chars.chars().all(|c| {
                        ctx.direction_character_classes()
                            .get(&CharacterClass::Punctuation)
                            .is_some_and(|class| class.contains(&c))
                    }) {
                        // See the comment in the `Rule::Prepunc` arm above.
                        builder.punctuation_constrainer.mark_present();
                        builder.postpunc_trie_mut(rule).insert(
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
        let numeric_chars: HashSet<char> = [CharacterClass::Digit, CharacterClass::Litdigit]
            .iter()
            .flat_map(|class| ctx.character_classes().get(class).unwrap_or_default())
            .collect();
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
            builder.backward = Some(BackwardSelector::new(
                rules,
                ctx,
                builder.trie.single_char_alternatives(),
            ));
        }
        Ok(builder.build(direction, ctx))
    }

    /// liblouis sets the attributes of characters and cells from every definition in a table,
    /// including those not meant for this direction. Back-translation consults them, so
    /// base them on all the `rules` of the table.
    pub fn with_all_definitions(mut self, rules: &[AnchoredRule], ctx: &TableContext) -> Self {
        if self.backward.is_some() {
            self.backward = Some(BackwardSelector::new(
                rules,
                ctx,
                self.trie.single_char_alternatives(),
            ));
        }
        self
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
    ) -> (Vec<ResolvedTranslation>, Vec<ResolvedTranslation>) {
        self.trie
            .find_translations(input, prev)
            .into_iter()
            .partition(|t| t.offset() == 0)
    }

    /// A nocross candidate is valid only if it doesn't cross a hyphenation break of the
    /// word it starts in, mirroring liblouis's `syllableBreak`. `char_pos` is the
    /// candidate's starting position in the whole input, needed to look up the
    /// precomputed breaks.
    fn nocross_candidates(
        &self,
        input: &str,
        prev: Option<char>,
        char_pos: usize,
        constraints: &PositionConstraints,
    ) -> Vec<ResolvedTranslation> {
        self.nocross_trie
            .find_translations(input, prev)
            .into_iter()
            .filter(|t| {
                !(char_pos..char_pos + t.length().saturating_sub(1))
                    .take_while(|&pos| constraints.letter_gap_at(pos))
                    .any(|pos| constraints.hyphenation_break_at(pos))
            })
            .collect()
    }

    /// liblouis's `markSyllables`: each `syllable` match, and everything after it up to the
    /// next one, gets one of two alternating marks, so no rule can span a syllable boundary.
    fn syllable_marks(&self, chars: &[char]) -> Vec<u64> {
        const MARKS: [u64; 2] = [1 << 61, 1 << 62];
        let mut marks = vec![0; chars.len()];
        if self.syllables.is_empty() {
            return marks;
        }
        let (mut current, mut next) = (0, 0);
        let mut pos = 0;
        while pos < chars.len() {
            let len = self
                .syllables
                .iter()
                .filter(|s| {
                    s.len() <= chars.len() - pos
                        && s.iter()
                            .zip(&chars[pos..])
                            .all(|(s, c)| c.to_lowercase().eq(std::iter::once(*s)))
                })
                .map(|s| s.len())
                .max();
            match len {
                Some(len) if len > 0 => {
                    current = MARKS[next];
                    next = 1 - next;
                    marks[pos..pos + len].iter_mut().for_each(|m| *m |= current);
                    pos += len;
                }
                _ => {
                    marks[pos] |= current;
                    pos += 1;
                }
            }
        }
        marks
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

    /// liblouis's `compbrlStart`..`compbrlEnd`: the word under the cursor, or up to the
    /// cursor with `compbrlLeftCursor`, or the cursor's character when it is a space.
    fn cursor_word(
        &self,
        input: &str,
        options: &TranslationOptions,
    ) -> Option<std::ops::Range<usize>> {
        let left = options.mode().contains(TranslationMode::CompbrlLeftCursor);
        if self.direction != Direction::Forward
            || !(left || options.mode().contains(TranslationMode::CompbrlAtCursor))
        {
            return None;
        }
        let chars: Vec<char> = input.chars().collect();
        let cursor = options.cursor_pos()?;
        let is_space = |pos: usize| self.space_chars.contains(&chars[pos]);
        if cursor >= chars.len() || is_space(cursor) {
            return Some(cursor..cursor + 1);
        }
        let start = (0..cursor)
            .rev()
            .find(|&pos| is_space(pos))
            .map_or(0, |pos| pos + 1);
        let end = if left {
            cursor
        } else {
            (cursor..chars.len())
                .find(|&pos| is_space(pos))
                .unwrap_or(chars.len())
        };
        Some(start..end)
    }

    fn context_candidates(
        &self,
        before: &str,
        input: &str,
        env: &Environment,
    ) -> (Vec<ResolvedTranslation>, Vec<ResolvedTranslation>) {
        self.context_patterns
            .find(before, input, env)
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
        // the loop at the same char_pos, where liblouis inserts the capital start
        // indicator again but nothing else.
        let mut last_indications_pos: Option<usize> = None;

        // Enrich spans with compbrl-detected regions.
        let enriched_spans = match &self.compbrl_scanner {
            Some(scanner) => scanner.enrich(input, options.emphasis()),
            None => options.emphasis().to_vec(),
        };

        let mut indications = self.indicators.precompute(input, &enriched_spans);
        let constraints = self.constrainers.precompute(input, &enriched_spans);
        let constraints = match self.cursor_word(input, options) {
            Some(range) => constraints.with_comp6(range),
            None => constraints,
        };
        let input_chars: Vec<char> = input.chars().collect();
        let stands_alone = |t: &ResolvedTranslation, pos: usize| match &self.sequences {
            Some(sequences)
                if matches!(t.rule(), Some(Rule::Word { .. } | Rule::Contraction { .. })) =>
            {
                sequences.in_sequence(&input_chars, pos, t.length())
            }
            _ => true,
        };
        // liblouis's typebuf: the emphasis classes of each character, plus a capitals bit
        // when the table defines capsletter
        let classes: Vec<&str> = options
            .emphasis()
            .iter()
            .map(|s| s.class.as_str())
            .collect();
        let syllable_marks = self.syllable_marks(&input_chars);
        let typebuf: Vec<u64> = (0..input_chars.len())
            .map(|pos| {
                let caps = self.capsletter_defined && self.case.is_uppercase(input_chars[pos]);
                options
                    .emphasis()
                    .iter()
                    .filter(|span| span.range.contains(&pos))
                    .filter_map(|span| classes.iter().position(|c| *c == span.class))
                    .fold(caps as u64 | syllable_marks[pos], |bits, i| {
                        bits | 2 << i.min(58)
                    })
            })
            .collect();
        let valid_match = |t: &ResolvedTranslation, pos: usize| {
            if self.direction == Direction::Backward {
                return true;
            }
            // liblouis files a context rule by the characters its test starts with
            let len = match t.rule() {
                Some(rule @ Rule::Context { .. }) => backward::context_cells(rule),
                _ => t.length(),
            };
            let end = (pos + len).min(typebuf.len());
            let first = typebuf.get(pos).copied().unwrap_or(0);
            let same_typeform =
                first & 1 != 0 || typebuf[pos.min(end)..end].iter().all(|k| k & !first == 0);
            same_typeform && self.case.valid_match(&input_chars, pos, len)
        };
        let emphasis_allows =
            |indications: &PrecomputedIndications, t: &ResolvedTranslation, pos: usize| {
                if self.direction == Direction::Backward {
                    return true;
                }
                let end = pos + t.length();
                let interior = matches!(
                    t.rule(),
                    Some(
                        Rule::Always { .. }
                            | Rule::Syllable { .. }
                            | Rule::Word { .. }
                            | Rule::Match { .. }
                    )
                ) && indications.changes_within(pos, t.length());
                let (before, after) = match t.rule() {
                    Some(Rule::Match {
                        matches: Some(m), ..
                    }) => (
                        m.contains(&WithMatch::Before),
                        m.contains(&WithMatch::After),
                    ),
                    _ => (false, false),
                };
                !(interior
                    || (before && indications.marked_at(pos))
                    || (after && indications.marked_at(end)))
            };

        let mut backward_state = backward::State::default();
        // a `context` rule liblouis files among its pass rules, due at the current position
        let mut forced: Option<ResolvedTranslation> = None;

        loop {
            let revisit = last_indications_pos == Some(char_pos);
            translations.extend(indications.translations_at(char_pos, revisit));
            last_indications_pos = Some(char_pos);

            // whether we're about to match starting at the true beginning of the whole input,
            // needed by match/context rules anchored with liblouis' `^`/`` ` `` boundary markers
            let at_start = chars.as_str().len() == input.len();

            // First check for nocross candidates
            // the checks of liblouis's `for_selectRule` that depend on indication state
            let dont_contract = constraints.dont_contract_at(char_pos);
            let selectable =
                |t: &ResolvedTranslation| !(dont_contract && t.rule().is_some_and(is_contraction));

            let nocross_candidate = self
                .nocross_candidates(chars.as_str(), prev, char_pos, &constraints)
                .into_iter()
                .filter(|t| {
                    stands_alone(t, char_pos)
                        && valid_match(t, char_pos)
                        && emphasis_allows(&indications, t, char_pos)
                        && selectable(t)
                })
                .max_by_key(|t| t.rank());

            // given an input query the trie for matching translations. Then split off the
            // translations that are delayed, i.e. have an offset because they have a pre-pattern
            let (mut candidates, delayed) = self.translation_candidates(chars.as_str(), prev);
            delayed_translations.extend(delayed);

            // then search for matching match patterns. Unless they have empty pre patterns they will all have
            // an offset. Split those off.
            let (match_candidates, match_delayed) = self.match_candidates(chars.as_str(), at_start);
            delayed_translations.extend(match_delayed);
            // merge the candidates from the match patters with the candidates from the plain translations
            candidates.extend(match_candidates);

            // then search for context patterns. Unless they have empty pre patterns they will all
            // have an offset. Split those off.
            let (mut context_candidates, mut context_delayed) = self.context_candidates(
                &input[..input.len() - chars.as_str().len()],
                chars.as_str(),
                &env,
            );
            if self.direction == Direction::Forward {
                let is_pass_rule = |t: &ResolvedTranslation| {
                    t.rule()
                        .is_some_and(|rule| backward::context_cells(rule) == 0)
                };
                let pass_rule = context_candidates
                    .iter()
                    .chain(&context_delayed)
                    .filter(|t| is_pass_rule(t) && !seen.contains(&TranslationSubset::from(*t)))
                    .max_by_key(|t| t.rank())
                    .cloned();
                context_candidates.retain(|t| !is_pass_rule(t));
                context_delayed.retain(|t| !is_pass_rule(t));
                if forced.is_none() && !constraints.use_comp6_at(char_pos) {
                    forced = pass_rule;
                }
            }
            delayed_translations.extend(context_delayed);
            // merge the candidates from the context patters with the candidates from the plain translations
            candidates.extend(context_candidates);

            // move delayed_translations with zero offset into candidates
            let (current, delayed) = self.partition_delayed_translations(delayed_translations);
            delayed_translations = delayed;
            candidates.extend(current);
            // a tie between the two goes to the later, prepunc, as when they shared a trie
            for trie in [&self.postpunc_trie, &self.prepunc_trie] {
                candidates.extend(trie.find_translations(chars.as_str(), prev));
            }

            // `prepunc`/`postpunc` need a variable-length scan the trie can't express
            // (see `PunctuationConstrainer`), so they're inserted with no trie-level
            // boundary and gated here instead. Rejected candidates are simply dropped,
            // leaving the position to fall through to the plain punctuation character
            // definition (`character_translations`), matching liblouis.
            candidates
                .retain(|t| valid_match(t, char_pos) && emphasis_allows(&indications, t, char_pos));

            candidates.retain(|t| match t.rule() {
                _ if self.backward.is_some() => true,
                Some(Rule::Joinword { .. } | Rule::Joinnum { .. })
                    if self.direction == Direction::Forward =>
                {
                    self.joins_ahead(t, chars.as_str())
                }
                Some(Rule::Prepunc { .. }) => constraints.prepunc_ok_at(char_pos),
                Some(Rule::Postpunc { .. }) => constraints.postpunc_ok_at(char_pos),
                _ => stands_alone(t, char_pos),
            });
            // the word a joinword or joinnum joins onto is one more condition it checks
            if self.direction == Direction::Forward {
                candidates = candidates
                    .into_iter()
                    .map(|t| match t.rule() {
                        Some(Rule::Joinword { .. } | Rule::Joinnum { .. }) => {
                            let weight = t.weight() + 1;
                            t.with_weight(weight)
                        }
                        _ => t,
                    })
                    .collect();
            }

            if self.direction == Direction::Forward {
                candidates.retain(selectable);
            }

            // In computer braille mode, discard normal candidates and use comp6 rules only.
            // comp6 rules have no pre-pattern so offset() is always 0 — no delayed handling needed.
            if constraints.use_comp6_at(char_pos) {
                candidates.clear();
                candidates.extend(self.comp6_trie.find_translations(chars.as_str(), prev));
            }

            // drop translation candidates that we have applied already at this position in the
            // input
            let unseen = candidates
                .iter()
                .filter(|t| !seen.contains(&TranslationSubset::from(*t)));
            let candidate = match &self.backward {
                Some(selector) => {
                    selector.begin(&mut backward_state, &translations);
                    let situation = Situation {
                        output: &translations,
                        cells: &input_chars,
                        pos: char_pos,
                        state: backward_state,
                        partial: options.mode().contains(TranslationMode::PartialTrans),
                    };
                    // `rev` so that of equally ranked rules the first defined wins, as in
                    // liblouis
                    unseen
                        .filter(|t| selector.is_valid(t, &situation))
                        .rev()
                        .max_by_key(|t| (t.length() == 0, BackwardSelector::order(t)))
                }
                // use the longest translation
                None => unseen.max_by_key(|translation| translation.rank()),
            };
            // liblouis's findForPassRule overrides the rule for_selectRule chose, and copies
            // the characters before the replacement one by one
            let forced_now = forced.take_if(|t| t.offset() == 0);
            let (candidate, nocross_candidate) = match (&forced_now, &forced) {
                (Some(t), _) => (Some(t), None),
                (None, Some(_)) => (None, None),
                (None, None) => (candidate, nocross_candidate),
            };
            forced = forced.map(|t| t.decrement_offset(1));
            // A nocross rule wins if there is no plain candidate to beat (many, e.g.
            // liblouis' `nocross always en`, have no plain counterpart at all).
            if let Some(nocross) = &nocross_candidate
                && candidate.is_none_or(|t| nocross.rank() >= t.rank())
            {
                let translation = nocross.clone();
                let consumed = chars.as_str();
                // move the iterator forward by the number of characters in the translation
                chars.nth(nocross.length() - 1);
                prev = translation.input().chars().last();
                char_pos += nocross.length();
                translations.push(translation);
                delayed_translations = self.update_offsets(delayed_translations, nocross.length());
                delayed_translations
                    .extend(self.match_candidates_within(consumed, nocross.length()));
            } else if let Some(t) = candidate {
                // an endnum rule takes back the letter sign inserted before it (liblouis's
                // CTO_EndNum case in translateString)
                if self.direction == Direction::Forward
                    && matches!(t.rule(), Some(Rule::Endnum { .. }))
                    && let Some(i) = translations
                        .iter()
                        .rposition(|t| matches!(t.rule(), Some(Rule::Letsign { .. })))
                    && translations[i + 1..]
                        .iter()
                        .all(|t| t.length() == 0 || t.input().is_empty())
                {
                    translations.remove(i);
                }
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
                let translation = match &self.backward {
                    Some(selector) => {
                        selector.applied(&mut backward_state, t.rule());
                        let output = selector.capitalize(&mut backward_state, &t.output());
                        t.clone().with_output(&output)
                    }
                    None => t.clone(),
                };
                translations.push(translation);
                // liblouis puts a space after a joinword or joinnum it back-translates, if
                // there is more input to map it to
                if self.direction == Direction::Backward
                    && matches!(t.rule(), Some(Rule::Joinword { .. } | Rule::Joinnum { .. }))
                    && char_pos < input_chars.len()
                {
                    translations.push(ResolvedTranslation::new(
                        "",
                        " ",
                        0,
                        TranslationStage::Main,
                        None,
                    ));
                }
                delayed_translations = self.update_offsets(delayed_translations, t.length());
                delayed_translations.extend(self.match_candidates_within(consumed, t.length()));
                // liblouis drops the spaces after a joinword or joinnum
                if self.direction == Direction::Forward
                    && matches!(t.rule(), Some(Rule::Joinword { .. } | Rule::Joinnum { .. }))
                {
                    while let Some(space) = chars.clone().next()
                        && self.space_chars.contains(&space)
                    {
                        chars.next();
                        prev = Some(space);
                        char_pos += 1;
                        translations.push(ResolvedTranslation::new(
                            &space.to_string(),
                            "",
                            1,
                            TranslationStage::Main,
                            None,
                        ));
                        delayed_translations = self.update_offsets(delayed_translations, 1);
                    }
                }

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

    /// liblouis's forward check for `joinword` (`onlyLettersAhead`) and `joinnum`: the
    /// match is followed by spaces and then by a letter or litdigit, or a digit for `joinnum`.
    fn joins_ahead(&self, t: &ResolvedTranslation, input: &str) -> bool {
        let mut rest = input.chars().skip(t.length()).peekable();
        if !rest.peek().is_some_and(|c| self.space_chars.contains(c)) {
            return false;
        }
        let Some(next) = rest.find(|c| !self.space_chars.contains(c)) else {
            return false;
        };
        match t.rule() {
            Some(Rule::Joinnum { .. }) => self.digit_chars.contains(&next),
            _ => self.letter_or_litdigit_chars.contains(&next),
        }
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

/// The opcodes liblouis's `for_selectRule` skips while `dontContract` is set
fn is_contraction(rule: &Rule) -> bool {
    matches!(
        rule,
        Rule::Repeated { .. }
            | Rule::Repword { .. }
            | Rule::Rependword { .. }
            | Rule::Nocont { .. }
            | Rule::Always { .. }
            | Rule::Syllable { .. }
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
    fn match_end_of_input_counts_as_checked_context() {
        let rules = [
            parse_rule("lowercase b 12"),
            parse_rule("lowercase e 15"),
            parse_rule("match - be b%[_~^] ="),
            parse_rule("match - be b 23"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("beb"), "⠃⠑⠃");
        assert_eq!(table.translate("bebb"), "⠆⠃⠃");
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
    fn prepunc_and_postpunc_for_the_same_character() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("space \\s 0"),
            parse_rule("punctuation \" 5"),
            parse_rule("prepunc \" 236"),
            parse_rule("postpunc \" 356"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("\"a\""), "⠦⠁⠴");
        assert_eq!(table.translate("a \" a"), "⠁⠀⠐⠀⠁");
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

    fn forward_table(rules: &[&str]) -> PrimaryTable {
        let rules: Vec<AnchoredRule> = rules.iter().map(|r| parse_rule(r)).collect();
        let context = TableContext::compile(&rules).unwrap();
        PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context).unwrap()
    }

    #[test]
    fn joinnum_joins_onto_a_following_number() {
        let table = forward_table(&[
            "space \\s 0",
            "digit 1 1",
            "digit 2 12",
            "litdigit 1 1",
            "litdigit 2 12",
            "lowercase a 1",
            "numsign 3456",
            "math + 235",
            "joinnum + 56-235",
        ]);
        assert_eq!(table.translate("1 + 2"), "⠼⠁⠀⠰⠖⠼⠃");
        assert_eq!(table.translate("1 + a"), "⠼⠁⠀⠖⠀⠁");
        assert_eq!(table.translate("+"), "⠖");
    }

    #[test]
    fn joinword_joins_onto_a_following_word() {
        let table = forward_table(&[
            "space \\s 0",
            "lowercase c 14",
            "lowercase e 15",
            "lowercase h 125",
            "lowercase t 2345",
            "punctuation . 256",
            "joinword the 2346",
            "word the 2346",
        ]);
        assert_eq!(table.translate("the cet"), "⠮⠉⠑⠞");
        assert_eq!(table.translate("the"), "⠮");
        assert_eq!(table.translate("the. cet"), "⠮⠲⠀⠉⠑⠞");
    }

    #[test]
    fn single_letter_word_takes_no_letsign() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase u 136"),
            parse_rule("letsign 56"),
            parse_rule("word u 136"),
            parse_rule("nofor word a 1"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("u"), "⠥");
        assert_eq!(table.translate("a"), "⠰⠁");
    }

    #[test]
    fn rule_does_not_span_a_change_of_case() {
        let rules = [
            parse_rule("lowercase c 14"),
            parse_rule("lowercase d 145"),
            parse_rule("lowercase m 134"),
            parse_rule("lowercase u 136"),
            parse_rule("base uppercase C c"),
            parse_rule("base uppercase D d"),
            parse_rule("base uppercase M m"),
            parse_rule("base uppercase U u"),
            parse_rule("capsletter 6"),
            parse_rule("always cc 25"),
            parse_rule("always duc 1256"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("mcc"), "⠍⠒");
        // the second c gets a capital sign within the span of `always cc`
        assert_eq!(table.translate("mcC"), "⠍⠉⠠⠉");
        // the case may change after the first character, but not later
        assert_eq!(table.translate("Duc"), "⠠⠳");
        assert_eq!(table.translate("duC"), "⠙⠥⠠⠉");
    }

    #[test]
    fn lettersign_before_capital_sign() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase c 14"),
            parse_rule("base uppercase A a"),
            parse_rule("base uppercase C c"),
            parse_rule("capsletter 6"),
            parse_rule("nocontractsign 56"),
            parse_rule("contraction ac"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("Ac"), "⠰⠠⠁⠉");
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
    fn indicators_follow_liblouis_insertion_order() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("base uppercase A a"),
            parse_rule("base uppercase B b"),
            parse_rule("litdigit 1 1"),
            parse_rule("capsletter 6"),
            parse_rule("numsign 3456"),
            parse_rule("nonumsign 56"),
            parse_rule("numericnocontchars abAB"),
            parse_rule("nocontractsign 5"),
            parse_rule("contraction b"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        // nocontractsign before caps start
        assert_eq!(table.translate("B"), "⠐⠠⠃");
        // caps start before nonumsign
        assert_eq!(table.translate("1A"), "⠼⠁⠠⠰⠁");
    }

    #[test]
    fn empmatchafter_blocks_a_match_before_a_capital() {
        let rules = [
            parse_rule("lowercase s 234"),
            parse_rule("lowercase e 15"),
            parse_rule("lowercase a 1"),
            parse_rule("lowercase w 2456"),
            parse_rule("base uppercase S s"),
            parse_rule("base uppercase W w"),
            parse_rule("capsletter 6"),
            parse_rule("empmatchafter match %a ea %a 2"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("seaw"), "⠎⠂⠺");
        assert_eq!(table.translate("SeaW"), "⠠⠎⠑⠁⠠⠺");
    }

    #[test]
    fn capital_inside_a_rule_prevents_it() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("base uppercase A a"),
            parse_rule("base uppercase B b"),
            parse_rule("capsletter 6"),
            parse_rule("always ab 1234"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("ab"), "⠏");
        assert_eq!(table.translate("aB"), "⠁⠠⠃");
    }

    #[test]
    fn rule_must_not_reach_into_a_typeform_its_first_character_lacks() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("base uppercase A a"),
            parse_rule("base uppercase B b"),
            parse_rule("capsletter 6"),
            parse_rule("always ab 1234"),
            parse_rule("always abb 123456"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        let italic_b =
            TranslationOptions::default().with_emphasis(vec![EmphasisSpan::new("italic", 1..2)]);
        assert_eq!(table.translate_with_options("ab", &italic_b), "⠁⠃");
        let italic_a =
            TranslationOptions::default().with_emphasis(vec![EmphasisSpan::new("italic", 0..1)]);
        assert_eq!(table.translate_with_options("ab", &italic_a), "⠏");
        // a capital first letter lifts the check
        assert_eq!(table.translate("Abb"), "⠠⠿");
        assert_eq!(table.translate("aBb"), "⠁⠠⠃⠃");
    }

    #[test]
    fn joinword_swallows_the_spaces_before_a_word() {
        let rules = [
            parse_rule("space \\s 0"),
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("punctuation . 256"),
            parse_rule("joinword a 1-2"),
            parse_rule("word a 1-5"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("a  b"), "⠁⠂⠃");
        assert_eq!(table.translate("a ."), "⠁⠐⠀⠲");
        assert_eq!(table.translate("a"), "⠁⠐");
    }

    #[test]
    fn single_letter_word_rule_means_no_letter_sign() {
        let rules = [
            parse_rule("lowercase i 24"),
            parse_rule("lowercase b 12"),
            parse_rule("punctuation : 25"),
            parse_rule("letsign 56"),
            parse_rule("word i 24"),
            parse_rule("contraction ib"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate(":i"), "⠒⠊");
        assert_eq!(table.translate(":b"), "⠒⠰⠃");
        // the letter sign before a contraction ignores noletsign
        assert_eq!(table.translate("ib"), "⠰⠊⠃");
    }

    #[test]
    fn no_rule_spans_a_syllable_boundary() {
        let rules = [
            parse_rule("lowercase b 12"),
            parse_rule("lowercase i 24"),
            parse_rule("lowercase g 1245"),
            parse_rule("lowercase h 125"),
            parse_rule("lowercase e 15"),
            parse_rule("lowercase a 1"),
            parse_rule("lowercase d 145"),
            parse_rule("always gh 126"),
            parse_rule("always ea 2"),
            parse_rule("syllable head 125-2-145"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("bigh"), "⠃⠊⠣");
        assert_eq!(table.translate("bighead"), "⠃⠊⠛⠓⠂⠙");
    }

    #[test]
    fn lowword_needs_spaces_on_both_sides() {
        let rules = [
            parse_rule("space \\s 0"),
            parse_rule("lowercase i 24"),
            parse_rule("lowercase n 1345"),
            parse_rule("punctuation . 256"),
            parse_rule("lowword in 35"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("in"), "⠔");
        assert_eq!(table.translate("in."), "⠊⠝⠲");
    }

    #[test]
    fn number_refuses_single_letter_always_rules_after_it() {
        let rules = [
            parse_rule("space \\s 0"),
            parse_rule("lowercase a 1"),
            parse_rule("litdigit 1 1"),
            parse_rule("punctuation . 256"),
            parse_rule("numsign 3456"),
            parse_rule("numericmodechars ."),
            parse_rule("nocontractsign 56"),
            parse_rule("nonumsign 56"),
            parse_rule("always a 6"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("a"), "⠠");
        assert_eq!(table.translate("1a"), "⠼⠁⠁");
        assert_eq!(table.translate("1 a"), "⠼⠁⠀⠠");
    }

    #[test]
    fn endnum_takes_back_the_letter_sign() {
        let rules = [
            parse_rule("lowercase s 234"),
            parse_rule("lowercase t 2345"),
            parse_rule("lowercase b 12"),
            parse_rule("digit 1 1"),
            parse_rule("letsign 56"),
            parse_rule("endnum st 34"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("1st"), "⠁⠌");
        assert_eq!(table.translate("1bt"), "⠁⠰⠃⠞");
    }

    #[test]
    fn case_change_inside_a_rule_prevents_it() {
        let rules = [
            parse_rule("lowercase m 134"),
            parse_rule("lowercase i 24"),
            parse_rule("lowercase t 2345"),
            parse_rule("base uppercase M m"),
            parse_rule("base uppercase I i"),
            parse_rule("base uppercase T t"),
            parse_rule("always mit 2345"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("Mit"), "⠞");
        assert_eq!(table.translate("MIT"), "⠞");
        assert_eq!(table.translate("mIt"), "⠍⠊⠞");
        assert_eq!(table.translate("miT"), "⠍⠊⠞");
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

    /// liblouis's `syllableBreak` hyphenates the lowercased word the rule starts in, and
    /// only that word
    #[test]
    fn nocross_checks_the_lowercased_word_it_starts_in() {
        let rules = vec![
            parse_rule("include dictionaries/nocross-test.dic"),
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("lowercase s 234"),
            parse_rule("uppercase S 234"),
            parse_rule("uppercase B 12"),
            parse_rule("punctuation - 36"),
            parse_rule("nocross always sb 456"),
            parse_rule("nocross always -sb 3456"),
        ];
        let rules = expand_includes(rules, &fixture_search_path(), &[]).unwrap();
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("asb"), "⠁⠎⠃");
        assert_eq!(table.translate("aSB"), "⠁⠎⠃");
        assert_eq!(table.translate("a-sb"), "⠁⠼");
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

    fn backward_table(rules: &[AnchoredRule]) -> PrimaryTable {
        let context = TableContext::compile(rules).unwrap();
        PrimaryTable::compile(rules, Direction::Backward, TranslationStage::Main, &context).unwrap()
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
    fn backward_endnum_only_before_a_non_digit() {
        let table = backward_table(&[
            parse_rule("numsign 3456"),
            parse_rule("digit 1 1"),
            parse_rule("litdigit 1 1"),
            parse_rule("lowercase h 125"),
            parse_rule("punctuation - 36"),
            parse_rule("endnum \\x2013 36-36"),
        ]);
        assert_eq!(table.translate("⠼⠁⠤⠤⠓"), "1–h");
        assert_eq!(table.translate("⠼⠁⠤⠤⠁"), "1--1");
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
    fn backward_joinnum_puts_a_space_after_it() {
        let table = backward_table(&[
            parse_rule("space \\s 0"),
            parse_rule("sign § 346"),
            parse_rule("digit 4 145"),
            parse_rule("joinnum § 346"),
        ]);
        assert_eq!(table.translate("⠬⠙"), "§ 4");
        assert_eq!(table.translate("⠬"), "§");
    }

    #[test]
    fn backward_begcomp_is_an_indicator() {
        let table = backward_table(&[
            parse_rule("lowercase a 1"),
            parse_rule("punctuation ' 6"),
            parse_rule("begcomp 6-46"),
        ]);
        assert_eq!(table.translate("⠠⠨⠁"), "a");
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
    fn backward_word_checks_the_text_before_it() {
        let rules = [
            parse_rule("lowercase b 12"),
            parse_rule("lowercase e 15"),
            parse_rule("lowercase a 1"),
            parse_rule("punctuation , 2"),
            parse_rule("word but 12"),
            parse_rule("midword ea 2"),
            parse_rule("space \\s 0"),
        ];
        let table = backward_table(&rules);
        assert_eq!(table.translate("⠃"), "but");
        assert_eq!(table.translate("⠃⠀⠃"), "but but");
        // the cell before ⠃ is a comma cell, but the text before it is the letter a
        assert_eq!(table.translate("⠑⠂⠃"), "eeab");
    }

    #[test]
    fn backward_end_of_word_skips_cells_that_are_not_letters() {
        let rules = [
            parse_rule("lowercase b 12"),
            parse_rule("lowercase e 15"),
            parse_rule("lowercase a 1"),
            parse_rule("punctuation , 2"),
            parse_rule("word but 12"),
            parse_rule("midword ea 2"),
            parse_rule("always ch 16"),
            parse_rule("space \\s 0"),
        ];
        let table = backward_table(&rules);
        // ⠂ says nothing about the end of the word, ⠡ stands for letters, so ⠃ is not a word
        assert_eq!(table.translate("⠃⠂⠡"), "beach");
        // at the end of the input the word does end
        assert_eq!(table.translate("⠃⠂"), "but,");
    }

    #[test]
    fn backward_prefers_a_longer_translation_of_one_cell() {
        let rules = [
            parse_rule("lowercase y 13456"),
            parse_rule("word you 13456"),
            parse_rule("lowercase b 12"),
            parse_rule("space \\s 0"),
        ];
        let table = backward_table(&rules);
        assert_eq!(table.translate("⠽"), "you");
        assert_eq!(table.translate("⠃⠽"), "by");
    }

    #[test]
    fn backward_indicator_before_character_definition() {
        let rules = [
            parse_rule("sign # 3456"),
            parse_rule("numsign 3456"),
            parse_rule("lowercase a 1"),
            parse_rule("litdigit 1 1"),
        ];
        let table = backward_table(&rules);
        assert_eq!(table.translate("⠼⠁"), "1");
        assert_eq!(table.translate("⠁"), "a");
    }

    #[test]
    fn backward_letsign_stops_whole_word_contractions() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("word but 12"),
            parse_rule("letsign 56"),
            parse_rule("space \\s 0"),
        ];
        let table = backward_table(&rules);
        assert_eq!(table.translate("⠰⠃"), "b");
        assert_eq!(table.translate("⠰⠃⠀⠃"), "b but");
    }

    #[test]
    fn backward_multind_applies_its_indicators() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("word also 1-123"),
            parse_rule("lowercase l 123"),
            parse_rule("nocontractsign 56"),
            parse_rule("capsletter 6"),
            parse_rule("multind 56-6 nocontractsign capsletter"),
        ];
        let table = backward_table(&rules);
        assert_eq!(table.translate("⠁⠇"), "also");
        assert_eq!(table.translate("⠰⠠⠁⠇"), "Al");
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
    fn context_lookback_reaches_into_a_consumed_rule() {
        let table = forward_table(&[
            "lowercase a 1",
            "lowercase r 1235",
            "lowercase o 135",
            "lowercase d 145",
            "always ar 345",
            "context _$l[\"od\"] @46",
        ]);
        assert_eq!(table.translate("arod"), "⠜⠨");
        assert_eq!(table.translate("od"), "⠕⠙");
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

    /// A `context` rule whose test starts with no literal is one of liblouis's pass rules:
    /// when it matches, it overrides the rule chosen at that position, even a longer one
    #[test]
    fn context_pass_rule_overrides_longer_rule() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("lowercase c 14"),
            parse_rule("always abc 3456"),
            parse_rule("context $l[\"bc\"] @2"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("abc"), "⠁⠂");
        assert_eq!(table.translate("cabc"), "⠉⠁⠂");
    }

    /// `compbrlAtCursor` spells out the word under the cursor character by character,
    /// with no computer braille indicators; `compbrlLeftCursor` only up to the cursor
    #[test]
    fn compbrl_at_cursor_spells_out_the_word() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("space \\s 0"),
            parse_rule("always ab 3456"),
            parse_rule("begcomp 456"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        let options = |mode: TranslationMode, cursor: usize| {
            let mut modes = crate::translator::options::TranslationModes::empty();
            modes.insert(mode);
            TranslationOptions::default()
                .with_mode(modes)
                .with_cursor_pos(cursor)
        };
        let at = options(TranslationMode::CompbrlAtCursor, 4);
        assert_eq!(table.translate_with_options("ab abab", &at), "⠼⠀⠁⠃⠁⠃");
        let left = options(TranslationMode::CompbrlLeftCursor, 4);
        assert_eq!(table.translate_with_options("ab abab", &left), "⠼⠀⠁⠃⠼");
    }

    #[test]
    fn numsign_before_a_digit() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("digit 1 1"),
            parse_rule("numsign 3456"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("1a1"), "⠼⠁⠁⠼⠁");
    }

    #[test]
    fn own_definition_beats_base() {
        let rules = [
            parse_rule("lowercase v 1236"),
            parse_rule("base uppercase V v"),
            parse_rule("letter V 1236f"),
            parse_rule("capsletter 46"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("Vv"), "⠨\u{f4027}⠧");
    }

    #[test]
    fn joinnum() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("space \\s 0"),
            parse_rule("digit 1 1"),
            parse_rule("sign § 346"),
            parse_rule("joinnum § 346"),
            parse_rule("numsign 3456"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("§ 1"), "⠬⠼⠁");
        assert_eq!(table.translate("§  1"), "⠬⠼⠁");
        assert_eq!(table.translate("§ a"), "⠬⠀⠁");
    }

    #[test]
    fn context_rule_inside_a_number() {
        let rules = [
            parse_rule("space \\s 0"),
            parse_rule("digit 1 1"),
            parse_rule("numsign 3456"),
            parse_rule("noback context [$d$s]$d @1"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("1 1"), "⠼⠁⠼⠁");
    }

    #[test]
    fn replace() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("space \\x2060 0"),
            parse_rule("replace \\x2060"),
            parse_rule("replace c ab"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table =
            PrimaryTable::compile(&rules, Direction::Forward, TranslationStage::Main, &context)
                .unwrap();
        assert_eq!(table.translate("a\u{2060}b"), "⠁⠃");
        assert_eq!(table.translate("c"), "⠁⠃");
        let table = PrimaryTable::compile(
            &rules,
            Direction::Backward,
            TranslationStage::Main,
            &context,
        )
        .unwrap();
        assert_eq!(table.translate("⠁⠃"), "ab");
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

    /// liblouis ends a caps word on the rules applied and the text produced, as its
    /// `allUpper` state, not on the cells: a blank cell the table also defines as a letter
    /// still ends it, being a space rule
    #[test]
    fn backward_capsword_ends_at_space_rule_and_non_letter_output() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("base uppercase A a"),
            parse_rule("base uppercase B b"),
            parse_rule("space \\s 0"),
            parse_rule("letter \\x00df 0"),
            parse_rule("punctuation - 36"),
            parse_rule("capsletter 46"),
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
        assert_eq!(table.translate("⠨⠨⠁⠃⠀⠁⠃"), "AB ab");
        assert_eq!(table.translate("⠨⠨⠁⠃⠤⠁⠃"), "AB-ab");
        assert_eq!(table.translate("⠨⠁⠃"), "Ab");
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
    fn backward_caps_passage_runs_to_endcaps() {
        let rules = [
            parse_rule("space \\s 0"),
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("base uppercase A a"),
            parse_rule("base uppercase B b"),
            parse_rule("begcapsword 46-46"),
            parse_rule("begcaps 45"),
            parse_rule("endcaps 56"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        let table = PrimaryTable::compile(
            &rules,
            Direction::Backward,
            TranslationStage::Main,
            &context,
        )
        .unwrap();
        assert_eq!(table.translate("⠘⠁⠀⠃⠰⠀⠁"), "A B a");
        assert_eq!(table.translate("⠨⠨⠁⠃⠀⠁"), "AB a");
    }
}
