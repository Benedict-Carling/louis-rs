//! Knuth-Liang hyphenation, parsed from liblouis's `.dic`
//! pattern-dictionary format.
//!
//! A table answers one question -- where in this word may a break occur?
//! -- and is identified purely by whichever `.dic` file it was parsed
//! from, exactly like every other liblouis table type. Implemented
//! natively rather than via the `hyphenation` crate, since we only need
//! its matching algorithm, not its bundled pattern corpus or its
//! print-text case-folding.

use std::collections::{HashMap, HashSet};

/// A parsed hyphenation table.
#[derive(PartialEq, Debug, Default, Clone)]
pub struct HyphenationTable {
    // Key: a pattern's letters, e.g. "ab" for the source pattern ".ab3a"
    // (the leading '.' is a literal word-boundary marker character, kept
    // as part of the key).
    // Value: the score at each gap around those letters -- one more
    // entry than there are characters in the key.
    patterns: HashMap<String, Vec<u8>>,
    // Every prefix of a pattern's letters: the states of liblouis's pattern automaton.
    prefixes: HashSet<String>,
}

/// A bad `LEFTHYPHENMIN`/`RIGHTHYPHENMIN` value is a configuration
/// mistake, not a pattern, so it fails loudly rather than falling back
/// to a default.
#[derive(thiserror::Error, Debug)]
pub enum ParseError {
    #[error("line {line}: expected a number after {directive}, found {found:?}")]
    InvalidHyphenMin {
        line: usize,
        directive: &'static str,
        found: String,
    },
    #[error("the file is neither ISO8859-1 nor valid UTF-8")]
    InvalidUtf8,
}

impl HyphenationTable {
    /// Parse a `.dic` file's bytes, which are Latin-1 when its first line names an `ISO`
    /// encoding, as liblouis reads them, and UTF-8 otherwise.
    pub fn parse_bytes(bytes: &[u8]) -> Result<Self, ParseError> {
        let source = if bytes.trim_ascii_start().starts_with(b"ISO") {
            bytes.iter().map(|&b| char::from(b)).collect()
        } else {
            String::from_utf8(bytes.to_vec()).map_err(|_| ParseError::InvalidUtf8)?
        };
        Self::parse(&source)
    }

    /// Parse a `.dic` hyphenation-pattern file.
    ///
    /// `LEFTHYPHENMIN`/`RIGHTHYPHENMIN` headers are checked but play no part:
    /// liblouis's `hyphenateWord` has no boundary minima.
    pub fn parse(source: &str) -> Result<Self, ParseError> {
        let mut table = HyphenationTable::default();

        for (lineno, raw_line) in source.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() {
                continue;
            }
            // The first line is an encoding marker, the text is already decoded
            if lineno == 0 && (line.starts_with("UTF-8") || line.starts_with("ISO")) {
                continue;
            }
            // Comment lines.
            if line.starts_with('#') || line.starts_with('%') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("LEFTHYPHENMIN") {
                parse_hyphenmin(rest, lineno, "LEFTHYPHENMIN")?;
                continue;
            }
            if let Some(rest) = line.strip_prefix("RIGHTHYPHENMIN") {
                parse_hyphenmin(rest, lineno, "RIGHTHYPHENMIN")?;
                continue;
            }
            let (letters, tally) = parse_pattern(line);
            let chars: Vec<char> = letters.chars().collect();
            for len in 1..=chars.len() {
                table.prefixes.insert(chars[..len].iter().collect());
            }
            table.patterns.insert(letters, tally);
        }

        Ok(table)
    }

    /// liblouis's `hyphenateWord`: the score of the gap before each character of `word`
    /// (index 0 is the gap before the first), odd scores marking breaks.
    ///
    /// liblouis walks the word, wrapped in `.`, through an automaton of the patterns'
    /// prefixes: after each character it is in the longest suffix read so far that is such
    /// a prefix, and only that state's own pattern is applied. The boundary minima play no
    /// part, and the word is expected to be lowercased already.
    pub fn hyphens(&self, word: &[char]) -> Vec<u8> {
        let wrapped: Vec<char> = std::iter::once('.')
            .chain(word.iter().copied())
            .chain(std::iter::once('.'))
            .collect();
        let mut scores = vec![0u8; word.len()];
        let mut state = 0;
        for end in 1..=wrapped.len() {
            state = (end - state - 1..end)
                .find(|&start| {
                    self.prefixes
                        .contains(&wrapped[start..end].iter().collect::<String>())
                })
                .map_or(0, |start| end - start);
            let key: String = wrapped[end - state..end].iter().collect();
            if let Some(tally) = self.patterns.get(&key) {
                // tally[k] scores the gap before wrapped[end - state + k], i.e. before
                // word character end - state + k - 1
                for (k, &v) in tally.iter().enumerate() {
                    let Some(gap) = (end + k).checked_sub(state + 1) else {
                        continue;
                    };
                    if let Some(score) = scores.get_mut(gap)
                        && v > *score
                    {
                        *score = v;
                    }
                }
            }
        }
        scores
    }
}

/// Parse the value after a `LEFTHYPHENMIN`/`RIGHTHYPHENMIN` directive.
fn parse_hyphenmin(
    rest: &str,
    lineno: usize,
    directive: &'static str,
) -> Result<usize, ParseError> {
    rest.trim()
        .parse()
        .map_err(|_| ParseError::InvalidHyphenMin {
            line: lineno + 1,
            directive,
            found: rest.trim().to_string(),
        })
}

/// ".ab3a" -> ("aba", `[0,0,3,0]`) -- letters plus one score per gap
/// (len(letters) + 1 of them). A pattern without a digit breaks nowhere, but liblouis still
/// counts it among the patterns the automaton reads.
fn parse_pattern(pattern: &str) -> (String, Vec<u8>) {
    let mut letters = String::new();
    let mut tally = vec![0u8];
    for ch in pattern.chars() {
        if let Some(d) = ch.to_digit(10) {
            // Scores the gap the last letter already opened, not a new one.
            *tally.last_mut().unwrap() = d as u8;
        } else {
            letters.push(ch);
            tally.push(0);
        }
    }
    (letters, tally)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hyphens(t: &HyphenationTable, word: &str) -> Vec<u8> {
        t.hyphens(&word.chars().collect::<Vec<_>>())
    }

    #[test]
    fn rejects_malformed_header_value() {
        assert!(HyphenationTable::parse("UTF-8\nLEFTHYPHENMIN two\n.ab3a\n").is_err());
    }

    #[test]
    fn reads_iso8859_1_as_latin1() {
        let t = HyphenationTable::parse_bytes(b"ISO8859-1\n.\xe93a\n").unwrap();
        assert_eq!(hyphens(&t, "éa"), vec![0, 3]);
        assert!(HyphenationTable::parse_bytes(b"UTF-8\n.\xe93a\n").is_err());
    }

    #[test]
    fn huge_hyphenmin_is_accepted() {
        assert!(
            HyphenationTable::parse("UTF-8\nLEFTHYPHENMIN 18446744073709551615\n.ab3a\n").is_ok()
        );
    }

    #[test]
    fn finds_a_break() {
        // ".ab3a" -- the '3' sits before the second 'a', so "aba" breaks as "ab|a"
        let t = HyphenationTable::parse("UTF-8\n.ab3a\n").unwrap();
        assert_eq!(hyphens(&t, "aba"), vec![0, 0, 3]);
    }

    #[test]
    fn ignores_minima() {
        let t =
            HyphenationTable::parse("UTF-8\nLEFTHYPHENMIN 2\nRIGHTHYPHENMIN 2\n.ab3a\n").unwrap();
        assert_eq!(hyphens(&t, "aba"), vec![0, 0, 3]);
    }

    #[test]
    fn keeps_the_highest_score() {
        let t = HyphenationTable::parse("UTF-8\na1b\nb2c\nab4c\n").unwrap();
        assert_eq!(hyphens(&t, "abc"), vec![0, 1, 4]);
    }

    /// Only the pattern of the longest matching prefix applies, so a pattern that is a
    /// suffix of a longer prefix goes unused, as in liblouis
    #[test]
    fn applies_only_the_longest_state() {
        let t = HyphenationTable::parse("UTF-8\nb1c\nabcd1\n").unwrap();
        assert_eq!(hyphens(&t, "abce"), vec![0, 0, 0, 0]);
        assert_eq!(hyphens(&t, "xbce"), vec![0, 0, 1, 0]);
    }

    #[test]
    fn no_match_means_no_breaks() {
        let t = HyphenationTable::parse("UTF-8\ns1b\n").unwrap();
        assert_eq!(hyphens(&t, "fff"), vec![0, 0, 0]);
    }

    #[test]
    fn skips_comments_and_blank_lines() {
        let t = HyphenationTable::parse("UTF-8\n# a comment\n\n% also a comment\n.ab3a\n").unwrap();
        assert_eq!(hyphens(&t, "aba"), vec![0, 0, 3]);
    }

    #[test]
    fn empty_word() {
        let t = HyphenationTable::parse("UTF-8\n.ab3a\n").unwrap();
        assert_eq!(hyphens(&t, ""), Vec::<u8>::new());
    }
}
