use crate::translator::ResolvedTranslation;
use crate::translator::pass::PassRules;
use crate::translator::table::TableContext;
use crate::{
    Direction,
    parser::AnchoredRule,
    translator::{TranslationError, TranslationStage},
};

#[derive(Debug)]
pub struct MultipassTable {
    rules: PassRules,
}

impl MultipassTable {
    pub fn compile(
        rules: &[AnchoredRule],
        direction: Direction,
        stage: TranslationStage,
        ctx: &TableContext,
    ) -> Result<Self, TranslationError> {
        Ok(Self {
            rules: PassRules::compile(rules, direction, stage, ctx)?,
        })
    }

    pub fn translate(&self, input: &str) -> String {
        self.trace(input).iter().map(|t| t.output()).collect()
    }

    pub fn trace(&self, input: &str) -> Vec<ResolvedTranslation> {
        self.rules.trace(input)
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

    fn corrections(rules: &[&str]) -> MultipassTable {
        let rules: Vec<_> = rules.iter().map(|r| parse_rule(r)).collect();
        let ctx = TableContext::compile(&rules).unwrap();
        MultipassTable::compile(&rules, Direction::Forward, TranslationStage::Pre, &ctx).unwrap()
    }

    #[test]
    fn a_test_is_greedy_and_does_not_backtrack() {
        let table = corrections(&["lowercase a 1", "correct $l.\"a\" \"x\""]);
        // `$l.` takes every letter, so nothing is left for the "a"
        assert_eq!(table.translate("aaa"), "aaa");
    }

    #[test]
    fn a_negated_attribute_takes_a_character_without_it() {
        let table = corrections(&["lowercase a 1", "sign ; 46", "correct !$l[\"a\"] \"x\""]);
        assert_eq!(table.translate(";a"), ";x");
        assert_eq!(table.translate("aa"), "aa");
    }

    #[test]
    fn rules_with_more_leading_characters_are_tried_first() {
        let table = corrections(&[
            "lowercase a 1",
            "correct $l.\"b\" \"y\"",
            "correct \"ab\" \"x\"",
        ]);
        assert_eq!(table.translate("ab"), "x");
    }

    #[test]
    fn a_lookback_checks_what_came_before() {
        let table = corrections(&["lowercase a 1", "correct _\"a\"[\"b\"] \"x\""]);
        assert_eq!(table.translate("abb"), "axb");
    }

    #[test]
    fn a_copy_replaces_the_whole_match() {
        let table = corrections(&["lowercase a 1", "correct \"<\"[$l.]\">\" \"(\"*\")\""]);
        assert_eq!(table.translate("<aa>"), "(aa)");
    }

    #[test]
    fn variables_count_up_and_down() {
        let table = corrections(&[
            "correct \"+\" #1+?",
            "correct \"-\" #1-?",
            "correct #1=2\"a\" \"x\"",
        ]);
        assert_eq!(table.translate("++a-a"), "xa");
    }

    #[test]
    fn grouping_characters_in_tests_and_actions() {
        let table = corrections(&["grouping p () 1,2", "correct {p}p }p{p"]);
        assert_eq!(table.translate("()"), ")(");
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
}
