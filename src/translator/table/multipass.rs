use crate::translator::ResolvedTranslation;
use crate::translator::context_pattern::{ContextPatterns, ContextPatternsBuilder};
use crate::translator::effect::Environment;
use crate::translator::table::TableContext;
use crate::{
    Direction,
    parser::{AnchoredRule, Rule, TestInstruction},
    translator::{TranslationError, TranslationStage},
};

#[derive(Debug)]
pub struct MultipassTable {
    patterns: ContextPatterns,
    stage: TranslationStage,
    direction: Direction,
}

/// A builder for [`MultipassTable`]
#[derive(Debug)]
struct MultipassTableBuilder {
    patterns: ContextPatternsBuilder,
}

impl MultipassTableBuilder {
    fn new(direction: Direction) -> Self {
        Self {
            patterns: ContextPatternsBuilder::new().for_direction(direction),
        }
    }

    fn build(self, direction: Direction, stage: TranslationStage) -> MultipassTable {
        MultipassTable {
            direction,
            stage,
            patterns: self.patterns.build(),
        }
    }
}

impl MultipassTable {
    pub fn compile(
        rules: &[AnchoredRule],
        direction: Direction,
        stage: TranslationStage,
        ctx: &TableContext,
    ) -> Result<Self, TranslationError> {
        let mut builder = MultipassTableBuilder::new(direction);

        for rule in rules {
            match &rule.rule {
                Rule::Correct { test, action, .. }
                | Rule::Pass2 { test, action, .. }
                | Rule::Pass3 { test, action, .. }
                | Rule::Pass4 { test, action, .. } => {
                    builder.patterns.insert(test, action, rule, stage, ctx)?;
                }
                _ => (),
            }
        }

        Ok(builder.build(direction, stage))
    }

    pub fn translate(&self, input: &str) -> String {
        self.trace(input).iter().map(|t| t.output()).collect()
    }

    /// liblouis's `translatePass`: at each position the rule whose test starts there with the
    /// most literal characters applies; the characters before its replacement are copied
    pub fn trace(&self, input: &str) -> Vec<ResolvedTranslation> {
        let mut translations: Vec<ResolvedTranslation> = Vec::new();
        // rules found with a leading lookback, and how far ahead their test starts
        let mut looking_back: Vec<(usize, ResolvedTranslation)> = Vec::new();
        // the rule that applies once the characters before its replacement are copied
        let mut forced: Option<ResolvedTranslation> = None;
        let mut env = Environment::new();
        let mut chars = input.chars();
        // liblouis copies the next character unchanged after a rule that did not advance
        let mut advanced = true;
        let mut copied = false;

        loop {
            if forced.is_none() {
                let at_start = chars.as_str().len() == input.len();
                let (mut candidates, later): (Vec<_>, Vec<_>) = self
                    .patterns
                    .find(chars.as_str(), &env, at_start)
                    .into_iter()
                    .map(|t| (leading_lookback(&t).min(t.offset()), t))
                    .partition(|(start, _)| *start == 0);
                looking_back.extend(later);
                let (due, later): (Vec<_>, Vec<_>) =
                    looking_back.drain(..).partition(|(start, _)| *start == 0);
                looking_back = later;
                candidates.extend(due);
                copied = false;
                if advanced {
                    forced = candidates
                        .into_iter()
                        .map(|(_, t)| t)
                        .max_by_key(|t| (literal_prefix(t), t.rank()));
                }
            }
            advanced = true;
            let step = match forced.take_if(|t| t.offset() == 0) {
                Some(t) => {
                    // a rule whose test started at an earlier position has advanced
                    if t.length() == 0 && !copied {
                        advanced = false;
                    } else if t.length() > 0 {
                        chars.nth(t.length() - 1);
                    }
                    for effect in t.effects() {
                        env.apply(effect);
                    }
                    translations.push(t.clone());
                    t.length()
                }
                None => match chars.next() {
                    Some(next_char) => {
                        // no rule applies here, or one applies further on: copy the character
                        translations.push(ResolvedTranslation::new(
                            &next_char.to_string(),
                            &next_char.to_string(),
                            1,
                            self.stage,
                            None,
                        ));
                        forced = forced.map(|t| t.decrement_offset(1));
                        copied = true;
                        1
                    }
                    None => break,
                },
            };
            looking_back = looking_back
                .into_iter()
                .filter(|(start, _)| *start >= step)
                .map(|(start, t)| (start - step, t.decrement_offset(step)))
                .collect();
        }
        translations
    }
}

/// How far back the test's leading lookback reaches before the rule's position
fn leading_lookback(translation: &ResolvedTranslation) -> usize {
    match translation.rule() {
        Some(
            Rule::Correct { test, .. }
            | Rule::Pass2 { test, .. }
            | Rule::Pass3 { test, .. }
            | Rule::Pass4 { test, .. },
        ) => test
            .tests()
            .iter()
            .map_while(|t| match t {
                TestInstruction::Lookback { len } => Some(*len as usize),
                _ => None,
            })
            .sum(),
        _ => 0,
    }
}

fn literal_prefix(translation: &ResolvedTranslation) -> usize {
    match translation.rule() {
        Some(
            Rule::Correct { test, .. }
            | Rule::Pass2 { test, .. }
            | Rule::Pass3 { test, .. }
            | Rule::Pass4 { test, .. },
        ) => test.literal_prefix(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::parser::RuleParser;

    fn parse_rule(source: &str) -> AnchoredRule {
        RuleParser::new(source).rule().unwrap().into()
    }

    #[test]
    fn correct() {
        let rules = [parse_rule("correct \"corect\" \"correct\"")];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Pre, &ctx)
                .unwrap();
        assert_eq!(transform.translate("foobar"), "foobar");
        assert_eq!(transform.translate("corect"), "correct");
        assert_eq!(transform.translate("🐂"), "🐂");
    }

    /// liblouis applies back-translation's `correct` rules like forward ones, so one whose
    /// action copies the match (`*`) keeps a shorter rule from rewriting it
    #[test]
    fn correct_backward_copy() {
        let rules = [
            parse_rule("nofor correct \"ssze\" \"szsze\""),
            parse_rule("nofor correct \"sszel\" *"),
        ];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Backward, TranslationStage::Pre, &ctx)
                .unwrap();
        assert_eq!(transform.translate("resszel"), "resszel");
        assert_eq!(transform.translate("ressze"), "reszsze");
    }

    #[test]
    fn pass2() {
        let rules = [parse_rule("pass2 @123 @12")];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Post1, &ctx)
                .unwrap();
        assert_eq!(transform.translate("⠇"), "⠃");
        assert_eq!(transform.translate("⠙"), "⠙");
        assert_eq!(transform.translate("🐂"), "🐂");
    }

    #[test]
    fn pass3() {
        let rules = [parse_rule("pass3 @123 @12")];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Post2, &ctx)
                .unwrap();
        assert_eq!(transform.translate("⠇"), "⠃");
        assert_eq!(transform.translate("⠙"), "⠙");
        assert_eq!(transform.translate("🐂"), "🐂");
    }

    #[test]
    fn pass4() {
        let rules = [parse_rule("pass4 @123 @12")];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Post3, &ctx)
                .unwrap();
        assert_eq!(transform.translate("⠇"), "⠃");
        assert_eq!(transform.translate("⠙"), "⠙");
        assert_eq!(transform.translate("🐂"), "🐂");
    }

    #[test]
    fn pass3_collapse_doubled_cell() {
        let rules = [parse_rule("noback pass3 @12-12 @12")];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Post2, &ctx)
                .unwrap();
        assert_eq!(transform.translate("⠃⠃"), "⠃");
        assert_eq!(transform.translate("⠃⠙⠃"), "⠃⠙⠃");
    }

    #[test]
    fn zero_length_rule_fires_again_after_the_next_character() {
        let rules = [parse_rule("pass2 []@1 @6")];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Post1, &ctx)
                .unwrap();
        assert_eq!(transform.translate("⠁⠁"), "⠠⠁⠠⠁");
        assert_eq!(transform.translate("⠁⠃⠁"), "⠠⠁⠃⠠⠁");
    }

    #[test]
    fn swap_class_test_in_a_post_pass() {
        let rules = [
            parse_rule("swapdd foo 1,12 4,45"),
            parse_rule("pass4 %foo %foo"),
        ];
        let ctx = TableContext::compile(&rules).unwrap();
        let transform =
            MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Post3, &ctx)
                .unwrap();
        assert_eq!(transform.translate("⠁⠃⠉"), "⠈⠘⠉");
    }

    #[test]
    fn unset_variable_is_zero() {
        let rules = [
            parse_rule("pass2 #1=0@1 @6#1=1"),
            parse_rule("pass2 !#2=1@2 @3"),
        ];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Post1, &ctx)
                .unwrap();
        assert_eq!(transform.translate("⠁⠁⠂"), "⠠⠁⠄");
    }

    #[test]
    fn literal_prefix_ranks_first() {
        let rules = [parse_rule("pass2 $a @4"), parse_rule("pass2 @1 @2")];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Post1, &ctx)
                .unwrap();
        assert_eq!(transform.translate("⠁"), "⠂");
    }

    #[test]
    fn rule_starting_earlier_applies_first() {
        let rules = [parse_rule("pass2 $a[@12] @4"), parse_rule("pass2 @12 @2")];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Post1, &ctx)
                .unwrap();
        assert_eq!(transform.translate("⠁⠃"), "⠁⠈");
    }

    #[test]
    fn zero_length_rule_begun_earlier_advances() {
        let rules = [
            parse_rule("pass2 $a[]@12 @4"),
            parse_rule("pass2 $a[]@14 @5"),
        ];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Post1, &ctx)
                .unwrap();
        assert_eq!(transform.translate("⠁⠃⠉"), "⠁⠈⠃⠐⠉");
    }

    #[test]
    fn effects() {
        let rules = [
            // when encountering @123 set var 1 to 1
            parse_rule("pass2 @123 @123#1=1"),
            // while var 1 is 1 transform @1 to @6
            parse_rule("pass2 #1=1@1 @6"),
            // when encountering @456 set var 1 back to 0
            parse_rule("pass2 @456 @456#1=0"),
        ];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Post1, &ctx)
                .unwrap();
        assert_eq!(transform.translate("⠁⠁⠇⠁⠁⠸⠁⠁"), "⠁⠁⠇⠠⠠⠸⠁⠁");
        assert_eq!(transform.translate("🐂"), "🐂");
    }
}
