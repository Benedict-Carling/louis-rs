use crate::translator::ResolvedTranslation;
use crate::translator::context_pattern::{ContextPatterns, ContextPatternsBuilder};
use crate::translator::effect::Environment;
use crate::translator::table::{TableContext, backward};
use crate::{
    Direction,
    parser::{AnchoredRule, Rule},
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

fn test_of(translation: &ResolvedTranslation) -> Option<&crate::parser::Test> {
    match translation.rule() {
        Some(
            Rule::Correct { test, .. }
            | Rule::Pass2 { test, .. }
            | Rule::Pass3 { test, .. }
            | Rule::Pass4 { test, .. },
        ) => Some(test),
        _ => None,
    }
}

fn lookback(translation: &ResolvedTranslation) -> usize {
    test_of(translation).map_or(0, |test| test.lookbehind().1)
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

    pub fn trace(&self, input: &str) -> Vec<ResolvedTranslation> {
        let mut translations: Vec<ResolvedTranslation> = Vec::new();
        let mut env = Environment::new();
        let mut chars = input.chars();
        // liblouis copies the next character unchanged after a rule that did not advance
        let mut advanced = true;
        // a rule matched from an earlier position, whose replacement is still ahead
        let mut pending: Option<ResolvedTranslation> = None;
        // rules found before what their lookback steps over, with the characters left
        // until liblouis would start matching them
        let mut behind: Vec<(usize, ResolvedTranslation)> = Vec::new();

        loop {
            let from_before = pending.is_some();
            let candidate = match pending.take() {
                Some(t) if t.offset() > 0 => {
                    pending = Some(t.decrement_offset(1));
                    None
                }
                Some(t) => Some(t),
                None if advanced => {
                    let at_start = chars.as_str().len() == input.len();
                    for t in self.patterns.find(chars.as_str(), &env, at_start) {
                        behind.push((lookback(&t), t));
                    }
                    let (due, waiting): (Vec<_>, Vec<_>) =
                        behind.drain(..).partition(|(left, _)| *left == 0);
                    behind = waiting;
                    // liblouis tries pass rules by the characters they are filed by, longest
                    // first, and takes the first that matches from this position
                    let best = due
                        .into_iter()
                        .map(|(_, t)| t)
                        .max_by_key(|t| (t.rule().map_or(0, backward::pass_rule_cells), t.rank()));
                    match best {
                        Some(t) if t.offset() > 0 => {
                            pending = Some(t.decrement_offset(1));
                            None
                        }
                        t => t,
                    }
                }
                None => None,
            };
            advanced = true;
            let consumed = if let Some(t) = candidate {
                if t.length() == 0 {
                    // a rule whose test began earlier has advanced past where it began
                    advanced = from_before;
                } else {
                    // move the iterator forward by the number of characters in the translation
                    chars.nth(t.length() - 1);
                }
                for effect in t.effects() {
                    env.apply(effect);
                }
                let length = t.length();
                translations.push(t);
                length
            } else if let Some(next_char) = chars.next() {
                // no translation rule found, just pass the character through
                translations.push(ResolvedTranslation::new(
                    &next_char.to_string(),
                    &next_char.to_string(),
                    1,
                    self.stage,
                    None,
                ));
                1
            } else {
                // the chars iterator is exhausted
                break;
            };
            behind = behind
                .into_iter()
                .filter(|(left, _)| *left >= consumed)
                .map(|(left, t)| (left - consumed, t.decrement_offset(consumed)))
                .collect();
        }
        translations
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
    fn a_rule_that_began_earlier_lets_the_next_one_start_where_it_ended() {
        let rules = [
            parse_rule("noback pass2 @1[]@2 @3"),
            parse_rule("noback pass2 @2[]@4 @5"),
        ];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Post1, &ctx)
                .unwrap();
        assert_eq!(transform.translate("⠁⠂⠈"), "⠁⠄⠂⠐⠈");
    }

    #[test]
    fn copy_rule_keeps_what_its_lookback_steps_over() {
        let rules = [
            parse_rule("noback pass2 _@15[@12] *"),
            parse_rule("noback pass2 @15[@14] *"),
            parse_rule("noback pass2 @12 @2"),
        ];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Post1, &ctx)
                .unwrap();
        assert_eq!(transform.translate("⠑⠃"), "⠑⠃");
        assert_eq!(transform.translate("⠑⠉"), "⠉");
        assert_eq!(transform.translate("⠃"), "⠂");
    }

    #[test]
    fn backward_copy_rule_keeps_its_match_from_other_rules() {
        let rules = [
            parse_rule("nofor pass2 @2357 @f-235"),
            parse_rule("nofor pass2 @6-2357 *"),
        ];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Backward, TranslationStage::Post1, &ctx)
                .unwrap();
        assert_eq!(transform.translate("⠠⡖"), "⠠⡖");
        assert_eq!(transform.translate("⡖"), "\u{f4000}⠖");
    }

    #[test]
    fn rules_starting_with_more_literal_cells_are_tried_first() {
        let rules = [
            parse_rule("noback pass2 @1-2[@3]@4 @5"),
            parse_rule("noback pass2 @1[@3]@4 @6"),
        ];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Post1, &ctx)
                .unwrap();
        assert_eq!(transform.translate("⠁⠂⠄⠈"), "⠁⠂⠐⠈");
    }

    #[test]
    fn a_rule_whose_test_began_earlier_comes_first() {
        let rules = [
            parse_rule("noback pass2 @1[@2] @4"),
            parse_rule("noback pass2 [@2-2] @5"),
        ];
        let ctx = TableContext::default();
        let transform =
            MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Post1, &ctx)
                .unwrap();
        assert_eq!(transform.translate("⠁⠂⠂"), "⠁⠈⠂");
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
