use std::collections::HashSet;

use crate::{
    Direction,
    emphasis::EmphasisSpan,
    parser::{AnchoredRule, BrailleChars, CharacterClass, Constraints, HasDirection, Rule},
    translator::{
        PositionMap, ResolvedTranslation, TranslationError, TranslationOptions, TranslationStage,
        back_emphasis,
        table::{
            TableContext, display::DisplayTable, multipass::MultipassTable, primary::PrimaryTable,
        },
    },
};

#[derive(Debug)]
pub enum Transformation {
    Pre(MultipassTable),
    Primary(PrimaryTable),
    Post(MultipassTable),
    Display(DisplayTable),
}

impl Transformation {
    pub fn trace(&self, input: &str, options: &TranslationOptions) -> Vec<ResolvedTranslation> {
        match self {
            Transformation::Pre(t) => t.trace(input),
            Transformation::Primary(t) => t.trace(input, options),
            Transformation::Post(t) => t.trace(input),
            Transformation::Display(t) => t.trace(input),
        }
    }

    fn translate(&self, input: &str) -> String {
        match self {
            Transformation::Pre(t) => t.translate(input),
            Transformation::Primary(t) => t.translate(input),
            Transformation::Post(t) => t.translate(input),
            Transformation::Display(t) => t.translate(input),
        }
    }

    fn translate_with_options(&self, input: &str, options: &TranslationOptions) -> String {
        match self {
            Transformation::Pre(t) => t.translate(input),
            Transformation::Primary(t) => t.translate_with_options(input, options),
            Transformation::Post(t) => t.translate(input),
            Transformation::Display(t) => t.translate(input),
        }
    }
}

/// liblouis defines the two characters of a `grouping` rule as `math` characters translating
/// to its two cells.
fn with_grouping_characters(rules: &[AnchoredRule]) -> Vec<AnchoredRule> {
    let mut expanded = Vec::with_capacity(rules.len());
    for rule in rules {
        expanded.push(rule.clone());
        if let Rule::Grouping {
            chars, left, right, ..
        } = &rule.rule
        {
            for (character, dots) in chars.chars().zip([left, right]) {
                let math = Rule::Math {
                    character,
                    dots: BrailleChars::from(vec![dots.clone()]),
                    constraints: Constraints::empty(),
                };
                expanded.push(AnchoredRule::new(
                    math,
                    rule.path().map(Into::into),
                    rule.line(),
                ));
            }
        }
    }
    expanded
}

#[derive(Debug)]
pub struct TranslationPipeline {
    steps: Vec<Transformation>,
    direction: Direction,
    /// The characters or cells that separate words on the input side of the main pass
    spaces: HashSet<char>,
    word_separators: HashSet<char>,
}

impl TranslationPipeline {
    pub fn compile(
        all_rules: &[AnchoredRule],
        direction: Direction,
    ) -> Result<Self, TranslationError> {
        let all_rules = &with_grouping_characters(all_rules);
        let ctx = TableContext::compile(all_rules)?;
        let mut steps = Vec::new();

        // ignore rules that aren't meant for the given direction
        let rules: Vec<_> = all_rules
            .iter()
            .filter(|r| r.is_direction(direction))
            .cloned()
            .collect();

        let correct_rules: Vec<AnchoredRule> = rules
            .iter()
            .filter(|r| matches!(r.rule, Rule::Correct { .. }))
            .cloned()
            .collect();
        if !correct_rules.is_empty() {
            let transform =
                MultipassTable::compile(&correct_rules, direction, TranslationStage::Pre, &ctx)?;
            steps.push(Transformation::Pre(transform));
        }
        // liblouis gives characters and cells their attributes whatever the direction
        // of the rule defining them, while their translation depends on it
        let context = TableContext::compile(rules.as_slice())?.with_classes_of(&ctx);
        let transform = PrimaryTable::compile(
            rules.as_slice(),
            direction,
            TranslationStage::Main,
            &context,
        )?
        .with_all_definitions(all_rules, &ctx);
        steps.push(Transformation::Primary(transform));
        let pass2_rules: Vec<AnchoredRule> = rules
            .iter()
            .filter(|r| matches!(r.rule, Rule::Pass2 { .. }))
            .cloned()
            .collect();
        if !pass2_rules.is_empty() {
            let transform =
                MultipassTable::compile(&pass2_rules, direction, TranslationStage::Post1, &ctx)?;
            steps.push(Transformation::Post(transform));
        }
        let pass3_rules: Vec<AnchoredRule> = rules
            .iter()
            .filter(|r| matches!(r.rule, Rule::Pass3 { .. }))
            .cloned()
            .collect();
        if !pass3_rules.is_empty() {
            let transform =
                MultipassTable::compile(&pass3_rules, direction, TranslationStage::Post2, &ctx)?;
            steps.push(Transformation::Post(transform));
        }
        let pass4_rules: Vec<AnchoredRule> = rules
            .iter()
            .filter(|r| matches!(r.rule, Rule::Pass4 { .. }))
            .cloned()
            .collect();
        if !pass4_rules.is_empty() {
            let transform =
                MultipassTable::compile(&pass4_rules, direction, TranslationStage::Post3, &ctx)?;
            steps.push(Transformation::Post(transform));
        }
        let classes = match direction {
            Direction::Forward => ctx.character_classes(),
            Direction::Backward => ctx.dots_classes(),
        };
        let spaces = classes.get(&CharacterClass::Space).unwrap_or_default();
        let mut word_separators = spaces.clone();
        if direction == Direction::Forward {
            word_separators.extend(
                classes
                    .get(&CharacterClass::Seqdelimiter)
                    .unwrap_or_default(),
            );
        }
        let steps = match direction {
            Direction::Forward => steps,
            Direction::Backward => steps.into_iter().rev().collect(),
        };
        Ok(Self {
            steps,
            direction,
            spaces,
            word_separators,
        })
    }

    /// Add `display` as the outermost stage, mapping the braille side of the pipeline.
    ///
    /// The display table is supplied rather than compiled from the same rules because the
    /// caller chooses it independently of the translation table: the YAML harness takes it
    /// from a test's `display:` key, `louis translate` from `--display`.
    pub fn with_display(mut self, display: DisplayTable) -> Self {
        let step = Transformation::Display(display);
        match self.direction {
            // translating to braille, so the display table maps what we produced: it runs last
            Direction::Forward => self.steps.push(step),
            // translating from braille, so it maps what we are about to read: it runs first.
            // `compile` has already reversed `steps`, so pushing would put it at the wrong end.
            Direction::Backward => self.steps.insert(0, step),
        }
        self
    }

    pub fn trace(&self, input: &str) -> Vec<Vec<ResolvedTranslation>> {
        self.trace_with_options(input, &TranslationOptions::default())
    }

    pub fn trace_with_options(
        &self,
        input: &str,
        options: &TranslationOptions,
    ) -> Vec<Vec<ResolvedTranslation>> {
        let mut input = input.to_string();
        let mut options = options.clone();
        let mut result: Vec<Vec<ResolvedTranslation>> = Vec::new();
        for step in &self.steps {
            let translations = step.trace(&input, &options);
            if matches!(step, Transformation::Pre(_)) {
                options = map_emphasis(options, &translations);
            }
            input = translations.iter().map(|t| t.output()).collect();
            result.push(translations);
        }
        result
    }

    pub fn translate(&self, input: &str) -> String {
        let mut result = input.to_string();
        for step in &self.steps {
            result = step.translate(&result);
        }
        result
    }

    pub fn translate_with_options(&self, input: &str, options: &TranslationOptions) -> String {
        let mut result = input.to_string();
        let mut options = options.clone();
        for step in &self.steps {
            if matches!(step, Transformation::Pre(_)) && !options.emphasis().is_empty() {
                let translations = step.trace(&result, &options);
                options = map_emphasis(options, &translations);
                result = translations.iter().map(|t| t.output()).collect();
            } else {
                result = step.translate_with_options(&result, &options);
            }
        }
        result
    }

    /// Translate `input` into at most `max_output` characters, as liblouis does into an
    /// output buffer of that size, also returning how much of the input was used. The main
    /// pass stops before the first rule that no longer fits and, in the middle of a word,
    /// gives back the whole word; every pass then skips the spaces that follow.
    pub fn translate_limited(
        &self,
        input: &str,
        options: &TranslationOptions,
        max_output: usize,
    ) -> (String, usize) {
        let mut text = input.to_string();
        let mut consumed_original: Option<usize> = None;
        let mut stages: Vec<Vec<ResolvedTranslation>> = Vec::new();
        for step in &self.steps {
            let chars: Vec<char> = text.chars().collect();
            let steps = step.trace(&text, options);
            let is_main = matches!(step, Transformation::Primary(_));
            let (mut pos, mut out) = (0, 0);
            let mut last_word = (0, 0);
            let mut kept = 0;
            let mut overflow = false;
            for (i, t) in steps.iter().enumerate() {
                if is_main && pos > 0 && self.word_separators.contains(&chars[pos - 1]) {
                    last_word = (pos, out);
                }
                let length = t.output().chars().count();
                if out + length > max_output {
                    overflow = true;
                    break;
                }
                out += length;
                pos += t.input().chars().count();
                kept = i + 1;
                if is_main
                    && pos > 0
                    && pos <= chars.len()
                    && self.word_separators.contains(&chars[pos - 1])
                {
                    last_word = (pos, out);
                }
            }
            let mut steps = steps;
            if overflow {
                if is_main
                    && last_word.1 != 0
                    && pos < chars.len()
                    && !self.spaces.contains(&chars[pos])
                {
                    let (word_pos, word_out) = last_word;
                    let (mut p, mut o) = (0, 0);
                    kept = 0;
                    for t in &steps {
                        if p >= word_pos || o >= word_out {
                            break;
                        }
                        p += t.input().chars().count();
                        o += t.output().chars().count();
                        kept += 1;
                    }
                    pos = word_pos;
                }
                steps.truncate(kept);
                while pos < chars.len() && self.spaces.contains(&chars[pos]) {
                    pos += 1;
                }
            }
            let stage_consumed = if overflow { pos } else { chars.len() };
            consumed_original = Some(match consumed_original {
                None => stage_consumed,
                Some(previous) => {
                    let map =
                        PositionMap::from_trace(input.chars().count(), &stages, self.direction);
                    map.input_positions()
                        .get(stage_consumed)
                        .copied()
                        .unwrap_or(previous)
                        .min(previous)
                }
            });
            text = steps.iter().map(|t| t.output()).collect();
            stages.push(steps);
        }
        (text, consumed_original.unwrap_or(input.chars().count()))
    }

    /// Translate `input` and map its positions to the positions of the output
    pub fn translate_with_positions(
        &self,
        input: &str,
        options: &TranslationOptions,
    ) -> (String, PositionMap) {
        let (output, positions, _) = self.translate_with_emphasis(input, options);
        (output, positions)
    }

    /// Like [`Self::translate_with_positions`], also giving the emphasis of a
    /// back-translation when `options` ask for it
    pub fn translate_with_emphasis(
        &self,
        input: &str,
        options: &TranslationOptions,
    ) -> (String, PositionMap, Option<Vec<EmphasisSpan>>) {
        let stages = self.trace_with_options(input, options);
        let output = stages.last().map_or_else(
            || input.to_string(),
            |steps| steps.iter().map(|t| t.output()).collect(),
        );
        let positions = PositionMap::from_trace(input.chars().count(), &stages, self.direction);
        let emphasis =
            (self.direction == Direction::Backward && options.emphasis_output()).then(|| {
                self.steps
                    .iter()
                    .zip(&stages)
                    .find(|(step, _)| matches!(step, Transformation::Primary(_)))
                    .map(|(_, trace)| back_emphasis::back_translated(trace))
                    .unwrap_or_default()
            });
        (output, positions, emphasis)
    }
}

/// Carries the emphasis of the input over to the output of `correct`, as liblouis's
/// `makeCorrections` does with the typeforms: each output character takes those of the
/// input character it came from.
fn map_emphasis(
    options: TranslationOptions,
    translations: &[ResolvedTranslation],
) -> TranslationOptions {
    if options.emphasis().is_empty() {
        return options;
    }
    let input_len: usize = translations.iter().map(|t| t.length()).sum();
    let mut origins = Vec::new();
    let mut pos = 0;
    for t in translations {
        let origin = pos.min(input_len.saturating_sub(1));
        origins.extend(std::iter::repeat_n(origin, t.output().chars().count()));
        pos += t.length();
    }
    let mut spans: Vec<EmphasisSpan> = Vec::new();
    for span in options.emphasis() {
        let mut start = None;
        for (k, origin) in origins.iter().chain([&usize::MAX]).enumerate() {
            match (span.range.contains(origin), start) {
                (true, None) => start = Some(k),
                (false, Some(s)) => {
                    spans.push(EmphasisSpan::new(span.class.clone(), s..k));
                    start = None;
                }
                _ => {}
            }
        }
    }
    options.with_emphasis(spans)
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
        let rules = [
            parse_rule("always foo 123"),
            parse_rule("always bar 456"),
            parse_rule("noback correct \"baz\" \"bar\""),
            parse_rule("space \\s 0"),
        ];
        let pipeline = TranslationPipeline::compile(&rules, Direction::Forward).unwrap();
        assert_eq!(pipeline.translate("baz"), "⠸");
        assert_eq!(pipeline.translate("foobaz"), "⠇⠸");
        assert_eq!(pipeline.translate("foobar"), "⠇⠸");
        assert_eq!(pipeline.translate("  "), "⠀⠀");
        assert_eq!(pipeline.translate("🐂"), "⠄⡳⠽⠂⠋⠲⠴⠆⠄");
    }

    #[test]
    fn correct_carries_emphasis_over() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("emphclass italic"),
            parse_rule("emphletter italic 46"),
            parse_rule("noback correct \"aa\" \"a\""),
        ];
        let pipeline = TranslationPipeline::compile(&rules, Direction::Forward).unwrap();
        let options =
            TranslationOptions::default().with_emphasis(vec![EmphasisSpan::new("italic", 2..3)]);
        assert_eq!(pipeline.translate_with_options("aab", &options), "⠁⠨⠃");
    }

    #[test]
    fn pass2() {
        let rules = [
            parse_rule("always foo 123"),
            parse_rule("always bar 456"),
            parse_rule("noback pass2 @123 @1"),
            parse_rule("space \\s 0"),
        ];
        let pipeline = TranslationPipeline::compile(&rules, Direction::Forward).unwrap();
        assert_eq!(pipeline.translate("foo"), "⠁");
        assert_eq!(pipeline.translate("foobar"), "⠁⠸");
        assert_eq!(pipeline.translate("  "), "⠀⠀");
        assert_eq!(pipeline.translate("🐂"), "⠄⡳⠽⠂⠋⠲⠴⠆⠄");
    }

    #[test]
    fn pass2_with_capture() {
        let rules = [
            parse_rule("lowercase o 135"),
            parse_rule("lowercase ύ 5-13456"),
            parse_rule("sign ΄ 5"),
            parse_rule("attribute accent ΄"),
            parse_rule("noback pass2 @135[%accent]@13456 *@136"),
        ];
        let pipeline = TranslationPipeline::compile(&rules, Direction::Forward).unwrap();
        assert_eq!(pipeline.translate("o"), "⠕");
        // `*@136` copies the accent out of the bracket and consumes the whole match,
        // so the @135 and @13456 cells around it are replaced, not kept
        assert_eq!(pipeline.translate("oύ"), "⠐⠥");
    }

    #[test]
    fn a_limited_output_gives_back_the_word_that_does_not_fit() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("lowercase c 14"),
            parse_rule("space \\s 0"),
        ];
        let pipeline = TranslationPipeline::compile(&rules, Direction::Forward).unwrap();
        let options = TranslationOptions::default();
        assert_eq!(
            pipeline.translate_limited("abc abc", &options, 5),
            ("⠁⠃⠉⠀".to_string(), 4)
        );
        assert_eq!(
            pipeline.translate_limited("abc abc", &options, 7),
            ("⠁⠃⠉⠀⠁⠃⠉".to_string(), 7)
        );
        assert_eq!(
            pipeline.translate_limited("abc", &options, 2),
            ("⠁⠃".to_string(), 2)
        );
    }

    #[test]
    fn translate_with_positions_matches_translate() {
        let rules = [
            parse_rule("always foo 123"),
            parse_rule("always bar 456"),
            parse_rule("noback correct \"baz\" \"bar\""),
            parse_rule("noback pass2 @456 @1"),
            parse_rule("space \\s 0"),
        ];
        let pipeline = TranslationPipeline::compile(&rules, Direction::Forward).unwrap();
        let options = TranslationOptions::default();
        for input in ["foobaz", "foobar", "  ", "🐂", ""] {
            assert_eq!(
                pipeline.translate_with_positions(input, &options).0,
                pipeline.translate_with_options(input, &options)
            );
        }
    }

    #[test]
    fn translate_with_positions_composes_stages() {
        let rules = [
            parse_rule("always foo 123"),
            parse_rule("always bar 456"),
            parse_rule("noback correct \"baz\" \"bar\""),
            parse_rule("space \\s 0"),
        ];
        let pipeline = TranslationPipeline::compile(&rules, Direction::Forward).unwrap();
        let (output, positions) =
            pipeline.translate_with_positions("foobaz", &TranslationOptions::default());
        assert_eq!(output, "⠇⠸");
        assert_eq!(positions.output_positions(), [0, 0, 0, 1, 1, 1]);
        assert_eq!(positions.input_positions(), [0, 3]);
        assert_eq!(positions.cursor(4), 1);
        assert_eq!(positions.cursor(6), 2);
    }

    #[test]
    fn pass3() {
        let rules = [
            parse_rule("always foo 123"),
            parse_rule("always bar 456"),
            parse_rule("noback pass2 @123 @78"),
            parse_rule("noback pass3 @78 @1"),
            parse_rule("space \\s 0"),
        ];
        let pipeline = TranslationPipeline::compile(&rules, Direction::Forward).unwrap();
        assert_eq!(pipeline.translate("foo"), "⠁");
        assert_eq!(pipeline.translate("foobar"), "⠁⠸");
        assert_eq!(pipeline.translate("  "), "⠀⠀");
        assert_eq!(pipeline.translate("🐂"), "⠄⡳⠽⠂⠋⠲⠴⠆⠄");
    }

    #[test]
    fn pass4() {
        let rules = [
            parse_rule("always foo 123"),
            parse_rule("always bar 456"),
            parse_rule("noback pass2 @123 @67"),
            parse_rule("noback pass3 @67 @78"),
            parse_rule("noback pass4 @78 @1"),
            parse_rule("space \\s 0"),
        ];
        let pipeline = TranslationPipeline::compile(&rules, Direction::Forward).unwrap();
        assert_eq!(pipeline.translate("foo"), "⠁");
        assert_eq!(pipeline.translate("foobar"), "⠁⠸");
        assert_eq!(pipeline.translate("  "), "⠀⠀");
        assert_eq!(pipeline.translate("🐂"), "⠄⡳⠽⠂⠋⠲⠴⠆⠄");
    }

    #[test]
    fn with_display_puts_the_display_stage_on_the_braille_side() {
        let rules = [parse_rule("letter a 1"), parse_rule("space \\s 0")];
        // maps the cell for dot 1 to `A` when displaying, and back when reading
        let display = [parse_rule("display A 1")];

        // forward: translate to braille, then show the cell as `A`
        let forward = TranslationPipeline::compile(&rules, Direction::Forward)
            .unwrap()
            .with_display(DisplayTable::compile(&display, Direction::Forward));
        assert_eq!(forward.translate("a"), "A");

        // backward: read `A` as the cell, then translate it back to text. `compile` has
        // already reversed the steps here, so this only works if `with_display` inserts at
        // the front rather than pushing.
        let backward = TranslationPipeline::compile(&rules, Direction::Backward)
            .unwrap()
            .with_display(DisplayTable::compile(&display, Direction::Backward));
        assert_eq!(backward.translate("A"), "a");

        // a one-to-one stage must leave the positions alone
        let options = TranslationOptions::default();
        let (output, _) = forward.translate_with_positions("a", &options);
        assert_eq!(output, "A");
    }
}
