//! Standing-alone sequences
//!
//! A table that defines `seqdelimiter` characters changes what "a whole word" means for the
//! `word` and `contraction` opcodes. Instead of requiring a space or punctuation on either
//! side, liblouis (`inSequence` in `lou_translateString.c`) lets the match be surrounded only
//! by `seqbeforechars` before and `seqafterchars` or `seqafterpattern`s after, up to a space,
//! a `seqdelimiter` or the edge of the input.

use std::collections::HashSet;

use crate::parser::CharacterClass;
use crate::translator::table::TableContext;
use crate::translator::trie::Transition;

/// The lookbehind and lookahead of a `word`/`contraction` rule. With standing-alone
/// sequences these are only a prefilter; [`Sequences::in_sequence`] has the final say.
pub fn whole_word_boundaries(sequences: Option<&Sequences>) -> (Transition, Option<Transition>) {
    match sequences {
        None => (
            Transition::Start(vec![CharacterClass::Space, CharacterClass::Punctuation]),
            Some(Transition::End(vec![
                CharacterClass::Space,
                CharacterClass::Punctuation,
            ])),
        ),
        Some(sequences) => (
            Transition::Start(vec![
                CharacterClass::Space,
                CharacterClass::Seqdelimiter,
                CharacterClass::Seqbeforechars,
            ]),
            sequences.after_classes_suffice().then(|| {
                Transition::End(vec![
                    CharacterClass::Space,
                    CharacterClass::Seqdelimiter,
                    CharacterClass::Seqafterchars,
                ])
            }),
        ),
    }
}

#[derive(Debug, Clone)]
pub struct Sequences {
    space: HashSet<char>,
    delimiter: HashSet<char>,
    before: HashSet<char>,
    after: HashSet<char>,
    after_patterns: Vec<Vec<char>>,
}

impl Sequences {
    /// `None` unless the table defines a `seqdelimiter`, as liblouis only switches to
    /// sequence checks (`usesSequences`) then.
    pub fn new(ctx: &TableContext) -> Option<Self> {
        let cc = ctx.character_classes();
        let class = |class: CharacterClass| -> HashSet<char> {
            cc.get(&class).into_iter().flatten().collect()
        };
        let delimiter = class(CharacterClass::Seqdelimiter);
        if delimiter.is_empty() {
            return None;
        }
        Some(Self {
            space: class(CharacterClass::Space),
            delimiter,
            before: class(CharacterClass::Seqbeforechars),
            after: class(CharacterClass::Seqafterchars),
            after_patterns: ctx
                .seq_after_patterns()
                .iter()
                .map(|p| p.chars().collect())
                .filter(|p: &Vec<char>| !p.is_empty())
                .collect(),
        })
    }

    /// Whether every character that can follow a standing-alone match is in one of the
    /// `Space`, `Seqdelimiter` or `Seqafterchars` classes, i.e. whether those classes are a
    /// sound lookahead prefilter for [`Sequences::in_sequence`].
    pub fn after_classes_suffice(&self) -> bool {
        self.after_patterns.iter().all(|p| {
            let c = p[0];
            self.space.contains(&c) || self.delimiter.contains(&c) || self.after.contains(&c)
        })
    }

    fn ends_sequence(&self, c: char) -> bool {
        self.space.contains(&c) || self.delimiter.contains(&c)
    }

    /// Whether the `len` characters at `start` stand alone.
    pub fn in_sequence(&self, chars: &[char], start: usize, len: usize) -> bool {
        self.before_ok(chars, start) && self.after_ok(chars, start + len)
    }

    fn before_ok(&self, chars: &[char], start: usize) -> bool {
        chars[..start.min(chars.len())]
            .iter()
            .rev()
            .find(|c| !self.before.contains(c))
            .is_none_or(|&c| self.ends_sequence(c))
    }

    fn after_ok(&self, chars: &[char], end: usize) -> bool {
        let mut i = end;
        while i < chars.len() {
            if let Some(p) = self
                .after_patterns
                .iter()
                .find(|p| chars[i..].starts_with(p))
            {
                i += p.len();
                if i >= chars.len() {
                    return true;
                }
            }
            let c = chars[i];
            if self.after.contains(&c) {
                i += 1;
            } else {
                return self.ends_sequence(c);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sequences() -> Sequences {
        Sequences {
            space: HashSet::from([' ']),
            delimiter: HashSet::from(['-']),
            before: HashSet::from(['(', '"']),
            after: HashSet::from([')', '"', '.', '\'']),
            after_patterns: vec!["'s".chars().collect(), "'ll".chars().collect()],
        }
    }

    fn check(input: &str, start: usize, len: usize) -> bool {
        let chars: Vec<char> = input.chars().collect();
        sequences().in_sequence(&chars, start, len)
    }

    #[test]
    fn plain_word() {
        assert!(check("ab", 0, 2));
        assert!(check("x ab y", 2, 2));
        assert!(!check("xab", 1, 2));
        assert!(!check("abx", 0, 2));
    }

    #[test]
    fn before_chars() {
        assert!(check("(\"ab", 2, 2));
        assert!(check("x (ab", 3, 2));
        // punctuation that is not a seqbeforechar does not stand alone
        assert!(!check("!ab", 1, 2));
        assert!(!check("x(ab", 2, 2));
    }

    #[test]
    fn after_chars() {
        assert!(check("ab).", 0, 2));
        assert!(check("ab\" x", 0, 2));
        assert!(!check("ab(", 0, 2));
        assert!(!check("ab)x", 0, 2));
    }

    #[test]
    fn delimiters() {
        assert!(check("x-ab-y", 2, 2));
        assert!(check("(ab)-y", 1, 2));
    }

    #[test]
    fn after_patterns() {
        assert!(check("ab's", 0, 2));
        assert!(check("ab'll x", 0, 2));
        assert!(check("ab's.", 0, 2));
        assert!(!check("ab'sx", 0, 2));
        // `'` is also a seqafterchar, so `'x` is only rejected at the `x`
        assert!(!check("ab'x", 0, 2));
        assert!(check("ab'", 0, 2));
    }
}
