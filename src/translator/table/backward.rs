//! Rule selection for back-translation, following liblouis's `back_selectRule` and its
//! helpers in `lou_backTranslateString.c`.
//!
//! Unlike forward translation, a boundary check here looks at two different things: the
//! side before a rule is the last character of the text produced so far, classified by the
//! text character classes (`back_setBefore`), while the side after it is the next braille
//! cell, classified by the classes of the characters defined as that single cell
//! (`back_setAfter`), or by scanning ahead for the end of the word (`isEndWord`).

use std::collections::{HashMap, HashSet};

use crate::{
    parser::{
        AnchoredRule, CharacterClass, CharacterClasses, Quantifier, Rule, TestInstruction,
        WithClass,
    },
    translator::{ResolvedTranslation, table::TableContext},
};

/// What `isEndWord` needs to know about the single-cell rules of a braille cell.
#[derive(Debug, Default, Clone, Copy)]
struct CellRules {
    /// A rule that translates the cell to more than one character, other than `begword`
    /// and `midword` (liblouis#360).
    translation: bool,
    postpunc: bool,
    hyphen: bool,
}

/// The state of a back-translation at the position a candidate is checked at.
pub struct Situation<'a> {
    /// The translations applied so far, whose output is the text before the candidate.
    pub output: &'a [ResolvedTranslation],
    /// All the braille cells of the input.
    pub cells: &'a [char],
    /// Where the candidate starts in `cells`.
    pub pos: usize,
    pub state: State,
    /// Whether the input may be the start of a word still being typed (liblouis's
    /// `partialTrans` mode), so no word can be taken to end with it.
    pub partial: bool,
}

fn last_char(output: &[ResolvedTranslation]) -> Option<char> {
    output.iter().rev().find_map(|t| t.output().chars().last())
}

impl Situation<'_> {
    fn previous_rule(&self) -> Option<&Rule> {
        self.output.last().and_then(|t| t.rule())
    }
}

/// liblouis's `noContractMode`: set by a letsign or nocontractsign, lapsed by the first
/// rule after it that is not a letter, and cleared by a space.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub enum NoContract {
    #[default]
    Off,
    Active,
    Lapsed,
}

/// The part of liblouis's `TranslationContext` that rule selection depends on.
#[derive(Debug, Default, Clone, Copy)]
pub struct State {
    /// liblouis's `itsANumber`: 0 outside a number, 1 right after a number sign, 2 after
    /// a digit.
    number: u8,
    no_contract: NoContract,
    /// liblouis's `nextUpper`: capitalize the next character output.
    next_upper: bool,
    /// liblouis's `allUpper`: 1 right after a begcapsword, 2 once a capital has been
    /// output.
    all_upper: u8,
    /// liblouis's `allUpperPhrase`.
    all_upper_phrase: bool,
}

#[derive(Debug, Default)]
pub struct BackwardSelector {
    text_classes: CharacterClasses,
    defined_chars: HashSet<char>,
    dots_classes: CharacterClasses,
    defined_cells: HashSet<char>,
    cells: HashMap<char, CellRules>,
    numeric_mode_chars: HashSet<char>,
    caps_mode_chars: HashSet<char>,
    /// The uppercase character a table derives from a character with `base uppercase`.
    uppercase: HashMap<char, char>,
}

fn is_character_definition(rule: &Rule) -> bool {
    matches!(
        rule,
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
}

fn with_classes(rule: &Rule) -> &[WithClass] {
    match rule {
        Rule::Always { with_classes, .. }
        | Rule::Word { with_classes, .. }
        | Rule::Sufword { with_classes, .. }
        | Rule::Prfword { with_classes, .. }
        | Rule::Begword { with_classes, .. }
        | Rule::Begmidword { with_classes, .. }
        | Rule::Midword { with_classes, .. }
        | Rule::Midendword { with_classes, .. }
        | Rule::Endword { with_classes, .. }
        | Rule::Partword { with_classes, .. } => with_classes,
        _ => &[],
    }
}

/// How many characters or cells a `context` rule's test starts with literally, as
/// liblouis's `passFindCharacters` counts them. liblouis files a `context` rule that has
/// some among the other rules for them.
fn leading_cells(tests: &[TestInstruction], lookback: &mut usize) -> Option<usize> {
    for test in tests {
        match test {
            TestInstruction::String { s } => {
                let count = s.chars().count();
                if count > *lookback {
                    return Some(count - *lookback);
                }
                *lookback -= count;
            }
            TestInstruction::Dots { dots } => {
                let count = dots.len();
                if count > *lookback {
                    return Some(count - *lookback);
                }
                *lookback -= count;
            }
            TestInstruction::Attributes { quantifier, .. }
            | TestInstruction::Class { quantifier, .. } => {
                let count = match quantifier {
                    None => 1,
                    Some(Quantifier::Number(n)) => *n as usize,
                    _ => return Some(0),
                };
                if count > *lookback {
                    return Some(0);
                }
                *lookback -= count;
            }
            TestInstruction::Lookback { len } => *lookback += *len as usize,
            TestInstruction::Replace { tests } => {
                if let Some(count) = leading_cells(tests, lookback) {
                    return Some(count);
                }
            }
            TestInstruction::GroupStart { .. } | TestInstruction::GroupEnd { .. } => {
                return Some(0);
            }
            TestInstruction::Variable { .. }
            | TestInstruction::Negate { .. }
            | TestInstruction::AtBeginning
            | TestInstruction::AtEnd => {}
        }
    }
    None
}

pub fn context_cells(rule: &Rule) -> usize {
    match rule {
        Rule::Context { test, .. } => leading_cells(test.tests(), &mut 0).unwrap_or(0),
        _ => 0,
    }
}

impl BackwardSelector {
    /// `single_cell_rules` are the translations of every rule whose dots are one cell,
    /// keyed by that cell.
    pub fn new<'a>(
        rules: &[AnchoredRule],
        ctx: &TableContext,
        single_cell_rules: impl Iterator<Item = (char, &'a [ResolvedTranslation])>,
    ) -> Self {
        let text_classes = ctx.character_classes().clone();
        let defined_chars = text_classes.members();

        // liblouis gives a cell the attributes of the characters defined as that single
        // cell, and nothing for the contractions it stands for.
        let mut dots_classes = CharacterClasses::default();
        for rule in rules {
            let (classes, dots): (&[CharacterClass], _) = match &rule.rule {
                Rule::Space { dots, .. } => (&[CharacterClass::Space], dots),
                Rule::Punctuation { dots, .. } => (&[CharacterClass::Punctuation], dots),
                Rule::Digit { dots, .. } => (&[CharacterClass::Digit], dots),
                Rule::Litdigit { dots, .. } => (&[CharacterClass::Litdigit], dots),
                Rule::Letter { dots, .. } => (&[CharacterClass::Letter], dots),
                Rule::Lowercase { dots, .. } => {
                    (&[CharacterClass::Lowercase, CharacterClass::Letter], dots)
                }
                Rule::Uppercase { dots, .. } => {
                    (&[CharacterClass::Uppercase, CharacterClass::Letter], dots)
                }
                Rule::Sign { dots, .. } => (&[CharacterClass::Sign], dots),
                Rule::Math { dots, .. } => (&[CharacterClass::Math], dots),
                Rule::Attribute { name, chars } | Rule::Class { name, chars } => {
                    let class = CharacterClass::from(name.as_str());
                    for c in chars.chars() {
                        dots_classes.insert_associated_dot(
                            class.clone(),
                            c,
                            ctx.character_definitions(),
                        );
                    }
                    continue;
                }
                _ => continue,
            };
            for class in classes {
                dots_classes.insert_dots(class.clone(), dots);
            }
        }
        let mut defined_cells = dots_classes.members();
        let numeric_mode_chars = rules
            .iter()
            .filter_map(|rule| match &rule.rule {
                Rule::Numericmodechars { chars } | Rule::Midendnumericmodechars { chars } => {
                    Some(chars.chars())
                }
                _ => None,
            })
            .flatten()
            .collect();
        let caps_mode_chars = rules
            .iter()
            .filter_map(|rule| match &rule.rule {
                Rule::Capsmodechars { chars } => Some(chars.chars()),
                _ => None,
            })
            .flatten()
            .collect();
        // the first base rule for a character is the one that counts
        let mut based = HashSet::new();
        let uppercase = rules
            .iter()
            .filter_map(|rule| match &rule.rule {
                Rule::Base {
                    name,
                    derived,
                    base,
                } if based.insert(*derived) && name == "uppercase" => Some((*base, *derived)),
                _ => None,
            })
            .collect();

        let mut cells = HashMap::new();
        for (cell, translations) in single_cell_rules {
            defined_cells.insert(cell);
            let mut info = CellRules::default();
            for t in translations {
                match t.rule() {
                    Some(Rule::Begword { .. } | Rule::Midword { .. }) => {}
                    Some(Rule::Postpunc { .. }) => info.postpunc = true,
                    Some(Rule::Hyphen { .. }) => info.hyphen = true,
                    _ => {}
                }
                if t.output().chars().count() > 1
                    && !matches!(t.rule(), Some(Rule::Begword { .. } | Rule::Midword { .. }))
                {
                    info.translation = true;
                }
            }
            cells.insert(cell, info);
        }

        Self {
            text_classes,
            defined_chars,
            dots_classes,
            defined_cells,
            cells,
            numeric_mode_chars,
            caps_mode_chars,
            uppercase,
        }
    }

    pub fn to_uppercase(&self, c: char) -> String {
        match self.uppercase.get(&c) {
            Some(upper) => upper.to_string(),
            None => c.to_uppercase().collect(),
        }
    }

    /// Update `state` before selecting a rule, as the main loop of liblouis's
    /// `backTranslateString` does: a number ends at the first character that cannot
    /// continue it.
    pub fn begin(&self, state: &mut State, output: &[ResolvedTranslation]) {
        let before = last_char(output);
        let caps_mode = before.is_some_and(|c| self.caps_mode_chars.contains(&c));
        if state.all_upper == 1 && self.char_is(before, &[CharacterClass::Uppercase]) {
            state.all_upper = 2;
        } else if state.all_upper == 2
            && !self.char_is(before, &[CharacterClass::Uppercase])
            && !caps_mode
        {
            state.all_upper = 0;
        }
        if state.number == 2
            && let Some(before) = last_char(output)
            && !self.char_is(Some(before), &[CharacterClass::Litdigit])
            && !self.numeric_mode_chars.contains(&before)
        {
            state.number = 0;
        }
    }

    /// Update `state` after `rule` has been applied.
    pub fn applied(&self, state: &mut State, rule: Option<&Rule>) {
        let Some(rule) = rule else {
            return;
        };
        if let Rule::Multind { names, .. } = rule {
            for name in names {
                Self::indicator_applied(state, name);
            }
            return;
        }
        let name = match rule {
            Rule::Capsletter { .. } => "capsletter",
            Rule::Begcapsword { .. } => "begcapsword",
            Rule::Endcapsword { .. } => "endcapsword",
            Rule::Begcaps { .. } => "begcaps",
            Rule::Endcaps { .. } => "endcaps",
            Rule::Letsign { .. } => "letsign",
            Rule::Nonumsign { .. } => "nonumsign",
            Rule::Nocontractsign { .. } => "nocontractsign",
            Rule::Numsign { .. } => "numsign",
            Rule::Begcomp { .. } => "begcomp",
            Rule::Endcomp { .. }
            | Rule::Emphletter { .. }
            | Rule::Begemphword { .. }
            | Rule::Endemphword { .. }
            | Rule::Begemph { .. }
            | Rule::Endemph { .. }
            | Rule::Begemphphrase { .. }
            | Rule::Endemphphrase { .. } => return,
            _ => "",
        };
        if !name.is_empty() {
            Self::indicator_applied(state, name);
            return;
        }
        match rule {
            Rule::Litdigit { .. } | Rule::Begnum { .. } => {
                state.number = if matches!(rule, Rule::Litdigit { .. }) {
                    2
                } else {
                    1
                }
            }
            Rule::Endnum { .. } => state.number = 0,
            Rule::Space { .. } => {
                state.no_contract = NoContract::Off;
                state.number = 0;
                state.all_upper = 0;
                state.next_upper = false;
            }
            _ => {}
        }
        if state.no_contract == NoContract::Active
            && !matches!(
                rule,
                Rule::Letter { .. } | Rule::Uppercase { .. } | Rule::Lowercase { .. }
            )
        {
            state.no_contract = NoContract::Lapsed;
        }
    }

    /// liblouis's `putchars`: capitalize `output` as the caps state asks.
    pub fn capitalize(&self, state: &mut State, output: &str) -> String {
        let mut chars = output.chars();
        let mut result = String::new();
        if state.next_upper
            && let Some(c) = chars.next()
        {
            result.push_str(&self.to_uppercase(c));
            state.next_upper = false;
        }
        if state.all_upper == 0 && !state.all_upper_phrase {
            result.extend(chars);
            return result;
        }
        while let Some(c) = chars.next() {
            if state.all_upper == 2
                && !state.all_upper_phrase
                && !self.char_is(Some(c), &[CharacterClass::Letter])
                && !self.caps_mode_chars.contains(&c)
            {
                state.all_upper = 0;
                result.push(c);
                result.extend(chars);
                return result;
            }
            result.push_str(&self.to_uppercase(c));
        }
        result
    }

    fn indicator_applied(state: &mut State, name: &str) {
        match name {
            "capsletter" => {
                state.next_upper = true;
                state.all_upper = 0;
            }
            "begcapsword" => state.all_upper = 1,
            "endcapsword" => state.all_upper = 0,
            "begcaps" => state.all_upper_phrase = true,
            "endcaps" => state.all_upper_phrase = false,
            _ => {}
        }
        match name {
            "capsletter" | "begcapsword" | "endcapsword" | "begcaps" | "endcaps" | "begcomp" => {
                state.number = 0
            }
            "letsign" | "nonumsign" | "nocontractsign" => {
                state.no_contract = NoContract::Active;
                state.number = 0;
            }
            "numsign" => {
                state.number = 1;
                state.all_upper = 0;
            }
            _ => {}
        }
    }

    /// Whether the text character `c` (`None` before the start of the text) is in any of
    /// `classes`. A character the table does not define counts as a space, as in liblouis.
    fn char_is(&self, c: Option<char>, classes: &[CharacterClass]) -> bool {
        match c {
            Some(c) if self.defined_chars.contains(&c) => classes
                .iter()
                .any(|class| self.text_classes.contains(class, c)),
            _ => classes.contains(&CharacterClass::Space),
        }
    }

    /// Like [`Self::char_is`], for a braille cell.
    fn cell_is(&self, c: Option<char>, classes: &[CharacterClass]) -> bool {
        match c {
            Some(c) if self.defined_cells.contains(&c) => classes
                .iter()
                .any(|class| self.dots_classes.contains(class, c)),
            _ => classes.contains(&CharacterClass::Space),
        }
    }

    /// liblouis's `isEndWord`: whether the word ends before the cell at `from`. Cells
    /// that are neither letters nor stand for more than one character are skipped over.
    fn is_end_word(&self, cells: &[char], from: usize) -> bool {
        for &cell in cells.iter().skip(from) {
            if self.cell_is(Some(cell), &[CharacterClass::Space]) {
                return true;
            }
            if self.cell_is(Some(cell), &[CharacterClass::Letter]) {
                return false;
            }
            let info = self.cells.get(&cell).copied().unwrap_or_default();
            if info.hyphen {
                return true;
            }
            if info.translation && !info.postpunc {
                return false;
            }
        }
        true
    }

    /// liblouis's `isBegWord`: whether the text produced so far ends at the start of a word.
    fn is_beg_word(&self, output: &[ResolvedTranslation]) -> bool {
        for c in output.iter().rev().flat_map(|t| {
            let chars: Vec<char> = t.output().chars().collect();
            chars.into_iter().rev()
        }) {
            if self.char_is(Some(c), &[CharacterClass::Space]) {
                return true;
            }
            if self.char_is(
                Some(c),
                &[
                    CharacterClass::Letter,
                    CharacterClass::Digit,
                    CharacterClass::Math,
                    CharacterClass::Sign,
                ],
            ) {
                return false;
            }
        }
        true
    }

    /// Whether `candidate`, found at `situation.pos`, may be applied there.
    pub fn is_valid(&self, candidate: &ResolvedTranslation, situation: &Situation) -> bool {
        use CharacterClass::*;
        let Some(rule) = candidate.rule() else {
            return true;
        };
        let before = last_char(situation.output);
        let end = situation.pos + candidate.length();
        let after = situation.cells.get(end).copied();
        let before_is = |classes: &[CharacterClass]| self.char_is(before, classes);
        let after_is = |classes: &[CharacterClass]| self.cell_is(after, classes);
        let end_word = || !situation.partial && self.is_end_word(situation.cells, end);

        for with in with_classes(rule) {
            let ok = match with {
                WithClass::After { class } => before_is(&[CharacterClass::from(class.as_str())]),
                WithClass::Before { class } => after_is(&[CharacterClass::from(class.as_str())]),
            };
            if !ok {
                return false;
            }
        }

        let number = situation.state.number != 0;
        let no_contract = situation.state.no_contract;
        if candidate.length() > 1
            && number
            && self.cell_is(situation.cells.get(situation.pos).copied(), &[Litdigit])
        {
            return false;
        }
        let whole_word_blocked = no_contract != NoContract::Off || number;
        match rule {
            Rule::Litdigit { .. } => number,
            Rule::Nocontractsign { .. } => after_is(&[Letter, Sign]),
            Rule::Letsign { .. } | Rule::Nonumsign { .. } => {
                !before_is(&[Letter]) && after_is(&[Letter, Sign])
            }
            Rule::Word { .. } => {
                !situation.partial
                    && !whole_word_blocked
                    && before_is(&[Space, Punctuation])
                    && (after_is(&[Space]) || end_word())
            }
            Rule::Lowword { .. } => {
                !situation.partial
                    && before_is(&[Space])
                    && after_is(&[Space])
                    && !matches!(situation.previous_rule(), Some(Rule::Joinword { .. }))
            }
            Rule::Joinword { .. } | Rule::Joinnum { .. } => {
                before_is(&[Space, Punctuation]) && (!after_is(&[Space]) || situation.partial)
            }
            Rule::Sufword { .. } => !whole_word_blocked && before_is(&[Space, Punctuation]),
            Rule::Prfword { .. } => {
                !whole_word_blocked && before_is(&[Space, Letter, Punctuation]) && end_word()
            }
            Rule::Begword { .. } => {
                no_contract != NoContract::Active && before_is(&[Space, Punctuation]) && !end_word()
            }
            Rule::Begmidword { .. } => before_is(&[Letter, Space, Punctuation]) && !end_word(),
            Rule::Partword { .. } => {
                !before_is(&[Litdigit]) && (before_is(&[Letter]) || !end_word())
            }
            Rule::Midword { .. } => before_is(&[Letter]) && !end_word(),
            Rule::Midendword { .. } => before_is(&[Letter]),
            Rule::Endword { .. } => before_is(&[Letter]) && end_word(),
            Rule::Begnum { .. } => before_is(&[Space, Punctuation]) && after_is(&[Litdigit, Sign]),
            Rule::Midnum { .. } => before_is(&[Digit]) && after_is(&[Litdigit]),
            Rule::Endnum { .. } => number && !after_is(&[Litdigit]),
            Rule::Decpoint { .. } => after_is(&[Digit, Litdigit]),
            Rule::Prepunc { .. } => self.is_beg_word(situation.output),
            Rule::Postpunc { .. } => end_word(),
            Rule::Always { .. } => {
                !(before_is(&[Litdigit])
                    && after_is(&[Litdigit])
                    && candidate.output().chars().count() > 1)
            }
            Rule::Syllable { .. } | Rule::Match { .. } => false,
            Rule::Context { .. } => context_cells(rule) > 0,
            _ => true,
        }
    }

    /// The order liblouis tries the rules for a position in: first those of more than one
    /// cell, by the length of their dots and characters together, `always` rules after the
    /// others; then those of one cell, character definitions after the others, by the
    /// length of their characters. Where liblouis would fall back on the order the rules
    /// are defined in, the longer match wins instead (see the ADR "Rule selection does not
    /// depend on table order").
    pub fn order(candidate: &ResolvedTranslation) -> (bool, usize, usize, usize) {
        let dots = candidate.length();
        let rule = candidate.rule();
        // liblouis stores the opcodes of a multind rule as its characters
        let chars = match rule {
            Some(Rule::Multind { names, .. }) => names.len(),
            Some(rule @ Rule::Context { .. }) => {
                let cells = context_cells(rule);
                return if cells > 1 {
                    (true, 2 * cells, 1, cells)
                } else {
                    (false, 1, cells, 0)
                };
            }
            _ => candidate.output().chars().count(),
        };
        if dots > 1 {
            let always = matches!(rule, Some(Rule::Always { .. }));
            (true, dots + chars, usize::from(!always), dots)
        } else {
            let definition = rule.is_some_and(is_character_definition);
            (false, usize::from(!definition), chars, 0)
        }
    }
}
