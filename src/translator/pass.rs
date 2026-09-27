//! The multipass engine of liblouis: at every position the `correct` or `passN` rules are tried
//! in turn and the first whose test holds rewrites its replace region. A test is a program of
//! instructions run left to right over the input without backtracking, so its quantifiers are
//! greedy and a negation applies to a single instruction.

use std::collections::HashSet;

use crate::Direction;
use crate::parser::multipass::test::Operator;
use crate::parser::{
    Action, ActionInstruction, AnchoredRule, Attribute, CharacterClass, CharacterClasses,
    Quantifier, Test, TestInstruction,
};
use crate::translator::swap::Swapper;
use crate::translator::table::TableContext;
use crate::translator::{ResolvedTranslation, TranslationError, TranslationStage};

/// liblouis's `pass_until` quantifier (`.`) repeats up to this many times
const UNTIL: usize = 0xffff;

#[derive(Debug, Clone)]
enum Op {
    First,
    Last,
    Lookback(usize),
    Not,
    Chars(Vec<char>),
    StartReplace,
    EndReplace,
    Attributes {
        chars: HashSet<char>,
        min: usize,
        max: usize,
        /// `$a`, which takes any character without looking at it
        any: bool,
    },
    Swap {
        swapper: Swapper,
        min: usize,
        max: usize,
    },
    Group(char),
    Variable {
        var: u8,
        op: Operator,
        operand: u8,
    },
}

#[derive(Debug, Clone)]
enum Act {
    Output(String),
    Swap(Swapper),
    Copy,
    Omit,
    Assign { var: u8, value: u8 },
    Increment(u8),
    Decrement(u8),
}

/// The pass variables `#1` to `#50`, all 0 at the start of a pass
#[derive(Debug, Default)]
struct Variables(std::collections::HashMap<u8, u16>);

impl Variables {
    fn get(&self, var: u8) -> u16 {
        self.0.get(&var).copied().unwrap_or(0)
    }

    fn holds(&self, var: u8, op: &Operator, operand: u8) -> bool {
        let (value, operand) = (self.get(var), u16::from(operand));
        match op {
            Operator::Eq => value == operand,
            Operator::Lt => value < operand,
            Operator::Gt => value > operand,
            Operator::LtEq => value <= operand,
            Operator::GtEq => value >= operand,
        }
    }
}

/// Where a test matched: `start_match..end_match` is everything the test looked at from the
/// current position on, `start_replace..end_replace` the part the action replaces
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Match {
    start_match: usize,
    start_replace: usize,
    end_replace: usize,
    end_match: usize,
    /// How much the test looked at: the span of input it went over, less what `$a` took
    /// unseen, and the conditions it checked besides
    context: usize,
}

#[derive(Debug, Clone)]
struct PassRule {
    test: Vec<Op>,
    action: Vec<Act>,
    /// The literal characters leading the test, as liblouis's `passFindCharacters` counts them
    charslen: usize,
    origin: AnchoredRule,
}

fn range(quantifier: &Option<Quantifier>) -> (usize, usize) {
    match quantifier {
        None | Some(Quantifier::Number(0)) => (1, 1),
        Some(Quantifier::Number(n)) => (*n as usize, *n as usize),
        Some(Quantifier::Range(min, max)) => (*min as usize, *max as usize),
        Some(Quantifier::OneOrMore) => (1, UNTIL),
    }
}

/// What a rule needs to know about the table to compile its operands
struct Compiler<'a> {
    ctx: &'a TableContext,
    classes: &'a CharacterClasses,
    /// whether the rule works on cells (`passN`) rather than characters (`correct`)
    dots: bool,
}

impl Compiler<'_> {
    fn class_chars(&self, class: &CharacterClass) -> HashSet<char> {
        self.classes.get(class).unwrap_or_default()
    }

    fn group_char(&self, name: &str, open: bool) -> Result<char, TranslationError> {
        let grouping = self
            .ctx
            .grouping(name)
            .ok_or_else(|| TranslationError::GroupingNotDefined(name.to_string()))?;
        let (start, end) = if self.dots {
            grouping.dots
        } else {
            grouping.chars
        };
        Ok(if open { start } else { end })
    }

    fn test(
        &self,
        instructions: &[TestInstruction],
        ops: &mut Vec<Op>,
    ) -> Result<(), TranslationError> {
        for instruction in instructions {
            match instruction {
                TestInstruction::Lookback { len } => ops.push(Op::Lookback(*len as usize)),
                TestInstruction::Variable { var, op, operand } => ops.push(Op::Variable {
                    var: *var,
                    op: op.clone(),
                    operand: *operand,
                }),
                TestInstruction::String { s } => ops.push(Op::Chars(s.chars().collect())),
                TestInstruction::Dots { dots } => {
                    ops.push(Op::Chars(dots.to_string().chars().collect()))
                }
                TestInstruction::Attributes { attrs, quantifier } => {
                    let (min, max) = range(quantifier);
                    let mut chars = HashSet::new();
                    for attr in attrs {
                        match attr {
                            Attribute::Any => chars.extend(self.classes.members()),
                            Attribute::Class(class) => chars.extend(self.class_chars(class)),
                            Attribute::ByOrder(n) => {
                                if let Some(class) = self.ctx.user_class(*n) {
                                    chars.extend(self.class_chars(&class));
                                }
                            }
                            Attribute::Boundary => {}
                        }
                    }
                    ops.push(Op::Attributes {
                        any: attrs.contains(&Attribute::Any),
                        chars,
                        min,
                        max,
                    });
                }
                TestInstruction::Class { name, quantifier } => {
                    let (min, max) = range(quantifier);
                    match self.ctx.swap_classes().get(name) {
                        Some(swapper) => ops.push(Op::Swap { swapper, min, max }),
                        None => ops.push(Op::Attributes {
                            any: false,
                            chars: self.class_chars(&CharacterClass::from(name.as_str())),
                            min,
                            max,
                        }),
                    }
                }
                TestInstruction::Negate { test } => {
                    ops.push(Op::Not);
                    self.test(std::slice::from_ref(test), ops)?;
                }
                TestInstruction::Replace { tests } => {
                    ops.push(Op::StartReplace);
                    self.test(tests, ops)?;
                    ops.push(Op::EndReplace);
                }
                TestInstruction::AtBeginning => ops.push(Op::First),
                TestInstruction::AtEnd => ops.push(Op::Last),
                TestInstruction::GroupStart { name } => {
                    ops.push(Op::Group(self.group_char(name, true)?))
                }
                TestInstruction::GroupEnd { name } => {
                    ops.push(Op::Group(self.group_char(name, false)?))
                }
            }
        }
        Ok(())
    }

    fn action(&self, instructions: &[ActionInstruction]) -> Result<Vec<Act>, TranslationError> {
        instructions
            .iter()
            .map(|instruction| {
                Ok(match instruction {
                    ActionInstruction::String { s } => Act::Output(s.clone()),
                    ActionInstruction::Dots { dots } => Act::Output(dots.to_string()),
                    ActionInstruction::SwapClass { name } => Act::Swap(
                        self.ctx
                            .swap_classes()
                            .get(name)
                            .ok_or_else(|| TranslationError::SwapClassNotDefined(name.clone()))?,
                    ),
                    ActionInstruction::Assignment { variable, value } => Act::Assign {
                        var: *variable,
                        value: *value,
                    },
                    ActionInstruction::Increment { variable } => Act::Increment(*variable),
                    ActionInstruction::Decrement { variable } => Act::Decrement(*variable),
                    ActionInstruction::Replace => Act::Copy,
                    ActionInstruction::Ignore => Act::Omit,
                    ActionInstruction::GroupStart { name } => {
                        Act::Output(self.group_char(name, true)?.to_string())
                    }
                    ActionInstruction::GroupEnd { name } => {
                        Act::Output(self.group_char(name, false)?.to_string())
                    }
                })
            })
            .collect()
    }
}

/// The literal characters a test starts with, looking through lookbacks that go back over
/// fixed-width instructions, as liblouis's `passFindCharacters` counts them
fn leading_chars(test: &[Op]) -> usize {
    let mut lookback = 0;
    for op in test {
        match op {
            Op::Chars(chars) => {
                if chars.len() > lookback {
                    return chars.len() - lookback;
                }
                lookback -= chars.len();
            }
            Op::Attributes { min, max, .. } if min == max && *max <= lookback => lookback -= max,
            Op::Attributes { .. } | Op::Swap { .. } | Op::Group(_) => return 0,
            Op::Lookback(n) => lookback += n,
            Op::Variable { .. }
            | Op::Not
            | Op::StartReplace
            | Op::EndReplace
            | Op::First
            | Op::Last => {}
        }
    }
    0
}

/// liblouis's `matchCurrentInput`: the characters that fit before the end of the input match
fn matches_chars(input: &[char], pos: usize, chars: &[char]) -> bool {
    input[pos.min(input.len())..]
        .iter()
        .zip(chars)
        .all(|(a, b)| a == b)
}

impl PassRule {
    fn compile(
        test: &Test,
        action: &Action,
        origin: &AnchoredRule,
        compiler: &Compiler,
    ) -> Result<Self, TranslationError> {
        let mut ops = Vec::new();
        compiler.test(test.tests(), &mut ops)?;
        Ok(Self {
            charslen: leading_chars(&ops),
            action: compiler.action(&action.actions())?,
            test: ops,
            origin: origin.clone(),
        })
    }

    /// liblouis's `passDoTest`
    fn test_forward(&self, input: &[char], start: usize, vars: &Variables) -> Option<Match> {
        let len = input.len() as isize;
        let mut pos = start as isize;
        let mut not = false;
        let (mut lowest, mut highest, mut conditions, mut unchecked) = (pos, pos, 0, 0);
        let (mut start_replace, mut end_replace) = (None, None);
        for op in &self.test {
            if pos > len || pos < 0 {
                return None;
            }
            let its_true = match op {
                Op::First => {
                    conditions += 1;
                    pos == 0
                }
                Op::Last => {
                    conditions += 1;
                    pos == len
                }
                Op::Lookback(n) => {
                    pos -= *n as isize;
                    pos >= 0
                }
                Op::Not => {
                    not = !not;
                    continue;
                }
                Op::Chars(chars) => {
                    let its_true = matches_chars(input, pos as usize, chars);
                    pos += chars.len() as isize;
                    its_true
                }
                Op::StartReplace => {
                    start_replace = Some(pos as usize);
                    true
                }
                Op::EndReplace => {
                    end_replace = Some(pos as usize);
                    true
                }
                Op::Attributes {
                    chars,
                    min,
                    max,
                    any,
                } => {
                    let before = pos;
                    let mut its_true = true;
                    for _ in 0..*min {
                        if pos >= len || chars.contains(&input[pos as usize]) == not {
                            its_true = false;
                            break;
                        }
                        pos += 1;
                    }
                    if its_true {
                        for _ in *min..*max {
                            if pos >= len || chars.contains(&input[pos as usize]) == not {
                                break;
                            }
                            pos += 1;
                        }
                    }
                    // the negation went into each character's check
                    not = false;
                    if *any {
                        unchecked += pos - before;
                    }
                    its_true
                }
                Op::Swap { swapper, min, max } => swap_test(input, &mut pos, swapper, *min, *max),
                Op::Group(c) => {
                    let its_true = input.get(pos as usize) == Some(c);
                    pos += 1;
                    its_true
                }
                Op::Variable { var, op, operand } => {
                    conditions += 1;
                    vars.holds(*var, op, *operand)
                }
            };
            lowest = lowest.min(pos);
            highest = highest.max(pos);
            if its_true == not {
                return None;
            }
            not = false;
        }
        if pos > len || pos < 0 {
            return None;
        }
        let end_match = pos as usize;
        let (start_replace, end_replace) = match start_replace {
            None => (start, Some(end_match)),
            Some(start_replace) => (start_replace, end_replace),
        };
        let end_replace = end_replace?;
        if start_replace < start {
            return None;
        }
        Some(Match {
            start_match: start,
            start_replace,
            end_replace,
            end_match,
            context: ((highest - lowest - unchecked) as usize) + conditions,
        })
    }

    /// liblouis's `back_passDoTest`, which, unlike the forward test, applies a negation of an
    /// attribute test to the whole test rather than to each character, and knows no grouping
    fn test_backward(&self, input: &[char], start: usize, vars: &Variables) -> Option<Match> {
        let len = input.len() as isize;
        let mut pos = start as isize;
        let mut not = false;
        let (mut lowest, mut highest, mut conditions, mut unchecked) = (pos, pos, 0, 0);
        let (mut start_replace, mut end_replace) = (None, None);
        for op in &self.test {
            if pos > len {
                return None;
            }
            let its_true = match op {
                Op::First => {
                    conditions += 1;
                    pos == 0
                }
                Op::Last => {
                    conditions += 1;
                    pos == len
                }
                Op::Lookback(n) => {
                    pos -= *n as isize;
                    if pos < 0 {
                        pos = 0;
                        false
                    } else {
                        true
                    }
                }
                Op::Not => {
                    not = !not;
                    continue;
                }
                Op::Chars(chars) => {
                    let its_true = matches_chars(input, pos as usize, chars);
                    pos += chars.len() as isize;
                    its_true
                }
                Op::StartReplace => {
                    start_replace = Some(pos as usize);
                    true
                }
                Op::EndReplace => {
                    end_replace = Some(pos as usize);
                    true
                }
                Op::Attributes {
                    chars,
                    min,
                    max,
                    any,
                } => {
                    let before = pos;
                    let mut its_true = true;
                    for _ in 0..*min {
                        if pos >= len || !chars.contains(&input[pos as usize]) {
                            its_true = false;
                            break;
                        }
                        pos += 1;
                    }
                    if its_true {
                        for _ in *min..*max {
                            if pos >= len || !chars.contains(&input[pos as usize]) {
                                break;
                            }
                            pos += 1;
                        }
                    }
                    if *any {
                        unchecked += pos - before;
                    }
                    its_true
                }
                Op::Swap { swapper, min, max } => swap_test(input, &mut pos, swapper, *min, *max),
                Op::Group(_) => return None,
                Op::Variable { var, op, operand } => {
                    conditions += 1;
                    vars.holds(*var, op, *operand)
                }
            };
            lowest = lowest.min(pos);
            highest = highest.max(pos);
            if its_true == not {
                return None;
            }
            not = false;
        }
        if pos > len {
            return None;
        }
        let end_match = pos as usize;
        let (start_replace, end_replace) = match start_replace {
            None => (start, end_match),
            Some(start_replace) => (start_replace, end_replace.unwrap_or(end_match)),
        };
        Some(Match {
            start_match: start,
            start_replace,
            end_replace,
            end_match,
            context: ((highest - lowest - unchecked) as usize) + conditions,
        })
    }

    /// Rewrite the match: the input before the replace region is kept and the action takes the
    /// place of the region, or with a `*` the action takes the place of the whole match.
    /// Returns the translations and the position to go on from.
    fn apply(
        &self,
        input: &[char],
        m: Match,
        vars: &mut Variables,
        stage: TranslationStage,
    ) -> (Vec<ResolvedTranslation>, usize) {
        let copies = self.action.iter().any(|act| matches!(act, Act::Copy));
        let new_pos = if copies { m.end_match } else { m.end_replace }.max(m.start_match);
        let region = m.start_replace.clamp(m.start_match, new_pos)
            ..m.end_replace.clamp(m.start_match, new_pos);
        let translation = |input: &[char], output: &str| {
            let input: String = input.iter().collect();
            ResolvedTranslation::new(
                &input,
                output,
                input.chars().count(),
                stage,
                self.origin.clone(),
            )
        };
        let mut translations = Vec::new();
        // the input before the replace region is kept, unless a `*` takes the whole match
        if copies {
            if region.start > m.start_match {
                translations.push(translation(&input[m.start_match..region.start], ""));
            }
        } else {
            for c in &input[m.start_match..region.start] {
                translations.push(translation(&[*c], &c.to_string()));
            }
        }
        // liblouis maps what `*` and a swap produce char by char, and everything else to the
        // start of the replace region
        let per_char = copies || self.action.iter().any(|act| matches!(act, Act::Swap(_)));
        let mut consumed = region.start;
        let mut output = String::new();
        for act in &self.action {
            let mut region_chars = |replace: &dyn Fn(char) -> Option<String>| {
                for (i, c) in input[region.clone()].iter().enumerate() {
                    let replacement = replace(*c).unwrap_or_default();
                    if region.start + i >= consumed {
                        translations.push(translation(&[*c], &replacement));
                        consumed = region.start + i + 1;
                    } else {
                        translations.push(translation(&[], &replacement));
                    }
                }
            };
            match act {
                Act::Output(s) if per_char => translations.push(translation(&[], s)),
                Act::Output(s) => output.push_str(s),
                Act::Swap(swapper) => region_chars(&|c| swapper.replacement(c).map(str::to_string)),
                Act::Copy => region_chars(&|c| Some(c.to_string())),
                Act::Omit => {}
                Act::Assign { var, value } => {
                    vars.0.insert(*var, u16::from(*value));
                }
                Act::Increment(var) => {
                    let value = vars.get(*var).wrapping_add(1);
                    vars.0.insert(*var, value);
                }
                Act::Decrement(var) => {
                    let value = vars.get(*var).saturating_sub(1);
                    vars.0.insert(*var, value);
                }
            }
        }
        if per_char {
            if consumed < new_pos {
                translations.push(translation(&input[consumed..new_pos], ""));
            }
        } else {
            translations.push(translation(&input[region.start..new_pos], &output));
        }
        (translations, new_pos)
    }

    /// How far the rule moves on from the start of the match `m`
    fn consumed(&self, m: &Match) -> usize {
        let end = if self.action.iter().any(|act| matches!(act, Act::Copy)) {
            m.end_match
        } else {
            m.end_replace
        };
        end.saturating_sub(m.start_match)
    }
}

/// liblouis's `swapTest`: at least `min` and at most `max` characters the swap maps
fn swap_test(input: &[char], pos: &mut isize, swapper: &Swapper, min: usize, max: usize) -> bool {
    let start = *pos;
    let mut p = *pos;
    while ((p - start) as usize) < min {
        if p as usize >= input.len() || !swapper.contains(input[p as usize]) {
            return false;
        }
        p += 1;
    }
    while ((p - start) as usize) < max {
        if p as usize >= input.len() || !swapper.contains(input[p as usize]) {
            break;
        }
        p += 1;
    }
    *pos = p;
    true
}

/// The rules of one pass, in the order liblouis tries them
#[derive(Debug)]
pub struct PassRules {
    rules: Vec<PassRule>,
    direction: Direction,
    stage: TranslationStage,
}

impl PassRules {
    pub fn compile(
        rules: &[AnchoredRule],
        direction: Direction,
        stage: TranslationStage,
        ctx: &TableContext,
    ) -> Result<Self, TranslationError> {
        let dots = !matches!(stage, TranslationStage::Pre | TranslationStage::Main);
        let compiler = Compiler {
            ctx,
            classes: if dots {
                ctx.dots_classes()
            } else {
                ctx.character_classes()
            },
            dots,
        };
        let mut compiled = Vec::new();
        for rule in rules {
            if let crate::parser::Rule::Correct { test, action, .. }
            | crate::parser::Rule::Pass2 { test, action, .. }
            | crate::parser::Rule::Pass3 { test, action, .. }
            | crate::parser::Rule::Pass4 { test, action, .. } = &rule.rule
            {
                compiled.push(PassRule::compile(test, action, rule, &compiler)?);
            }
        }
        Ok(Self {
            rules: compiled,
            direction,
            stage,
        })
    }

    /// The rule to apply at `pos`. liblouis tries the rules with more leading literal
    /// characters first and takes the first whose test holds. Among rules that tie on that it
    /// takes the one defined first; here the one moving on further, then the one that looked at
    /// more, wins instead.
    fn select(&self, input: &[char], pos: usize, vars: &Variables) -> Option<(&PassRule, Match)> {
        self.rules
            .iter()
            .filter_map(|rule| {
                let m = match self.direction {
                    Direction::Forward => rule.test_forward(input, pos, vars),
                    Direction::Backward => rule.test_backward(input, pos, vars),
                }?;
                Some((rule, m))
            })
            .max_by_key(|(rule, m)| (rule.charslen, rule.consumed(m), m.context))
    }

    pub fn trace(&self, input: &str) -> Vec<ResolvedTranslation> {
        let input: Vec<char> = input.chars().collect();
        let mut translations = Vec::new();
        let mut vars = Variables::default();
        let mut pos = 0;
        let mut advanced = true;
        while pos < input.len() {
            let selected = if advanced {
                self.select(&input, pos, &vars)
            } else {
                None
            };
            advanced = true;
            match selected {
                Some((rule, m)) => {
                    let (applied, new_pos) = rule.apply(&input, m, &mut vars, self.stage);
                    translations.extend(applied);
                    advanced = new_pos > pos;
                    pos = new_pos;
                }
                None => {
                    let c = input[pos].to_string();
                    translations.push(ResolvedTranslation::new(&c, &c, 1, self.stage, None));
                    pos += 1;
                }
            }
        }
        translations
    }
}
