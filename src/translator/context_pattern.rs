//! Store and find context rules using a simple regexp engine ([`Regexp`](crate::translator::regexp))

use std::collections::HashSet;

use crate::parser::multipass::test::Operator;
use crate::parser::{Action, ActionInstruction, HasPrecedence, Quantifier, Test, TestInstruction};

use crate::Direction;
use crate::parser::{AnchoredRule, Attribute, CharacterClass, CharacterClasses};
use crate::translator::effect::{Effect, Environment};
use crate::translator::regexp::{self, CompiledRegexp, Regexp};
use crate::translator::swap::SwapClasses;
use crate::translator::table::TableContext;
use crate::translator::translation::{
    Translation, TranslationTarget, TranslationTargets, UnresolvedTranslation,
};
use crate::translator::{ResolvedTranslation, TranslationError, TranslationStage};

impl Regexp {
    fn from_test(test: &Test, whole_match: bool, ctx: &CharacterClasses) -> Self {
        Regexp::from_test_in(test, whole_match, ctx, Direction::Forward)
    }

    fn from_test_in(
        test: &Test,
        whole_match: bool,
        ctx: &CharacterClasses,
        direction: Direction,
    ) -> Self {
        let regexp = Regexp::from_instructions(test.tests(), whole_match, ctx, direction);
        if whole_match {
            Regexp::Consume(Box::new(regexp))
        } else {
            regexp
        }
    }

    fn from_instructions(
        instructions: &[TestInstruction],
        whole_match: bool,
        ctx: &CharacterClasses,
        direction: Direction,
    ) -> Self {
        instructions
            .iter()
            .map(|instruction| Regexp::from_instruction(instruction, whole_match, ctx, direction))
            .reduce(|ast, other| Regexp::Concat(Box::new(ast), Box::new(other)))
            .unwrap_or(Regexp::Empty)
    }

    fn from_instruction(
        instruction: &TestInstruction,
        whole_match: bool,
        ctx: &CharacterClasses,
        direction: Direction,
    ) -> Self {
        match instruction {
            // a leading `_N` is handled by matching from N characters back (see `find`)
            TestInstruction::Lookback { .. } => Regexp::Empty,
            TestInstruction::Variable { var, op, operand } => match op {
                Operator::Eq => Regexp::VariableEqual(*var, *operand),
                // FIXME: operands other than equal do not seem to exist in the wild (at least not
                // in the tables that are shipped with liblouis), so maybe that feature should be
                // removed
                _ => todo!(),
            },
            TestInstruction::String { s } => Regexp::String(s.to_string()),
            TestInstruction::Dots { dots } => Regexp::String(dots.to_string()),
            TestInstruction::Attributes { attrs, quantifier } => {
                Regexp::from_multipass_attributes(attrs, quantifier, ctx)
            }
            TestInstruction::Class { name, quantifier } => {
                Regexp::from_class(name, quantifier, ctx)
            }
            TestInstruction::Negate { test } => match (direction, test.as_ref()) {
                (Direction::Backward, TestInstruction::Attributes { attrs, quantifier }) => {
                    Regexp::backward_negation(Regexp::attributes_set(attrs, ctx), quantifier)
                }
                (Direction::Backward, TestInstruction::Class { name, quantifier }) => {
                    Regexp::backward_negation(Some(Regexp::class_set(name, ctx)), quantifier)
                }
                (Direction::Forward, TestInstruction::Attributes { attrs, quantifier }) => {
                    Regexp::forward_negation(Regexp::attributes_set(attrs, ctx), quantifier)
                }
                (Direction::Forward, TestInstruction::Class { name, quantifier }) => {
                    Regexp::forward_negation(Some(Regexp::class_set(name, ctx)), quantifier)
                }
                _ => Regexp::from_instruction(test, whole_match, ctx, direction).negate(),
            },
            TestInstruction::Replace { tests } => {
                let regexp = Regexp::Capture(Box::new(Regexp::from_instructions(
                    tests,
                    whole_match,
                    ctx,
                    direction,
                )));
                if whole_match {
                    regexp
                } else {
                    Regexp::Consume(Box::new(regexp))
                }
            }
            TestInstruction::AtBeginning => Regexp::StartAnchor,
            TestInstruction::AtEnd => Regexp::EndAnchor,
        }
    }

    /// A quantified attribute test: liblouis's `passDoTest` takes as many matching
    /// characters as the quantifier allows and never gives any back
    fn possessive(characters: Option<HashSet<char>>, quantifier: &Option<Quantifier>) -> Self {
        let (single, not_ahead) = match characters {
            Some(characters) => (
                Regexp::CharacterClass(characters.clone()),
                Regexp::NotCharacterClassAhead(characters),
            ),
            None => (Regexp::Any, Regexp::EndAnchor),
        };
        match quantifier {
            None => single,
            Some(Quantifier::Number(n)) => Regexp::RepeatExactly(*n, Box::new(single)),
            Some(Quantifier::Range(min, max)) => {
                let tail = (*min..*max).fold(Regexp::Empty, |rest, _| {
                    Regexp::Either(
                        Box::new(Regexp::Concat(Box::new(single.clone()), Box::new(rest))),
                        Box::new(not_ahead.clone()),
                    )
                });
                Regexp::Concat(
                    Box::new(Regexp::RepeatExactly(*min, Box::new(single))),
                    Box::new(tail),
                )
            }
            Some(Quantifier::OneOrMore) => Regexp::Concat(
                Box::new(Regexp::OneOrMore(Box::new(single))),
                Box::new(not_ahead),
            ),
        }
    }

    fn from_class(name: &str, quantifier: &Option<Quantifier>, ctx: &CharacterClasses) -> Self {
        Regexp::possessive(Some(Regexp::class_set(name, ctx)), quantifier)
    }

    fn class_set(name: &str, ctx: &CharacterClasses) -> HashSet<char> {
        ctx.get(&CharacterClass::from(name)).unwrap_or_default() // FIXME: should probably fail if we cannot find the class
    }

    fn from_multipass_attributes(
        attrs: &HashSet<Attribute>,
        quantifier: &Option<Quantifier>,
        ctx: &CharacterClasses,
    ) -> Self {
        Regexp::possessive(Regexp::attributes_set(attrs, ctx), quantifier)
    }

    /// The characters an attribute test matches, or `None` for every character: liblouis
    /// compiles `a` to the full attribute mask, so it matches any character no matter what
    /// else is in the list
    fn attributes_set(attrs: &HashSet<Attribute>, ctx: &CharacterClasses) -> Option<HashSet<char>> {
        if attrs.contains(&Attribute::Any) {
            return None;
        }
        let mut characters: HashSet<char> = HashSet::default();
        for attr in attrs {
            match attr {
                Attribute::Class(class) => {
                    if let Some(chars) = ctx.get(class) {
                        characters.extend(chars);
                    }
                }
                Attribute::Boundary => (),   // TODO
                Attribute::ByOrder(_) => (), // TODO
                Attribute::Any => unreachable!("handled above"),
            }
        }
        Some(characters)
    }

    fn forward_negation(
        characters: Option<HashSet<char>>,
        quantifier: &Option<Quantifier>,
    ) -> Self {
        let single = match characters {
            Some(characters) => Regexp::CharacterClass(characters),
            None => Regexp::Any,
        }
        .negate();
        match quantifier {
            None => single,
            Some(Quantifier::Number(n)) => Regexp::RepeatExactly(*n, Box::new(single)),
            Some(Quantifier::Range(min, max)) => {
                Regexp::RepeatAtLeastAtMost(*min, *max, Box::new(single))
            }
            Some(Quantifier::OneOrMore) => Regexp::OneOrMore(Box::new(single)),
        }
    }

    /// A negated attribute test in back-translation: liblouis's `back_passDoTest` inverts
    /// the test's outcome, so it holds when fewer than the minimum count of characters
    /// match, and it consumes only those that did
    fn backward_negation(
        characters: Option<HashSet<char>>,
        quantifier: &Option<Quantifier>,
    ) -> Self {
        let min = match quantifier {
            Some(Quantifier::Number(n)) | Some(Quantifier::Range(n, _)) => *n,
            None | Some(Quantifier::OneOrMore) => 1,
        };
        let (single, not_ahead) = match characters {
            Some(characters) => (
                Regexp::CharacterClass(characters.clone()),
                Regexp::NotCharacterClassAhead(characters),
            ),
            None => (Regexp::Any, Regexp::EndAnchor),
        };
        (0..min)
            .map(|k| {
                Regexp::Concat(
                    Box::new(Regexp::RepeatExactly(k, Box::new(single.clone()))),
                    Box::new(not_ahead.clone()),
                )
            })
            .reduce(|a, b| Regexp::Either(Box::new(a), Box::new(b)))
            .unwrap_or(Regexp::Never)
    }
}

impl TranslationTarget {
    fn from_instruction(
        value: ActionInstruction,
        ctx: &SwapClasses,
    ) -> Result<Self, TranslationError> {
        match value {
            ActionInstruction::String { s } => Ok(TranslationTarget::Literal(s)),
            ActionInstruction::Dots { dots } => Ok(TranslationTarget::Literal(dots.to_string())),
            ActionInstruction::SwapClass { name } => {
                let swapper = ctx
                    .get(&name)
                    .ok_or(TranslationError::SwapClassNotDefined(name))?;
                Ok(TranslationTarget::Swap(swapper))
            }
            ActionInstruction::Replace => Ok(TranslationTarget::Capture),
            ActionInstruction::Ignore => Ok(TranslationTarget::Literal("".to_string())),
            // ignore instructions that change the environment
            ActionInstruction::Assignment { .. }
            | ActionInstruction::Increment { .. }
            | ActionInstruction::Decrement { .. } => Ok(TranslationTarget::Literal("".to_string())),
        }
    }
}

impl TranslationTargets {
    fn from_instructions(
        values: &[ActionInstruction],
        ctx: &SwapClasses,
    ) -> Result<Vec<TranslationTarget>, TranslationError> {
        values
            .iter()
            .cloned()
            .map(|instruction| TranslationTarget::from_instruction(instruction, ctx))
            .collect()
    }
}

/// A sequence of [`Effect`]
#[derive(Debug, Clone)]
pub struct Effects(Vec<Effect>);

impl Effects {
    fn from_instructions(values: &[ActionInstruction]) -> Vec<Effect> {
        values
            .iter()
            .cloned()
            .flat_map(Effects::from_instruction)
            .collect()
    }

    fn from_instruction(value: ActionInstruction) -> Option<Effect> {
        if let ActionInstruction::Assignment { variable, value } = value {
            Some(Effect::new(variable, value))
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub struct ContextPatternsBuilder {
    regexps: Vec<Pattern>,
    direction: Direction,
}

/// A compiled context pattern, with the number of characters its test first steps back
/// over and whether its action replaces the whole match
#[derive(Debug)]
struct Pattern {
    lookback: usize,
    whole_match: bool,
    regexp: CompiledRegexp,
}

impl ContextPatternsBuilder {
    pub fn new() -> Self {
        Self {
            regexps: Vec::new(),
            direction: Direction::Forward,
        }
    }

    pub fn for_direction(self, direction: Direction) -> Self {
        Self { direction, ..self }
    }

    /// Create a translation based on `test`, `action`, `origin` and `swap_classes`
    fn translation(
        &self,
        action: &Action,
        origin: &AnchoredRule,
        stage: TranslationStage,
        swap_classes: &SwapClasses,
    ) -> Result<UnresolvedTranslation, TranslationError> {
        let targets = TranslationTargets::from_instructions(&action.actions(), swap_classes)?;
        let effects = Effects::from_instructions(&action.actions());
        Ok(UnresolvedTranslation::new(
            &targets,
            origin.precedence(),
            stage,
            &effects,
            origin.clone(),
        ))
    }

    pub fn insert(
        &mut self,
        test: &Test,
        action: &Action,
        origin: &AnchoredRule,
        stage: TranslationStage,
        ctx: &TableContext,
    ) -> Result<(), TranslationError> {
        let translation =
            Translation::Unresolved(self.translation(action, origin, stage, ctx.swap_classes())?);
        let test = test.clone().add_implicit_replace();
        let lookback = match test.tests().first() {
            Some(TestInstruction::Lookback { len }) => usize::from(*len),
            _ => 0,
        };
        // for the multipass stages where we translate from braille to braille the character classes
        // need to be the dots_classes
        let character_classes = match stage {
            TranslationStage::Pre | TranslationStage::Main => ctx.character_classes(),
            TranslationStage::Post1 | TranslationStage::Post2 | TranslationStage::Post3 => {
                ctx.dots_classes()
            }
            // a display table is a plain character mapping, it has no context rules
            TranslationStage::Display => unreachable!(),
        };
        let whole_match = action.consumes_whole_match();
        let regexp = Regexp::from_test_in(&test, whole_match, character_classes, self.direction)
            .compile_with_payload(translation);
        self.regexps.push(Pattern {
            lookback,
            whole_match,
            regexp,
        });
        Ok(())
    }

    pub fn build(self) -> ContextPatterns {
        ContextPatterns {
            regexps: self.regexps,
        }
    }
}

#[derive(Debug)]
pub struct ContextPatterns {
    regexps: Vec<Pattern>,
}

impl ContextPatterns {
    /// The translations of the patterns that match at `input`, `before` being the text
    /// that precedes it. A test that starts with liblouis's `_N` is matched from `N`
    /// characters back, and replaces nothing before `input`.
    pub fn find(&self, before: &str, input: &str, env: &Environment) -> Vec<ResolvedTranslation> {
        let before_len = before.chars().count();
        self.regexps
            .iter()
            .flat_map(|p| {
                let n = p.lookback;
                if n == 0 {
                    return p.regexp.find_anchored(input, env, before.is_empty());
                }
                if n > before_len {
                    return None;
                }
                let (start, _) = before.char_indices().nth(before_len - n)?;
                let shifted = format!("{}{}", &before[start..], input);
                let t = p.regexp.find_anchored(&shifted, env, before_len == n)?;
                if p.whole_match {
                    let length = t.length().checked_sub(n)?;
                    Some(t.with_length(length))
                } else {
                    (t.offset() >= n).then(|| t.decrement_offset(n))
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Precedence;
    use crate::parser::RuleParser;
    use crate::parser::multipass::action;
    use crate::parser::multipass::test;

    fn context(class: CharacterClass, chars: &[char]) -> CharacterClasses {
        let mut ctx = CharacterClasses::default();
        for ch in chars {
            ctx.insert(class.clone(), *ch);
        }
        ctx
    }

    #[test]
    fn find_string() {
        let env = Environment::new();
        let tests = test::Parser::new("\"abc\"").tests().unwrap();
        let stage = TranslationStage::Main;
        let ctx = CharacterClasses::default();
        let re = Regexp::from_test(&tests, true, &ctx).compile();
        assert_eq!(
            re.find("abc", &env).unwrap(),
            ResolvedTranslation::new("", "", 3, stage, None).with_length(3)
        );
        assert_eq!(re.find("def", &env), None);
    }

    #[test]
    fn find_attribute_digit() {
        let env = Environment::new();
        let tests = test::Parser::new("$d").tests().unwrap();
        let stage = TranslationStage::Main;
        let ctx = context(CharacterClass::Digit, &['1', '2', '3']);
        let translation = Translation::Unresolved(UnresolvedTranslation::new(
            &[TranslationTarget::Literal("".to_string())],
            Precedence::Default,
            stage,
            &[],
            None,
        ));
        let regexp = Regexp::from_test(&tests, true, &ctx);
        let re = regexp.compile_with_payload(translation);
        assert_eq!(
            re.find("1", &env).unwrap(),
            ResolvedTranslation::new("", "", 1, stage, None).with_length(1)
        );
        assert_eq!(
            re.find("2", &env).unwrap(),
            ResolvedTranslation::new("", "", 1, stage, None).with_length(1)
        );
        assert_eq!(
            re.find("3", &env).unwrap(),
            ResolvedTranslation::new("", "", 1, stage, None).with_length(1)
        );
        assert_eq!(re.find("def", &env), None);
    }

    /// `$a` is liblouis' "any character": it matches whatever is there, even with no
    /// character classes defined at all
    #[test]
    fn find_attribute_any() {
        let env = Environment::new();
        let tests = test::Parser::new("$a").tests().unwrap();
        let stage = TranslationStage::Main;
        let ctx = CharacterClasses::default();
        let re = Regexp::from_test(&tests, true, &ctx).compile();
        assert_eq!(
            re.find("x", &env).unwrap(),
            ResolvedTranslation::new("", "", 1, stage, None).with_length(1)
        );
        assert_eq!(re.find("", &env), None);
    }

    /// liblouis takes as many characters as a quantified attribute test allows and never
    /// gives any back to the tests after it
    #[test]
    fn quantified_attribute_is_possessive() {
        let env = Environment::new();
        let ctx = context(CharacterClass::Digit, &['1', '2', '3']);
        let re =
            Regexp::from_test(&test::Parser::new("$d.$d").tests().unwrap(), true, &ctx).compile();
        assert!(!re.is_match("123", &env));
        let re =
            Regexp::from_test(&test::Parser::new("$d1-2$d").tests().unwrap(), true, &ctx).compile();
        assert!(!re.is_match("12", &env));
        assert!(re.is_match("123", &env));
    }

    /// In back-translation a negated attribute test only inverts the outcome, so it
    /// consumes nothing when the character doesn't match
    #[test]
    fn negated_attribute_backward_is_zero_width() {
        let env = Environment::new();
        let mut ctx = context(CharacterClass::Digit, &['1']);
        ctx.insert(CharacterClass::Letter, 'a');
        let tests = test::Parser::new("!$d$l").tests().unwrap();
        let backward = Regexp::from_test_in(&tests, true, &ctx, Direction::Backward).compile();
        assert!(backward.is_match("a", &env));
        assert!(!backward.is_match("1a", &env));
        let forward = Regexp::from_test_in(&tests, true, &ctx, Direction::Forward).compile();
        assert!(!forward.is_match("a", &env));
        assert!(forward.is_match("aa", &env));
    }

    #[test]
    fn find_attribute_uppercase() {
        let env = Environment::new();
        let tests = test::Parser::new("$U").tests().unwrap();
        let stage = TranslationStage::Main;
        let ctx = context(CharacterClass::Uppercase, &['A', 'B', 'C']);
        let translation = Translation::Unresolved(UnresolvedTranslation::new(
            &[TranslationTarget::Literal("".to_string())],
            Precedence::Default,
            stage,
            &[],
            None,
        ));
        let regexp = Regexp::from_test(&tests, true, &ctx);
        let re = regexp.compile_with_payload(translation);
        assert_eq!(
            re.find("A", &env).unwrap(),
            ResolvedTranslation::new("", "", 1, stage, None).with_length(1)
        );
        assert_eq!(
            re.find("A", &env).unwrap(),
            ResolvedTranslation::new("", "", 1, stage, None).with_length(1)
        );
        assert_eq!(
            re.find("C", &env).unwrap(),
            ResolvedTranslation::new("", "", 1, stage, None).with_length(1)
        );
        assert_eq!(re.find("def", &env), None);
    }

    #[test]
    fn find_attribute_uppercase_punctuation_or_sign() {
        let env = Environment::new();
        let tests = test::Parser::new("$USp").tests().unwrap();
        let mut ctx = CharacterClasses::default();
        let stage = TranslationStage::Main;
        for c in ['A', 'B', 'C'] {
            ctx.insert(CharacterClass::Uppercase, c);
        }
        for c in ['.', ',', '!'] {
            ctx.insert(CharacterClass::Punctuation, c);
        }
        for c in ['%', '&', '/'] {
            ctx.insert(CharacterClass::Sign, c);
        }
        let translation = Translation::Unresolved(UnresolvedTranslation::new(
            &[TranslationTarget::Literal("".to_string())],
            Precedence::Default,
            stage,
            &[],
            None,
        ));
        let regexp = Regexp::from_test(&tests, true, &ctx);
        let re = regexp.compile_with_payload(translation);
        assert_eq!(
            re.find("%", &env).unwrap(),
            ResolvedTranslation::new("", "", 1, stage, None).with_length(1)
        );
        assert_eq!(
            re.find(".", &env).unwrap(),
            ResolvedTranslation::new("", "", 1, stage, None).with_length(1)
        );
        assert_eq!(
            re.find("A", &env).unwrap(),
            ResolvedTranslation::new("", "", 1, stage, None).with_length(1)
        );
        assert_eq!(re.find("def", &env), None);
    }

    #[test]
    fn find_character_class() {
        let env = Environment::new();
        let tests = test::Parser::new("%letter").tests().unwrap();
        let stage = TranslationStage::Main;
        let ctx = context(CharacterClass::Letter, &['a', 'b', 'c']);
        let translation = Translation::Unresolved(UnresolvedTranslation::new(
            &[TranslationTarget::Literal("".to_string())],
            Precedence::Default,
            stage,
            &[],
            None,
        ));
        let regexp = Regexp::from_test(&tests, true, &ctx);
        let re = regexp.compile_with_payload(translation);
        assert_eq!(
            re.find("a", &env).unwrap(),
            ResolvedTranslation::new("", "", 1, stage, None).with_length(1)
        );
        assert_eq!(
            re.find("b", &env).unwrap(),
            ResolvedTranslation::new("", "", 1, stage, None).with_length(1)
        );
        assert_eq!(
            re.find("c", &env).unwrap(),
            ResolvedTranslation::new("", "", 1, stage, None).with_length(1)
        );
        assert_eq!(re.find("def", &env), None);
    }

    #[test]
    fn find_character_class_three() {
        let env = Environment::new();
        let tests = test::Parser::new("%letter3").tests().unwrap();
        let stage = TranslationStage::Main;
        let ctx = context(CharacterClass::Letter, &['a', 'b', 'c']);
        let translation = Translation::Unresolved(UnresolvedTranslation::new(
            &[TranslationTarget::Literal("".to_string())],
            Precedence::Default,
            stage,
            &[],
            None,
        ));
        let regexp = Regexp::from_test(&tests, true, &ctx);
        let re = regexp.compile_with_payload(translation);
        assert_eq!(
            re.find("abc", &env).unwrap(),
            ResolvedTranslation::new("", "", 3, stage, None).with_length(3)
        );
        assert_eq!(
            re.find("bbb", &env).unwrap(),
            ResolvedTranslation::new("", "", 3, stage, None).with_length(3)
        );
        assert_eq!(
            re.find("ccc", &env).unwrap(),
            ResolvedTranslation::new("", "", 3, stage, None).with_length(3)
        );
        assert_eq!(re.find("a", &env), None);
        assert_eq!(re.find("aa", &env), None);
        assert_eq!(re.find("def", &env), None);
    }

    #[test]
    fn find_character_class_any() {
        let env = Environment::new();
        let tests = test::Parser::new("%letter.").tests().unwrap();
        let stage = TranslationStage::Main;
        let ctx = context(CharacterClass::Letter, &['a', 'b', 'c']);
        let translation = Translation::Unresolved(UnresolvedTranslation::new(
            &[TranslationTarget::Literal("".to_string())],
            Precedence::Default,
            stage,
            &[],
            None,
        ));
        let regexp = Regexp::from_test(&tests, true, &ctx);
        let re = regexp.compile_with_payload(translation);
        assert_eq!(
            re.find("a", &env).unwrap(),
            ResolvedTranslation::new("", "", 1, stage, None).with_length(1)
        );
        assert_eq!(
            re.find("b", &env).unwrap(),
            ResolvedTranslation::new("", "", 1, stage, None).with_length(1)
        );
        assert_eq!(
            re.find("c", &env).unwrap(),
            ResolvedTranslation::new("", "", 1, stage, None).with_length(1)
        );
        assert_eq!(re.find("y", &env), None);
        assert_eq!(
            re.find("bbb", &env).unwrap(),
            ResolvedTranslation::new("", "", 3, stage, None).with_length(3)
        );
        assert_eq!(
            re.find("ccc", &env).unwrap(),
            ResolvedTranslation::new("", "", 3, stage, None).with_length(3)
        );
        assert_eq!(re.find("def", &env), None);
    }

    #[test]
    fn find_capture_with_character_class() {
        let env = Environment::new();
        let tests = test::Parser::new("\"a\"[%digit]\"b\"").tests().unwrap();
        let ctx = context(CharacterClass::Digit, &['1', '2', '3']);
        let translation = Translation::Unresolved(UnresolvedTranslation::new(
            &[TranslationTarget::Literal("".to_string())],
            Precedence::Default,
            TranslationStage::Main,
            &[],
            None,
        ));
        let regexp = Regexp::from_test(&tests, true, &ctx);
        let re = regexp.compile_with_payload(translation);
        assert_eq!(
            re.find("a1b", &env).unwrap(),
            ResolvedTranslation::new("1", "", 3, TranslationStage::Main, None).with_length(3)
        );
        assert_eq!(
            re.find("a2b", &env).unwrap(),
            ResolvedTranslation::new("2", "", 3, TranslationStage::Main, None).with_length(3)
        );
        assert_eq!(
            re.find("a3b", &env).unwrap(),
            ResolvedTranslation::new("3", "", 3, TranslationStage::Main, None).with_length(3)
        );
        assert_eq!(re.find("bbb", &env), None);
        assert_eq!(re.find("ccc", &env), None);
        assert_eq!(re.find("a", &env), None);
        assert_eq!(re.find("aa", &env), None);
        assert_eq!(re.find("def", &env), None);
    }

    #[test]
    fn find_capture_with_attribute() {
        let env = Environment::new();
        let tests = test::Parser::new("\"a\"[$dU]\"b\"").tests().unwrap();
        let mut ctx = context(CharacterClass::Digit, &['1', '2', '3']);
        for c in ['A', 'B', 'C'] {
            ctx.insert(CharacterClass::Uppercase, c);
        }
        let translation = Translation::Unresolved(UnresolvedTranslation::new(
            &[TranslationTarget::Literal("".to_string())],
            Precedence::Default,
            TranslationStage::Main,
            &[],
            None,
        ));
        let regexp = Regexp::from_test(&tests, true, &ctx);
        let re = regexp.compile_with_payload(translation);
        assert_eq!(
            re.find("a1b", &env).unwrap(),
            ResolvedTranslation::new("1", "", 3, TranslationStage::Main, None).with_length(3)
        );
        assert_eq!(
            re.find("a2b", &env).unwrap(),
            ResolvedTranslation::new("2", "", 3, TranslationStage::Main, None).with_length(3)
        );
        assert_eq!(
            re.find("a3b", &env).unwrap(),
            ResolvedTranslation::new("3", "", 3, TranslationStage::Main, None).with_length(3)
        );
        assert_eq!(
            re.find("aAb", &env).unwrap(),
            ResolvedTranslation::new("A", "", 3, TranslationStage::Main, None).with_length(3)
        );
        assert_eq!(
            re.find("aBb", &env).unwrap(),
            ResolvedTranslation::new("B", "", 3, TranslationStage::Main, None).with_length(3)
        );
        assert_eq!(
            re.find("aCb", &env).unwrap(),
            ResolvedTranslation::new("C", "", 3, TranslationStage::Main, None).with_length(3)
        );
        assert_eq!(re.find("bbb", &env), None);
        assert_eq!(re.find("ccc", &env), None);
        assert_eq!(re.find("a", &env), None);
        assert_eq!(re.find("aa", &env), None);
        assert_eq!(re.find("def", &env), None);
    }

    #[test]
    fn find_capture_with_attribute_and_quantifier() {
        let env = Environment::new();
        let tests = test::Parser::new("\"a\"[$dU3]\"b\"").tests().unwrap();
        let mut ctx = context(CharacterClass::Digit, &['1', '2', '3']);
        for c in ['A', 'B', 'C'] {
            ctx.insert(CharacterClass::Uppercase, c);
        }
        let translation = Translation::Unresolved(UnresolvedTranslation::new(
            &[TranslationTarget::Literal("".to_string())],
            Precedence::Default,
            TranslationStage::Main,
            &[],
            None,
        ));
        let regexp = Regexp::from_test(&tests, true, &ctx);
        let re = regexp.compile_with_payload(translation);
        assert_eq!(re.find("a1b", &env), None);
        assert_eq!(re.find("a22b", &env), None);
        assert_eq!(re.find("a31b", &env), None);
        assert_eq!(
            re.find("a123b", &env).unwrap(),
            ResolvedTranslation::new("123", "", 5, TranslationStage::Main, None).with_length(5)
        );
        assert_eq!(
            re.find("a222b", &env).unwrap(),
            ResolvedTranslation::new("222", "", 5, TranslationStage::Main, None).with_length(5)
        );
        assert_eq!(
            re.find("a321b", &env).unwrap(),
            ResolvedTranslation::new("321", "", 5, TranslationStage::Main, None).with_length(5)
        );
        assert_eq!(
            re.find("aABCb", &env).unwrap(),
            ResolvedTranslation::new("ABC", "", 5, TranslationStage::Main, None).with_length(5)
        );
        assert_eq!(
            re.find("aBBBb", &env).unwrap(),
            ResolvedTranslation::new("BBB", "", 5, TranslationStage::Main, None).with_length(5)
        );
        assert_eq!(
            re.find("aCBAb", &env).unwrap(),
            ResolvedTranslation::new("CBA", "", 5, TranslationStage::Main, None).with_length(5)
        );
        assert_eq!(re.find("bbb", &env), None);
        assert_eq!(re.find("ccc", &env), None);
        assert_eq!(re.find("a", &env), None);
        assert_eq!(re.find("aa", &env), None);
        assert_eq!(re.find("def", &env), None);
    }

    // just create some fake anchored rule for testing purposes
    fn origin(line: &str) -> AnchoredRule {
        let rule = RuleParser::new(line).rule().unwrap();
        AnchoredRule::new(rule, None, 0)
    }

    #[test]
    fn context_simple() {
        let env = Environment::new();
        let tests = test::Parser::new("\"abc\"").tests().unwrap();
        let action = action::Parser::new("\"A_B_C\"").actions().unwrap();
        let origin = origin("context \"abc\" \"A_B_C\"");
        let stage = TranslationStage::Main;
        let mut builder = ContextPatternsBuilder::new();
        builder
            .insert(&tests, &action, &origin, stage, &TableContext::default())
            .unwrap();
        let translation =
            ResolvedTranslation::new("abc", "A_B_C", 3, TranslationStage::Main, origin.clone());
        let patterns = builder.build();
        assert_eq!(patterns.find("", "abc", &env), [translation]);
        assert_eq!(patterns.find("", "def", &env), []);
    }

    #[test]
    fn context_capture() {
        let env = Environment::new();
        let tests = test::Parser::new(r#"["abc"]"#).tests().unwrap();
        let action = action::Parser::new(r#""<"*">""#).actions().unwrap();
        let origin = origin(r#"context "abc" "<"*">""#);
        let stage = TranslationStage::Main;
        let mut builder = ContextPatternsBuilder::new();
        builder
            .insert(&tests, &action, &origin, stage, &TableContext::default())
            .unwrap();
        let translation =
            ResolvedTranslation::new("abc", "<abc>", 3, TranslationStage::Main, origin.clone());
        let patterns = builder.build();
        assert_eq!(patterns.find("", "abc", &env), [translation]);
        assert!(patterns.find("", "def", &env).is_empty());
    }

    #[test]
    fn context_at_beginning_anchor() {
        let env = Environment::new();
        let tests = test::Parser::new(r#"`"a""#).tests().unwrap();
        let action = action::Parser::new(r#""X""#).actions().unwrap();
        let origin = origin(r#"context `"a" "X""#);
        let stage = TranslationStage::Main;
        let mut builder = ContextPatternsBuilder::new();
        builder
            .insert(&tests, &action, &origin, stage, &TableContext::default())
            .unwrap();
        let translation =
            ResolvedTranslation::new("a", "X", 1, TranslationStage::Main, origin.clone());
        let patterns = builder.build();
        // matches when `input` truly starts at position 0 of the whole string being translated
        assert_eq!(patterns.find("", "a", &env), [translation]);
        // does not match when we are mid-string, even though the substring looks identical
        assert!(patterns.find("x", "a", &env).is_empty());
    }

    #[test]
    fn context_at_end_anchor() {
        let env = Environment::new();
        let tests = test::Parser::new(r#""a"~"#).tests().unwrap();
        let action = action::Parser::new(r#""X""#).actions().unwrap();
        let origin = origin(r#"context "a"~ "X""#);
        let stage = TranslationStage::Main;
        let mut builder = ContextPatternsBuilder::new();
        builder
            .insert(&tests, &action, &origin, stage, &TableContext::default())
            .unwrap();
        let translation =
            ResolvedTranslation::new("a", "X", 1, TranslationStage::Main, origin.clone());
        let patterns = builder.build();
        // matches when "a" is exactly the whole remaining input
        assert_eq!(patterns.find("", "a", &env), [translation]);
        // does not match when there is trailing input after "a"
        assert!(patterns.find("", "ab", &env).is_empty());
    }

    #[test]
    fn context_at_beginning_anchor_after_other_test() {
        // liblouis/liblouis#2071: `quotation-marks.uti:26` places `` ` `` after `#1=0` rather
        // than first. The manual says it must be first, but the compiler doesn't enforce that,
        // and both `` ` `` and `#1=0` are zero-width test-phase conditions, so this must behave
        // identically to `context_at_beginning_anchor` above.
        let mut env = Environment::new();
        env.apply(&Effect::new(1, 0));
        let tests = test::Parser::new(r#"#1=0`"a""#).tests().unwrap();
        let action = action::Parser::new(r#""X""#).actions().unwrap();
        let origin = origin(r#"context #1=0`"a" "X""#);
        let stage = TranslationStage::Main;
        let mut builder = ContextPatternsBuilder::new();
        builder
            .insert(&tests, &action, &origin, stage, &TableContext::default())
            .unwrap();
        let translation =
            ResolvedTranslation::new("a", "X", 1, TranslationStage::Main, origin.clone());
        let patterns = builder.build();
        assert_eq!(patterns.find("", "a", &env), [translation]);
        assert!(patterns.find("x", "a", &env).is_empty());
    }

    #[test]
    fn context_negated_beginning_and_end_anchor() {
        let env = Environment::new();
        let tests = test::Parser::new(r#"!`"a""#).tests().unwrap();
        let action = action::Parser::new(r#""X""#).actions().unwrap();
        let origin = origin(r#"context !`"a" "X""#);
        let stage = TranslationStage::Main;
        let mut builder = ContextPatternsBuilder::new();
        builder
            .insert(&tests, &action, &origin, stage, &TableContext::default())
            .unwrap();
        let translation =
            ResolvedTranslation::new("a", "X", 1, TranslationStage::Main, origin.clone());
        let patterns = builder.build();
        // "not at the beginning" -- opposite of `context_at_beginning_anchor`
        assert!(patterns.find("", "a", &env).is_empty());
        assert_eq!(patterns.find("x", "a", &env), [translation]);
    }

    #[test]
    fn context_lookback_checks_the_preceding_text() {
        // liblouis' `_1$d[...]` rewinds 1 and checks that the preceding character is a digit
        let env = Environment::new();
        let tests = test::Parser::new(r#"_1$d["b"]"#).tests().unwrap();
        let action = action::Parser::new(r#""X""#).actions().unwrap();
        let origin = origin(r#"context _1$d["b"] "X""#);
        let stage = TranslationStage::Main;
        let ctx = TableContext::new(
            CharacterClasses::new(&[(CharacterClass::Digit, &['a'])]),
            CharacterClasses::default(),
            SwapClasses::default(),
        );
        let mut builder = ContextPatternsBuilder::new();
        builder
            .insert(&tests, &action, &origin, stage, &ctx)
            .unwrap();
        let patterns = builder.build();
        // "a" is a digit here, so the rule fires on "b" after it; `weight()` reflects the
        // whole span that was checked ("a" + "b")
        let translation = ResolvedTranslation::new("b", "X", 2, stage, origin.clone());
        assert_eq!(patterns.find("a", "b", &env), [translation]);
        // does not fire when the preceding character isn't a digit, or there is none
        assert!(patterns.find("b", "b", &env).is_empty());
        assert!(patterns.find("", "ab", &env).is_empty());
    }

    #[test]
    fn context_lookback_after_bracket_is_ignored_not_consumed() {
        // liblouis' `[...]!$d_` checks the trailing character then rewinds so it isn't counted
        // as part of the consumed span. We drop the trailing `_` and just check `!$d` in place —
        // the reported length is always the bracket's own length (see
        // `ResolvedTranslation::resolve`), never however much trailing context was checked, so
        // the checked-but-not-bracketed character is correctly left for the next position to
        // translate on its own.
        let env = Environment::new();
        let tests = test::Parser::new(r#"["b"]!$d_"#).tests().unwrap();
        let action = action::Parser::new(r#""X""#).actions().unwrap();
        let origin = origin(r#"context ["b"]!$d_ "X""#);
        let stage = TranslationStage::Main;
        let ctx = TableContext::new(
            CharacterClasses::new(&[(CharacterClass::Digit, &['1'])]),
            CharacterClasses::default(),
            SwapClasses::default(),
        );
        let mut builder = ContextPatternsBuilder::new();
        builder
            .insert(&tests, &action, &origin, stage, &ctx)
            .unwrap();
        let patterns = builder.build();
        // matches, and only consumes "b" (`length()` 1), when followed by a non-digit; `weight()`
        // is 2, reflecting the whole "b"+"z" span that was checked
        let translation = ResolvedTranslation::new("b", "X", 2, stage, origin.clone());
        assert_eq!(patterns.find("", "bz", &env), [translation]);
        // does not fire when followed by a digit
        assert!(patterns.find("", "b1", &env).is_empty());
    }

    #[test]
    fn context_beginning_and_end_anchor_regression() {
        // Regression test for da-dk-g26.ctb's `["-"]~ @36-36: a hyphen that is both anchored to
        // the beginning and the end of input (i.e. the whole input is exactly "-") must not match
        // when there is trailing text after the hyphen, e.g. "-0". Before this fix, the `~`
        // anchor was silently ignored, so this rule matched "-0" and doubled the hyphen cell.
        let env = Environment::new();
        let tests = test::Parser::new(r#"`["-"]~"#).tests().unwrap();
        let action = action::Parser::new("*").actions().unwrap();
        let origin = origin(r#"context `["-"]~ *"#);
        let stage = TranslationStage::Main;
        let mut builder = ContextPatternsBuilder::new();
        builder
            .insert(&tests, &action, &origin, stage, &TableContext::default())
            .unwrap();
        let translation =
            ResolvedTranslation::new("-", "-", 1, TranslationStage::Main, origin.clone());
        let patterns = builder.build();
        assert_eq!(patterns.find("", "-", &env), [translation]);
        assert!(patterns.find("", "-0", &env).is_empty());
        assert!(patterns.find("x", "-", &env).is_empty());
    }

    #[test]
    fn context_capture_with_class() {
        let env = Environment::new();
        let tests = test::Parser::new("@135[%accent]@13456").tests().unwrap();
        let action = action::Parser::new("*@136").actions().unwrap();
        let origin = origin("pass2 @135[%accent]@13456 *@136");
        let stage = TranslationStage::Post1;
        let ctx = TableContext::new(
            CharacterClasses::new(&[(CharacterClass::UserDefined("accent".to_string()), &['΄'])]),
            CharacterClasses::new(&[(CharacterClass::UserDefined("accent".to_string()), &['⠐'])]),
            SwapClasses::default(),
        );
        let mut builder = ContextPatternsBuilder::new();
        builder
            .insert(&tests, &action, &origin, stage, &ctx)
            .unwrap();
        let translation =
            // `*@136` copies the bracket and advances to `endMatch`, so all three cells go
            ResolvedTranslation::new("⠐", "⠐⠥", 3, TranslationStage::Post1, origin.clone())
	    .with_length(3);
        let patterns = builder.build();
        assert_eq!(patterns.find("", "⠕⠐⠽", &env), [translation]);
        assert!(patterns.find("", "⠕", &env).is_empty());
    }

    #[test]
    fn context_capture_advanced() {
        let env = Environment::new();
        let tests = test::Parser::new(r#"$p3["abc"]$p3"#).tests().unwrap();
        let action = action::Parser::new(r#""<"*">""#).actions().unwrap();
        let context = TableContext::new(
            CharacterClasses::new(&[(CharacterClass::Punctuation, &['{', '}'])]),
            CharacterClasses::default(),
            SwapClasses::default(),
        );
        let origin = origin(r#"context $p3"abc"$p3 "<"*">""#);
        let stage = TranslationStage::Main;
        let mut builder = ContextPatternsBuilder::new();
        builder
            .insert(&tests, &action, &origin, stage, &context)
            .unwrap();
        let translation =
            // `"<"*">"` copies the bracket and advances to `endMatch`, eating both `$p3`s
            ResolvedTranslation::new("abc", "<abc>", 9, TranslationStage::Main, origin.clone())
                .with_length(9);
        let patterns = builder.build();
        assert_eq!(patterns.find("", "{{{abc}}}", &env), [translation]);
        assert!(patterns.find("", "def", &env).is_empty());
    }

    #[test]
    fn context_swap() {
        let env = Environment::new();
        let tests = test::Parser::new(r#"$p3["abc"]$p3"#).tests().unwrap();
        let action = action::Parser::new(r#""<"%foo">""#).actions().unwrap();
        let context = TableContext::new(
            CharacterClasses::new(&[(CharacterClass::Punctuation, &['{', '}'])]),
            CharacterClasses::default(),
            SwapClasses::new(&[("foo", &[('a', "A"), ('b', "B"), ('c', "C")])]),
        );
        let origin = origin(r#"context $p3"abc"$p3 "<"%foo">""#);
        let stage = TranslationStage::Main;
        let mut builder = ContextPatternsBuilder::new();
        builder
            .insert(&tests, &action, &origin, stage, &context)
            .unwrap();
        let translation =
            ResolvedTranslation::new("abc", "<ABC>", 9, TranslationStage::Main, origin.clone())
                .with_offset(3)
                .with_length(3);
        let patterns = builder.build();
        assert_eq!(patterns.find("", "{{{abc}}}", &env), [translation]);
        assert!(patterns.find("", "def", &env).is_empty());
    }
}
