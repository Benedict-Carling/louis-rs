pub mod backward;
pub mod display;
pub mod multipass;
pub mod primary;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::{
    parser::{AnchoredRule, CharacterClass, CharacterClasses, Rule},
    translator::{
        CharacterDefinition, TranslationError, swap::SwapClasses, translation::CharacterDots,
    },
};

/// The `base` rules liblouis keeps, as (attribute, derived, base): a character defined
/// before its base rule keeps its own definition instead
pub fn kept_bases(rules: &[AnchoredRule]) -> Vec<(String, char, char)> {
    let mut defined_before = HashSet::new();
    let mut bases = Vec::new();
    for rule in rules {
        match &rule.rule {
            Rule::Base {
                name,
                derived,
                base,
            } if !defined_before.contains(derived) => {
                bases.push((name.clone(), *derived, *base));
            }
            Rule::Space { character, .. }
            | Rule::Punctuation { character, .. }
            | Rule::Digit { character, .. }
            | Rule::Litdigit { character, .. }
            | Rule::Letter { character, .. }
            | Rule::Lowercase { character, .. }
            | Rule::Uppercase { character, .. }
            | Rule::Sign { character, .. }
            | Rule::Math { character, .. } => {
                defined_before.insert(*character);
            }
            _ => {}
        }
    }
    bases
}

#[derive(Debug, Default)]
pub struct TableContext {
    character_definitions: CharacterDefinition,
    character_classes: CharacterClasses,
    dots_classes: CharacterClasses,
    /// The classes of only the rules for the translation direction, where they differ from
    /// `character_classes` and `dots_classes` (see [`Self::with_classes_of`])
    direction_character_classes: Option<CharacterClasses>,
    direction_dots_classes: Option<CharacterClasses>,
    swap_classes: SwapClasses,
    seq_after_patterns: Vec<String>,
    /// liblouis's `toLowercase`: each capital a kept `base uppercase` rule derives, mapped
    /// to its base
    case_folding: HashMap<char, char>,
    character_dots: Arc<CharacterDots>,
}

impl TableContext {
    // used for testing
    pub fn new(
        character_classes: CharacterClasses,
        dots_classes: CharacterClasses,
        swap_classes: SwapClasses,
    ) -> Self {
        Self {
            character_definitions: CharacterDefinition::default(),
            character_classes,
            dots_classes,
            swap_classes,
            seq_after_patterns: Vec::new(),
            case_folding: HashMap::new(),
            direction_character_classes: None,
            direction_dots_classes: None,
            character_dots: Arc::default(),
        }
    }

    pub fn character_definitions(&self) -> &CharacterDefinition {
        &self.character_definitions
    }

    pub fn character_classes(&self) -> &CharacterClasses {
        &self.character_classes
    }

    pub fn dots_classes(&self) -> &CharacterClasses {
        &self.dots_classes
    }

    pub fn swap_classes(&self) -> &SwapClasses {
        &self.swap_classes
    }

    pub fn character_dots(&self) -> &Arc<CharacterDots> {
        &self.character_dots
    }

    /// Each character's dots by the rule liblouis takes as its definition
    pub fn definitions(&self) -> &CharacterDefinition {
        self.character_dots.definitions()
    }

    /// This context with the character and dots classes of `other`
    pub fn with_classes_of(self, other: &TableContext) -> Self {
        Self {
            direction_character_classes: Some(self.character_classes),
            character_classes: other.character_classes.clone(),
            direction_dots_classes: Some(self.dots_classes),
            dots_classes: other.dots_classes.clone(),
            ..self
        }
    }

    pub fn direction_character_classes(&self) -> &CharacterClasses {
        self.direction_character_classes
            .as_ref()
            .unwrap_or(&self.character_classes)
    }

    pub fn direction_dots_classes(&self) -> &CharacterClasses {
        self.direction_dots_classes
            .as_ref()
            .unwrap_or(&self.dots_classes)
    }

    pub fn case_folding(&self) -> &HashMap<char, char> {
        &self.case_folding
    }

    pub fn seq_after_patterns(&self) -> &[String] {
        &self.seq_after_patterns
    }

    pub fn compile(rules: &[AnchoredRule]) -> Result<TableContext, TranslationError> {
        let mut character_definitions = CharacterDefinition::new();
        let mut character_classes = CharacterClasses::default();
        let mut dots_classes = CharacterClasses::default();
        let mut swap_classes = SwapClasses::default();
        let mut seq_after_patterns = Vec::new();
        let mut undefined = None;

        // First pass: everything that later rules need to *look up*. These include
        // character definitions (`space`, `letter`, `digit`, …), because `Rule::Base` and
        // the attribute opcodes in the second pass resolve a character to its dots
        // through them, and also the letter dots class, because a boundary check in
        // backward translation asks whether a neighbouring *cell* is a letter and a
        // contraction's cell only qualifies via the rule that produced it.
        for rule in rules {
            match &rule.rule {
                Rule::Space {
                    character, dots, ..
                } => {
                    character_definitions.insert(*character, &dots.to_string());
                    character_classes.insert(CharacterClass::Space, *character);
                    dots_classes.insert_dots(CharacterClass::Space, dots);
                }
                Rule::Punctuation {
                    character, dots, ..
                } => {
                    character_definitions.insert(*character, &dots.to_string());
                    character_classes.insert(CharacterClass::Punctuation, *character);
                    dots_classes.insert_dots(CharacterClass::Punctuation, dots);
                }
                Rule::Digit {
                    character, dots, ..
                } => {
                    character_definitions.insert(*character, &dots.to_string());
                    character_classes.insert(CharacterClass::Digit, *character);
                    dots_classes.insert_dots(CharacterClass::Digit, dots);
                }
                Rule::Litdigit {
                    character, dots, ..
                } => {
                    character_definitions.insert(*character, &dots.to_string());
                    character_classes.insert(CharacterClass::Litdigit, *character);
                    dots_classes.insert_dots(CharacterClass::Litdigit, dots);
                }
                Rule::Letter {
                    character, dots, ..
                } => {
                    character_definitions.insert(*character, &dots.to_string());
                    character_classes.insert(CharacterClass::Letter, *character);
                    dots_classes.insert_dots(CharacterClass::Letter, dots);
                }
                Rule::Lowercase {
                    character, dots, ..
                } => {
                    character_definitions.insert(*character, &dots.to_string());
                    character_classes.insert(CharacterClass::Lowercase, *character);
                    character_classes.insert(CharacterClass::Letter, *character);
                    dots_classes.insert_dots(CharacterClass::Lowercase, dots);
                    dots_classes.insert_dots(CharacterClass::Letter, dots);
                }
                Rule::Uppercase {
                    character, dots, ..
                } => {
                    character_definitions.insert(*character, &dots.to_string());
                    character_classes.insert(CharacterClass::Uppercase, *character);
                    character_classes.insert(CharacterClass::Letter, *character);
                    dots_classes.insert_dots(CharacterClass::Uppercase, dots);
                    dots_classes.insert_dots(CharacterClass::Letter, dots);
                }
                Rule::Sign {
                    character, dots, ..
                } => {
                    character_definitions.insert(*character, &dots.to_string());
                    character_classes.insert(CharacterClass::Sign, *character);
                    dots_classes.insert_dots(CharacterClass::Sign, dots);
                }
                Rule::Math {
                    character, dots, ..
                } => {
                    character_definitions.insert(*character, &dots.to_string());
                    character_classes.insert(CharacterClass::Math, *character);
                    dots_classes.insert_dots(CharacterClass::Math, dots);
                }
                Rule::Hyphen {
                    character, dots, ..
                } => {
                    character_definitions.insert(*character, &dots.to_string());
                    // FIXME: In what character class is the hyphen (if any)?
                    character_classes.insert(CharacterClass::Punctuation, *character);
                    dots_classes.insert_dots(CharacterClass::Punctuation, dots);
                }
                Rule::Decpoint { character, dots } => {
                    character_definitions.insert(*character, &dots.to_string());
                    // FIXME: In what character class is the decimal point (if any)?
                    character_classes.insert(CharacterClass::Punctuation, *character);
                    dots_classes.insert_dots(CharacterClass::Punctuation, dots);
                }
                _ => (),
            }
        }
        // go through the rules again with all the character_definitions sorted out
        for rule in rules {
            match &rule.rule {
                Rule::Base {
                    derived,
                    base,
                    name,
                } => {
                    if let Some(translation) = character_definitions.get(base).cloned() {
                        if character_definitions.get(derived).is_none() {
                            character_definitions.insert(*derived, &translation);
                        }
                        character_classes.insert(CharacterClass::from(name.as_str()), *derived);
                        // Base-derived uppercase/lowercase characters are letters too,
                        // matching what the `uppercase`/`lowercase` opcodes do directly.
                        if name == "uppercase" || name == "lowercase" {
                            character_classes.insert(CharacterClass::Letter, *derived);
                        }
                        dots_classes.insert_associated_dot(
                            CharacterClass::from(name.as_str()),
                            *base,
                            &character_definitions,
                        );
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
                Rule::Attribute { name, chars } => {
                    let class = CharacterClass::from(name.as_str());
                    for c in chars.chars() {
                        character_classes.insert(class.clone(), c);
                        dots_classes.insert_associated_dot(
                            class.clone(),
                            c,
                            &character_definitions,
                        );
                    }
                }
                Rule::Seqdelimiter { chars } => {
                    for c in chars.chars() {
                        character_classes.insert(CharacterClass::Seqdelimiter, c);
                        dots_classes.insert_associated_dot(
                            CharacterClass::Seqdelimiter,
                            c,
                            &character_definitions,
                        );
                    }
                }
                Rule::Seqbeforechars { chars } => {
                    for c in chars.chars() {
                        character_classes.insert(CharacterClass::Seqbeforechars, c);
                        dots_classes.insert_associated_dot(
                            CharacterClass::Seqbeforechars,
                            c,
                            &character_definitions,
                        );
                    }
                }
                Rule::Seqafterchars { chars } => {
                    for c in chars.chars() {
                        character_classes.insert(CharacterClass::Seqafterchars, c);
                        dots_classes.insert_associated_dot(
                            CharacterClass::Seqafterchars,
                            c,
                            &character_definitions,
                        );
                    }
                }
                Rule::Seqafterpattern { chars } => {
                    seq_after_patterns.push(chars.clone());
                }
                Rule::Undefined { dots } => undefined = Some(dots.to_string()),
                Rule::Swapcc {
                    name,
                    chars,
                    replacement,
                } => {
                    let class = CharacterClass::from(name.as_str());
                    for c in chars.chars() {
                        character_classes.insert(class.clone(), c);
                        dots_classes.insert(class.clone(), c);
                    }
                    let replacements: Vec<String> =
                        replacement.chars().map(|c| c.to_string()).collect();
                    let mapping: Vec<(char, &str)> = chars
                        .chars()
                        .zip(replacements.iter().map(|s| s.as_str()))
                        .collect();
                    swap_classes.insert(name, &mapping);
                }
                Rule::Swapcd { name, chars, dots } => {
                    let class = CharacterClass::from(name.as_str());
                    for c in chars.chars() {
                        character_classes.insert(class.clone(), c);
                        dots_classes.insert(class.clone(), c);
                    }
                    let replacements: Vec<String> = dots.iter().map(|b| b.to_string()).collect();
                    let mapping: Vec<(char, &str)> = chars
                        .chars()
                        .zip(replacements.iter().map(|s| s.as_str()))
                        .collect();
                    swap_classes.insert(name, &mapping);
                }
                Rule::Swapdd {
                    name,
                    dots,
                    replacement,
                } => {
                    let class = CharacterClass::from(name.as_str());
                    for c in dots.to_string().chars() {
                        character_classes.insert(class.clone(), c);
                        dots_classes.insert(class.clone(), c);
                    }
                    let replacements: Vec<String> =
                        replacement.iter().map(|b| b.to_string()).collect();
                    let mapping: Vec<(char, &str)> = dots
                        .iter()
                        .map(|b| b.to_unicode())
                        .zip(replacements.iter().map(|s| s.as_str()))
                        .collect();
                    swap_classes.insert(name, &mapping);
                }
                _ => (),
            }
        }
        let case_folding = kept_bases(rules)
            .into_iter()
            .filter(|(name, _, _)| name == "uppercase")
            .map(|(_, derived, base)| (derived, base))
            .collect();
        for (n, class) in user_classes(rules).into_iter().take(4).enumerate() {
            let ordinal = CharacterClass::Ordinal(n as u8 + 1);
            for c in character_classes.get(&class).unwrap_or_default() {
                character_classes.insert(ordinal.clone(), c);
            }
            for c in dots_classes.get(&class).unwrap_or_default() {
                dots_classes.insert(ordinal.clone(), c);
            }
        }
        Ok(TableContext {
            case_folding,
            character_dots: Arc::new(CharacterDots::new(definition_rules(rules), undefined)),
            character_definitions,
            character_classes,
            dots_classes,
            swap_classes,
            seq_after_patterns,
            direction_character_classes: None,
            direction_dots_classes: None,
        })
    }
}

/// The dots of each character by the rule liblouis takes as its definition: the first of its
/// character definition opcodes, of which `litdigit` is none, else its base's
fn definition_rules(rules: &[AnchoredRule]) -> CharacterDefinition {
    let mut definitions = CharacterDefinition::new();
    for rule in rules {
        match &rule.rule {
            Rule::Space {
                character, dots, ..
            }
            | Rule::Punctuation {
                character, dots, ..
            }
            | Rule::Digit {
                character, dots, ..
            }
            | Rule::Letter {
                character, dots, ..
            }
            | Rule::Lowercase {
                character, dots, ..
            }
            | Rule::Uppercase {
                character, dots, ..
            }
            | Rule::Sign {
                character, dots, ..
            }
            | Rule::Math {
                character, dots, ..
            } if definitions.get(character).is_none() => {
                definitions.insert(*character, &dots.to_string());
            }
            _ => (),
        }
    }
    for rule in rules {
        if let Rule::Base { derived, base, .. } = &rule.rule
            && definitions.get(derived).is_none()
            && let Some(dots) = definitions.get(base).cloned()
        {
            definitions.insert(*derived, &dots);
        }
    }
    definitions
}

/// The classes a table defines beyond liblouis' predefined ones, in the order liblouis
/// allocates their attributes
fn user_classes(rules: &[AnchoredRule]) -> Vec<CharacterClass> {
    let mut classes: Vec<CharacterClass> = Vec::new();
    for rule in rules {
        let name = match &rule.rule {
            Rule::Attribute { name, .. } if !name.starts_with(|c: char| c.is_ascii_digit()) => name,
            Rule::Class { name, .. }
            | Rule::Base { name, .. }
            | Rule::Modeletter { name, .. }
            | Rule::Begmodeword { name, .. }
            | Rule::Endmodeword { name, .. }
            | Rule::Begmode { name, .. }
            | Rule::Endmode { name, .. }
            | Rule::Begmodephrase { name, .. }
            | Rule::Endmodephrase { name, .. }
            | Rule::Lenmodephrase { name, .. } => name,
            _ => continue,
        };
        if matches!(
            name.as_str(),
            "space"
                | "letter"
                | "digit"
                | "punctuation"
                | "uppercase"
                | "lowercase"
                | "math"
                | "sign"
                | "litdigit"
        ) {
            continue;
        }
        let class = CharacterClass::from(name.as_str());
        if !classes.contains(&class) {
            classes.push(class);
        }
    }
    classes
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    use crate::parser::RuleParser;

    fn parse_rule(source: &str) -> AnchoredRule {
        RuleParser::new(source).rule().unwrap().into()
    }

    #[test]
    fn ordinal_classes() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("attribute 1 a"),
            parse_rule("attribute letter b"),
            parse_rule("attribute foo a"),
            parse_rule("base uppercase A a"),
            parse_rule("attribute bar b"),
            parse_rule("attribute foo c"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        assert_eq!(
            context.character_classes.get(&CharacterClass::Ordinal(1)),
            Some(HashSet::from(['a', 'c']))
        );
        assert_eq!(
            context.character_classes.get(&CharacterClass::Ordinal(2)),
            Some(HashSet::from(['b']))
        );
        assert_eq!(
            context.character_classes.get(&CharacterClass::Ordinal(3)),
            None
        );
    }

    #[test]
    fn character_classes() {
        let rules = [
            parse_rule("lowercase a 1"),
            parse_rule("lowercase b 12"),
            parse_rule("lowercase c 14"),
            parse_rule("base uppercase A a"),
            parse_rule("base uppercase B b"),
            parse_rule("space \\s 0"),
            parse_rule("litdigit 1 1"),
            parse_rule("attribute foo abc"),
            parse_rule("seqbeforechars xyz"),
        ];
        let context = TableContext::compile(&rules).unwrap();

        assert_eq!(
            context.character_classes.get(&CharacterClass::Letter),
            Some(HashSet::from(['a', 'b', 'c', 'A', 'B']))
        );
        assert_eq!(
            context.character_classes.get(&CharacterClass::Lowercase),
            Some(HashSet::from(['a', 'b', 'c']))
        );
        assert_eq!(
            context.character_classes.get(&CharacterClass::Uppercase),
            Some(HashSet::from(['A', 'B']))
        );
        assert_eq!(
            context.character_classes.get(&CharacterClass::Space),
            Some(HashSet::from([' ']))
        );
        assert_eq!(
            context.character_classes.get(&CharacterClass::Litdigit),
            Some(HashSet::from(['1']))
        );
        assert_eq!(
            context
                .character_classes
                .get(&CharacterClass::Seqbeforechars),
            Some(HashSet::from(['x', 'y', 'z']))
        );
        assert_eq!(
            context
                .character_classes
                .get(&CharacterClass::UserDefined("foo".to_string())),
            Some(HashSet::from(['a', 'b', 'c']))
        );
    }

    #[test]
    fn dots_classes() {
        let rules = [
            parse_rule("lowercase o 135"),
            parse_rule("lowercase ύ 5-13456"),
            parse_rule("sign ΄ 5"),
            parse_rule("attribute accent ΄"),
            parse_rule("pass2 @135[%accent]@13456 *@136"),
        ];
        let context = TableContext::compile(&rules).unwrap();

        assert_eq!(
            context.dots_classes.get(&CharacterClass::Letter),
            Some(HashSet::from(['⠕']))
        );
        assert_eq!(
            context.dots_classes.get(&CharacterClass::Lowercase),
            Some(HashSet::from(['⠕']))
        );
        assert_eq!(
            context.dots_classes.get(&CharacterClass::Sign),
            Some(HashSet::from(['⠐']))
        );
        assert_eq!(context.dots_classes.get(&CharacterClass::Uppercase), None);
        assert_eq!(context.dots_classes.get(&CharacterClass::Space), None);
        assert_eq!(context.dots_classes.get(&CharacterClass::Litdigit), None);
        assert_eq!(
            context.dots_classes.get(&CharacterClass::Seqbeforechars),
            None
        );
        assert_eq!(
            context
                .dots_classes
                .get(&CharacterClass::UserDefined("accent".to_string())),
            Some(HashSet::from(['⠐']))
        );
    }

    #[test]
    fn dots_classes_includes_base_attribute() {
        let rules = [
            parse_rule("lowercase b 12"),
            parse_rule("base uppercase B b"),
        ];
        let context = TableContext::compile(&rules).unwrap();

        assert_eq!(
            context.dots_classes.get(&CharacterClass::Uppercase),
            Some(HashSet::from(['⠃']))
        );
    }

    #[test]
    fn dots_classes_leave_out_contractions() {
        // like liblouis's dots attributes, only character definitions classify a cell
        let rules = [
            parse_rule("lowercase h 125"),
            parse_rule("always le 246"),
            parse_rule("always sz 234-1235"),
        ];
        let context = TableContext::compile(&rules).unwrap();
        assert_eq!(
            context.dots_classes.get(&CharacterClass::Letter),
            Some(HashSet::from(['⠓']))
        );
    }

    #[test]
    fn classes_of_rules_for_the_other_direction() {
        let all = [parse_rule("lowercase a 1"), parse_rule("noback sign ⠠ 6")];
        let backward = [parse_rule("lowercase a 1")];
        let context = TableContext::compile(&backward)
            .unwrap()
            .with_classes_of(&TableContext::compile(&all).unwrap());
        assert!(
            context
                .dots_classes()
                .get(&CharacterClass::Sign)
                .is_some_and(|cells| cells.contains(&'⠠'))
        );
        assert_eq!(
            context.direction_dots_classes().get(&CharacterClass::Sign),
            None
        );
    }
}
