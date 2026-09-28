//! The emphasis of a back-translation.

use crate::{emphasis::EmphasisSpan, parser::Rule, translator::ResolvedTranslation};

/// The emphasis of a back-translation, read off the emphasis indicators among its
/// `translations` as liblouis's `backTranslateString` fills its typeform buffer: a letter
/// indicator emphasizes the next output, a word indicator the output up to the next space
/// or end indicator, a phrase indicator the output up to its end indicator.
pub fn back_translated(translations: &[ResolvedTranslation]) -> Vec<EmphasisSpan> {
    #[derive(Default)]
    struct Active<'a> {
        word: Vec<&'a str>,
        phrase: Vec<&'a str>,
        next_char: Vec<&'a str>,
    }
    fn remove(classes: &mut Vec<&str>, name: &str) -> bool {
        let len = classes.len();
        classes.retain(|c| *c != name);
        classes.len() != len
    }
    let mut active = Active::default();
    let mut spans: Vec<EmphasisSpan> = Vec::new();
    let mut pos = 0;
    for t in translations {
        match t.rule() {
            Some(Rule::Emphletter { name, .. }) => active.next_char.push(name),
            Some(Rule::Begemphword { name, .. } | Rule::Begemph { name, .. }) => {
                active.word.push(name)
            }
            Some(Rule::Begemphphrase { name, .. }) => active.phrase.push(name),
            Some(Rule::Endemphphrase { name, .. }) => {
                remove(&mut active.phrase, name);
            }
            Some(Rule::Endemphword { name, .. } | Rule::Endemph { name, .. }) => {
                if !remove(&mut active.word, name) {
                    remove(&mut active.phrase, name);
                }
            }
            _ => {
                let len = t.output().chars().count();
                if len > 0 {
                    let mut classes: Vec<&str> = [&active.phrase, &active.word, &active.next_char]
                        .into_iter()
                        .flatten()
                        .copied()
                        .collect();
                    classes.sort();
                    classes.dedup();
                    for class in classes {
                        match spans
                            .iter_mut()
                            .find(|s| s.class == *class && s.range.end == pos)
                        {
                            Some(span) => span.range.end = pos + len,
                            None => spans.push(EmphasisSpan::new(class, pos..pos + len)),
                        }
                    }
                    pos += len;
                    active.next_char.clear();
                }
                if matches!(t.rule(), Some(Rule::Space { .. })) {
                    active.word.clear();
                }
            }
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        parser::{AnchoredRule, RuleParser},
        translator::TranslationStage,
    };

    fn rule(s: &str) -> AnchoredRule {
        AnchoredRule::new(RuleParser::new(s).rule().unwrap(), None, 0)
    }

    fn italic(range: std::ops::Range<usize>) -> Vec<EmphasisSpan> {
        vec![EmphasisSpan::new("italic", range)]
    }

    #[test]
    fn back_translated_emphasis() {
        let letter = ResolvedTranslation::new(
            "",
            "⠨",
            1,
            TranslationStage::Main,
            rule("emphletter italic 46"),
        );
        let text = |s: &str| ResolvedTranslation::new(s, s, 1, TranslationStage::Main, None);
        let spans = back_translated(&[letter, text("a"), text("b")]);
        assert_eq!(spans, italic(0..1));
    }
}
