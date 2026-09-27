//! The capitalization check liblouis's `validMatch` applies to every multi-character
//! forward rule

use std::collections::HashSet;

use crate::parser::CharacterClass;
use crate::translator::table::TableContext;

#[derive(Debug, Clone, Default)]
pub struct CaseClasses {
    letters: HashSet<char>,
    uppercase: HashSet<char>,
    lowercase: HashSet<char>,
}

impl CaseClasses {
    pub fn new(letters: HashSet<char>, uppercase: HashSet<char>, lowercase: HashSet<char>) -> Self {
        Self {
            letters,
            uppercase,
            lowercase,
        }
    }

    pub fn from_context(ctx: &TableContext) -> Self {
        let class = |c| ctx.character_classes().get(&c).unwrap_or_default();
        Self::new(
            class(CharacterClass::Letter),
            class(CharacterClass::Uppercase),
            class(CharacterClass::Lowercase),
        )
    }

    fn attributes(&self, c: char) -> (bool, bool, bool) {
        (
            self.letters.contains(&c),
            self.uppercase.contains(&c),
            self.lowercase.contains(&c),
        )
    }

    /// Whether a rule may match `chars[pos..pos + len]`: the case of its letters must not
    /// change after the second one, so "Cds" matches a rule for "cds" but "CDs" and "cdS" do
    /// not.
    pub fn valid_match(&self, chars: &[char], pos: usize, len: usize) -> bool {
        let end = (pos + len).min(chars.len());
        for k in pos + 2..end {
            let prev = self.attributes(chars[k - 1]);
            let cur = self.attributes(chars[k]);
            let caseless_letter = cur.0 && !cur.1 && !cur.2;
            if !caseless_letter && prev.0 && cur.0 && prev != cur {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes() -> CaseClasses {
        CaseClasses::new(
            HashSet::from(['c', 'd', 's', 'C', 'D', 'S']),
            HashSet::from(['C', 'D', 'S']),
            HashSet::from(['c', 'd', 's']),
        )
    }

    fn valid(s: &str) -> bool {
        let chars: Vec<char> = s.chars().collect();
        classes().valid_match(&chars, 0, chars.len())
    }

    #[test]
    fn same_case_or_capitalized() {
        assert!(valid("cds"));
        assert!(valid("CDS"));
        assert!(valid("Cds"));
        assert!(valid("cD"));
    }

    #[test]
    fn case_change_after_the_second_letter() {
        assert!(!valid("CDs"));
        assert!(!valid("cdS"));
        assert!(!valid("cDs"));
    }
}
