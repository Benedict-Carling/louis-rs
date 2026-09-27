//! Emphasis (italic, bold, underline, …) braille indication.
//!
//! A port of how liblouis marks emphasis (`markEmphases` and the `resolveEmphasis*`
//! functions in `lou_translateString.c`) and inserts its indicators (`insertEmphasesAt`),
//! for the emphasis classes a table declares with `emphclass`. Capitals and the other
//! modes go through the same functions in liblouis, with a character class in place of
//! the caller's typeform; they are handled elsewhere.
//!
//! Marking works on one class at a time. Where the caller's typeform emphasizes the
//! text, it marks the positions where emphasis begins and ends, then, depending on the
//! indicators the class has, turns those into words, passages and single letters. Each
//! position then gets the indicators its marks call for, the classes ordered so that
//! indicators nest.

use std::collections::{HashMap, HashSet};

use crate::{
    emphasis::EmphasisSpan,
    parser::{AnchoredRule, CharacterClass, CharacterClasses, Position, Rule},
    translator::{ResolvedTranslation, TranslationStage, table::TableContext},
};

fn make_translation(dots: &str, origin: &AnchoredRule) -> ResolvedTranslation {
    ResolvedTranslation::new("", dots, 1, TranslationStage::Main, origin.clone())
}

/// liblouis's `EmphasisInfo` for one class: what happens to the emphasis at each
/// position, with one more position for the end of the input.
#[derive(Debug, Clone, Default)]
struct Marks {
    begin: Vec<bool>,
    end: Vec<bool>,
    word: Vec<bool>,
    symbol: Vec<bool>,
}

impl Marks {
    fn new(n: usize) -> Self {
        Marks {
            begin: vec![false; n + 1],
            end: vec![false; n + 1],
            word: vec![false; n + 1],
            symbol: vec![false; n + 1],
        }
    }

    fn any(&self, at: usize) -> bool {
        self.begin[at] || self.end[at] || self.word[at] || self.symbol[at]
    }
}

/// liblouis's `wordBuffer` for one class: which characters are part of a word, and which
/// words are emphasized as a whole. One more position for the end of the input.
struct Words {
    char: Vec<bool>,
    whole: Vec<bool>,
}

/// Compiled indicator data for one emphasis class (e.g. "italic").
#[derive(Debug, Clone)]
struct ClassIndicator {
    class_name: String,
    emphletter: Option<ResolvedTranslation>,
    begemph: Option<ResolvedTranslation>,
    endemph: Option<ResolvedTranslation>,
    begemphword: Option<ResolvedTranslation>,
    endemphword: Option<ResolvedTranslation>,
    begemphphrase: Option<ResolvedTranslation>,
    endemphphrase_before: Option<ResolvedTranslation>,
    endemphphrase_after: Option<ResolvedTranslation>,
    /// `lenemphphrase`, 0 when passages are not marked
    len_phrase: Option<usize>,
    /// Characters that don't cancel word emphasis (emphmodechars); by default only a
    /// space does
    emphmodechars: HashSet<char>,
    /// Characters emphasis is not indicated on (noemphchars); by default spaces
    noemphchars: HashSet<char>,
    character_classes: CharacterClasses,
}

impl ClassIndicator {
    fn new(class_name: String) -> Self {
        ClassIndicator {
            class_name,
            emphletter: None,
            begemph: None,
            endemph: None,
            begemphword: None,
            endemphword: None,
            begemphphrase: None,
            endemphphrase_before: None,
            endemphphrase_after: None,
            len_phrase: None,
            emphmodechars: HashSet::new(),
            noemphchars: HashSet::new(),
            character_classes: CharacterClasses::default(),
        }
    }

    fn is(&self, class: CharacterClass, c: char) -> bool {
        self.character_classes.contains(&class, c)
    }

    /// liblouis's `isEmphasizable`
    fn is_emphasizable(&self, c: char) -> bool {
        if self.noemphchars.is_empty() {
            !self.is(CharacterClass::Space, c)
        } else {
            !self.noemphchars.contains(&c)
        }
    }

    /// liblouis's `resetsEmphMode`: whether `c` cancels word emphasis
    fn resets(&self, c: char) -> bool {
        if self.is(CharacterClass::Letter, c) {
            false
        } else if self.emphmodechars.is_empty() {
            self.is(CharacterClass::Space, c)
        } else {
            !self.emphmodechars.contains(&c)
        }
    }

    /// liblouis's `isEmphSpace`: whether `c` separates words
    fn is_emph_space(&self, c: char) -> bool {
        !self.is_emphasizable(c) && (self.begemphword.is_none() || self.resets(c))
    }

    fn end_defined(&self) -> bool {
        self.endemph.is_some() || self.endemphphrase_after.is_some()
    }

    /// liblouis's `markEmphases` for this class, given which characters the caller
    /// emphasizes with it
    fn mark(&self, chars: &[char], emphasized: &[bool]) -> Marks {
        let n = chars.len();
        let mut marks = Marks::new(n);
        let mut words = Words {
            char: chars
                .iter()
                .map(|&c| !self.is_emph_space(c))
                .chain([false])
                .collect(),
            whole: vec![false; n + 1],
        };
        self.resolve_begin_end(chars, emphasized, &mut marks, &words);
        if self.begemphword.is_some() {
            self.resolve_words(chars, &mut marks, &mut words);
            if let Some(len) = self.len_phrase.filter(|&len| len > 0) {
                self.resolve_passages(n, len, &mut marks, &mut words);
            }
            self.resolve_resets(chars, &mut marks, &mut words);
            if self.endemphword.is_none() {
                self.resolve_all_symbols(n, &mut marks, &words);
            }
        } else if self.emphletter.is_some() {
            if self.begemph.is_some() {
                resolve_single_symbols(n, &mut marks);
            } else {
                self.resolve_all_symbols(n, &mut marks, &words);
            }
        }
        marks
    }

    /// liblouis's `resolveEmphasisBeginEnd`: mark the sections that start with an
    /// emphasized character and go on until an unemphasized one, not ending with a word
    /// that has no emphasized character. With phrase indicators, a section doesn't end
    /// inside a word.
    fn resolve_begin_end(
        &self,
        chars: &[char],
        emphasized: &[bool],
        marks: &mut Marks,
        words: &Words,
    ) {
        let n = chars.len();
        let mut last_space: Option<usize> = None;
        let mut emph_start: Option<usize> = None;
        let mut last_word: Option<usize> = None;
        let mut emph = false;
        let phrase_enabled = self.begemphphrase.is_some();
        let mut i = 0;
        while i < n {
            let is_space = !words.char[i];
            if is_space {
                last_space = Some(i);
                if emph {
                    last_word = Some(i);
                    emph = false;
                }
            }
            if !is_space && self.is_emphasizable(chars[i]) && emphasized[i] {
                emph_start.get_or_insert(i);
                emph = true;
            } else if !is_space
                && self.is_emphasizable(chars[i])
                && let Some(start) = emph_start
            {
                marks.begin[start] = true;
                if emph {
                    if phrase_enabled && let Some(space) = last_space.filter(|&space| start < space)
                    {
                        if let Some(word) = last_word {
                            marks.end[word] = true;
                        }
                        emph_start = None;
                        last_word = None;
                        emph = false;
                        i = space + 1;
                        continue;
                    }
                    marks.end[i] = true;
                } else if let Some(word) = last_word {
                    marks.end[word] = true;
                }
                emph_start = None;
                last_word = None;
                emph = false;
            }
            i += 1;
        }
        if let Some(start) = emph_start {
            marks.begin[start] = true;
            if emph {
                marks.end[n] = true;
            } else if let Some(word) = last_word {
                marks.end[word] = true;
            }
        }
    }

    /// liblouis's `resolveEmphasisWords`: turn the sections into words, and into single
    /// letters where `emphletter` is defined
    fn resolve_words(&self, chars: &[char], marks: &mut Marks, words: &mut Words) {
        let n = chars.len();
        let mut in_word = false;
        let mut in_emp = false;
        let mut word_start: Option<usize> = None;
        let mut char_cnt = 0;
        let mut last_char: Option<usize> = None;
        let letter_defined = self.emphletter.is_some();
        let endphraseafter_defined = self.begemphphrase.is_some()
            && (self.endemphphrase_after.is_some() || self.endemph.is_some());

        // remove the word marks after the last emphasizable character of a word, and a
        // word end mark there
        let trim_word_end =
            |marks: &mut Marks, words: &mut Words, last: Option<usize>, to: usize| {
                let from = last.map_or(0, |l| l + 1);
                for j in from..to {
                    words.char[j] = false;
                }
                if let Some(last) = last
                    && !marks.symbol[last]
                {
                    if marks.word[last] && !marks.end[last] {
                        marks.symbol[last] = true;
                    }
                    for j in last..to.saturating_sub(1) {
                        if marks.end[j + 1] {
                            marks.end[j + 1] = false;
                            marks.word[j + 1] = false;
                            break;
                        }
                    }
                }
            };

        for i in 0..n {
            if !in_emp && marks.begin[i] {
                in_emp = true;
                marks.begin[i] = false;
                if in_word {
                    word_start = Some(i);
                }
                if !words.char[i] {
                    word_start = None;
                }
            }

            if in_emp && marks.end[i] {
                in_emp = false;
                marks.end[i] = false;
                if in_word && let Some(start) = word_start {
                    if letter_defined && char_cnt == 1 {
                        marks.symbol[start] = true;
                    } else {
                        marks.word[start] = true;
                        if words.char[i] {
                            marks.end[i] = true;
                            marks.word[i] = true;
                        }
                    }
                }
            }

            if !in_word && words.char[i] && self.is_emphasizable(chars[i]) {
                in_word = true;
                if in_emp {
                    word_start = Some(i);
                }
                trim_word_end(marks, words, last_char, i);
            }

            if in_word && !words.char[i] {
                if in_emp && let Some(start) = word_start {
                    if letter_defined && char_cnt == 1 {
                        marks.symbol[start] = true;
                    } else {
                        marks.word[start] = true;
                    }
                }
                in_word = false;
                word_start = None;
            }

            if word_start == Some(i) {
                last_char = Some(i);
                char_cnt = 1;
            } else if in_word && (endphraseafter_defined || self.is_emphasizable(chars[i])) {
                last_char = Some(i);
                if in_emp {
                    char_cnt += 1;
                }
            }
        }

        if in_emp {
            marks.end[n] = false;
            if in_word && let Some(start) = word_start {
                if letter_defined && char_cnt == 1 {
                    marks.symbol[start] = true;
                } else {
                    marks.word[start] = true;
                }
            }
        }

        trim_word_end(marks, words, last_char, n);

        // mark whole words
        let mut word_start: Option<usize> = None;
        for i in 0..n {
            if marks.symbol[i] {
                if (i == 0 || !words.char[i - 1]) && (i + 1 == n || !words.char[i + 1]) {
                    words.whole[i] = true;
                }
            } else if marks.word[i] {
                if marks.end[i] {
                    if let Some(start) = word_start
                        && words.char[i]
                    {
                        words.whole[start] = false;
                    }
                    word_start = None;
                } else {
                    if i == 0 || !words.char[i - 1] {
                        words.whole[i] = true;
                    }
                    word_start = Some(i);
                }
            }
        }
    }

    /// liblouis's `convertToPassage`
    fn convert_to_passage(
        &self,
        pass_start: usize,
        pass_end: usize,
        word_start: usize,
        marks: &mut Marks,
        words: &mut Words,
    ) {
        for i in pass_start..=pass_end {
            marks.symbol[i] = false;
            marks.word[i] = false;
            words.whole[i] = false;
        }
        marks.begin[pass_start] = true;
        if self.end_defined() {
            marks.end[pass_end] = true;
        } else if let Some(end) = &self.endemphphrase_before {
            // an end indicator that is the word indicator is marked as one
            if self
                .begemphword
                .as_ref()
                .is_some_and(|word| word.output() == end.output())
            {
                marks.word[word_start] = true;
                words.whole[word_start] = true;
            } else {
                marks.end[word_start] = true;
            }
        }
    }

    /// liblouis's `resolveEmphasisPassages`: turn `len` or more consecutive emphasized
    /// words into a passage
    fn resolve_passages(&self, n: usize, len: usize, marks: &mut Marks, words: &mut Words) {
        let mut in_word = false;
        let mut last_word_end: Option<usize> = None;
        let mut in_emph_word = false;
        let mut last_emph_symbol: Option<usize> = None;
        let mut in_pass = false;
        let mut last_pass_word_start = 0;
        let mut last_pass_word_end: Option<usize> = None;
        let mut pass_start = 0;
        let mut pass_word_cnt = 0;
        let endphraseafter_defined = self.end_defined();

        for i in 0..n {
            if !in_word && words.char[i] {
                in_word = true;
            } else if in_word && !words.char[i] {
                in_word = false;
                last_word_end = Some(i);
            }

            let mut end_passage = false;
            if !in_emph_word && (marks.symbol[i] || (marks.word[i] && !marks.end[i])) {
                if marks.symbol[i] {
                    last_emph_symbol = Some(i);
                } else {
                    in_emph_word = true;
                }
                if in_pass {
                    last_pass_word_start = i;
                    pass_word_cnt += 1;
                }
            } else if (in_emph_word && marks.word[i] && marks.end[i]) || last_word_end == Some(i) {
                in_emph_word = false;
                if in_pass {
                    last_pass_word_end = Some(i);
                }
            }

            if !in_pass && (in_emph_word || last_emph_symbol == Some(i)) {
                in_pass = true;
                pass_start = i;
                last_pass_word_start = i;
                last_pass_word_end = None;
                pass_word_cnt = 1;
            } else if in_pass {
                if in_word && !(in_emph_word || last_emph_symbol == Some(i)) {
                    end_passage = true;
                } else if i == n - 1 && pass_word_cnt >= len {
                    let end = match last_pass_word_end {
                        Some(end) if end >= last_pass_word_start => end,
                        _ => n,
                    };
                    self.convert_to_passage(pass_start, end, last_pass_word_start, marks, words);
                }
            }
            if end_passage {
                in_pass = false;
                let end = match last_pass_word_end {
                    Some(end) if end >= last_pass_word_start => end,
                    _ => i,
                };
                // a passage closed before its last word needs one more word if that word is
                // only partly emphasized
                if !endphraseafter_defined && Some(end) != last_word_end {
                    pass_word_cnt -= 1;
                }
                if pass_word_cnt >= len {
                    self.convert_to_passage(pass_start, end, last_pass_word_start, marks, words);
                }
            }
        }
    }

    /// liblouis's `resolveEmphasisResets`: where a character cancels word emphasis
    /// inside a word, emphasize the rest of the word again
    fn resolve_resets(&self, chars: &[char], marks: &mut Marks, words: &mut Words) {
        let n = chars.len();
        let mut in_word = false;
        let mut in_pass = false;
        let mut word_start: Option<usize> = None;
        let mut word_reset = false;
        let mut letter_cnt = 0;
        let mut pass_end: Option<usize> = None;
        let letter_defined = self.emphletter.is_some();

        let mut i = 0;
        while i < n {
            if in_pass {
                if marks.end[i] {
                    in_pass = false;
                } else if marks.word[i] {
                    in_pass = false;
                    pass_end = Some(i);
                }
            }
            if !in_pass {
                if marks.begin[i] {
                    in_pass = true;
                } else {
                    if !in_word {
                        if marks.word[i] {
                            if self.resets(chars[i]) && letter_defined && pass_end != Some(i) {
                                marks.symbol[i] = true;
                                if words.char[i + 1] {
                                    marks.word[i + 1] = true;
                                    if words.whole[i] {
                                        words.whole[i + 1] = true;
                                    }
                                    if pass_end == Some(i) {
                                        pass_end = Some(i + 1);
                                    }
                                }
                                marks.word[i] = false;
                                words.whole[i] = false;
                                i += 1;
                                continue;
                            }
                            in_word = true;
                            word_start = Some(i);
                            letter_cnt = 0;
                            word_reset = false;
                        } else if marks.symbol[i] && self.resets(chars[i]) {
                            marks.symbol[i] = false;
                        }
                    }

                    if in_word {
                        if !words.char[i] || (marks.word[i] && marks.end[i]) {
                            in_word = false;
                            let start = word_start.unwrap();
                            if letter_defined && letter_cnt == 1 && Some(start) != pass_end {
                                marks.symbol[start] = true;
                                marks.word[start] = false;
                                words.whole[start] = false;
                                marks.end[i] = false;
                                marks.word[i] = false;
                            }
                            if word_reset || self.resets(chars[i]) {
                                marks.end[i] = false;
                                marks.word[i] = false;
                            }
                            if i == start {
                                words.whole[start] = false;
                                marks.end[i] = false;
                                marks.word[i] = false;
                            }
                        } else {
                            if self.resets(chars[i]) {
                                let start = word_start.unwrap();
                                if letter_defined && letter_cnt == 1 && Some(start) != pass_end {
                                    marks.symbol[start] = true;
                                    marks.word[start] = false;
                                    words.whole[start] = false;
                                }
                                word_reset = true;
                                i += 1;
                                continue;
                            }
                            if word_reset {
                                if let Some(end) = pass_end
                                    && word_start == Some(end)
                                {
                                    marks.word[end] = false;
                                }
                                pass_end = None;
                                word_reset = false;
                                word_start = Some(i);
                                letter_cnt = 0;
                                marks.word[i] = true;
                            }
                            letter_cnt += 1;
                        }
                    }
                }
            }
            i += 1;
        }

        if in_word {
            let start = word_start.unwrap();
            if letter_defined && letter_cnt == 1 && Some(start) != pass_end {
                marks.symbol[start] = true;
                marks.word[start] = false;
                words.whole[start] = false;
                marks.end[n] = false;
                marks.word[n] = false;
            }
            if word_reset {
                marks.end[n] = false;
                marks.word[n] = false;
            }
        }
    }

    /// liblouis's `resolveEmphasisAllSymbols`: without a word indicator, or without a
    /// word end indicator where emphasis ends inside a word, mark each character
    fn resolve_all_symbols(&self, n: usize, marks: &mut Marks, words: &Words) {
        if self.begemphword.is_none() {
            let mut in_emph = false;
            for i in 0..n {
                if in_emph {
                    if marks.end[i] {
                        in_emph = false;
                        marks.end[i] = false;
                    }
                } else if marks.begin[i] {
                    in_emph = true;
                    marks.begin[i] = false;
                }
                if in_emph {
                    marks.symbol[i] = true;
                }
            }
        } else if self.endemphword.is_none() {
            let mut in_pass = false;
            let mut in_word = false;
            let mut word_start = 0;
            for i in 0..n {
                if in_pass && (marks.end[i] || marks.word[i]) {
                    in_pass = false;
                }
                if !in_pass {
                    if marks.begin[i] {
                        in_pass = true;
                    } else {
                        if !in_word && marks.word[i] {
                            in_word = true;
                            word_start = i;
                        }
                        if in_word {
                            if marks.word[i] && marks.end[i] {
                                in_word = false;
                                marks.end[i] = false;
                                marks.word[i] = false;
                                marks.word[word_start] = false;
                                for j in word_start..i {
                                    marks.symbol[j] = true;
                                }
                            } else if !words.char[i] {
                                in_word = false;
                            }
                        }
                    }
                }
            }
        }
    }

    /// liblouis's `insertEmphasisEnd`
    fn end_indicator(&self, marks: &Marks, at: usize) -> Option<&ResolvedTranslation> {
        if !marks.end[at] {
            None
        } else if marks.word[at] {
            self.endemphword.as_ref()
        } else {
            self.endemph
                .as_ref()
                .or(self.endemphphrase_after.as_ref())
                .or(self.endemphphrase_before.as_ref())
        }
    }

    /// liblouis's `insertEmphasisBegin`
    fn begin_indicators(&self, marks: &Marks, at: usize) -> Vec<&ResolvedTranslation> {
        let mut indicators = Vec::new();
        if marks.begin[at]
            && let Some(t) = self.begemphphrase.as_ref().or(self.begemph.as_ref())
        {
            indicators.push(t);
        }
        if marks.word[at]
            && !marks.end[at]
            && let Some(t) = &self.begemphword
        {
            indicators.push(t);
        }
        indicators
    }

    /// liblouis's `endCount`: how far back the emphasis ending at `at` began
    fn end_count(marks: &Marks, at: usize) -> usize {
        if !marks.end[at] {
            return 0;
        }
        1 + (0..at)
            .rev()
            .take_while(|&i| !(marks.begin[i] || marks.word[i]))
            .count()
    }

    /// liblouis's `beginCount`: how far the emphasis beginning at `at` reaches
    fn begin_count(&self, marks: &Marks, chars: &[char], at: usize) -> usize {
        let n = chars.len();
        if marks.begin[at] {
            1 + (at + 1..n).take_while(|&i| !marks.end[i]).count()
        } else if marks.word[at] {
            1 + (at + 1..n)
                .take_while(|&i| {
                    !marks.end[i]
                        && !self.is(CharacterClass::Seqdelimiter, chars[i])
                        && !self.is_emph_space(chars[i])
                })
                .count()
        } else {
            0
        }
    }
}

/// liblouis's `resolveEmphasisSingleSymbols`: a section of one character is a letter
fn resolve_single_symbols(n: usize, marks: &mut Marks) {
    for i in 0..n {
        if marks.begin[i] && marks.end[i + 1] {
            marks.begin[i] = false;
            marks.end[i + 1] = false;
            marks.symbol[i] = true;
        }
    }
}

// ── Builder ─────────────────────────────────────────────────────────────────

/// Builder for [`Indicator`].
#[derive(Debug)]
pub struct IndicatorBuilder {
    classes: HashMap<String, ClassIndicator>,
    /// Tracks emphclass declaration order so the built `Indicator` preserves it.
    class_order: Vec<String>,
}

impl IndicatorBuilder {
    pub fn new() -> Self {
        Self {
            classes: HashMap::new(),
            class_order: Vec::new(),
        }
    }

    fn class_mut(&mut self, name: &str) -> &mut ClassIndicator {
        if !self.classes.contains_key(name) {
            self.class_order.push(name.to_string());
            self.classes
                .insert(name.to_string(), ClassIndicator::new(name.to_string()));
        }
        self.classes.get_mut(name).unwrap()
    }

    pub fn emphclass(&mut self, name: &str) {
        self.class_mut(name);
    }

    pub fn emphletter(&mut self, name: &str, dots: &str, origin: &AnchoredRule) {
        self.class_mut(name).emphletter = Some(make_translation(dots, origin));
    }

    pub fn begemphword(&mut self, name: &str, dots: &str, origin: &AnchoredRule) {
        self.class_mut(name).begemphword = Some(make_translation(dots, origin));
    }

    pub fn endemphword(&mut self, name: &str, dots: &str, origin: &AnchoredRule) {
        self.class_mut(name).endemphword = Some(make_translation(dots, origin));
    }

    pub fn begemphphrase(&mut self, name: &str, dots: &str, origin: &AnchoredRule) {
        self.class_mut(name).begemphphrase = Some(make_translation(dots, origin));
    }

    pub fn endemphphrase(
        &mut self,
        name: &str,
        dots: &str,
        position: &Position,
        origin: &AnchoredRule,
    ) {
        let class = self.class_mut(name);
        let t = Some(make_translation(dots, origin));
        match position {
            Position::Before => class.endemphphrase_before = t,
            Position::After => class.endemphphrase_after = t,
        }
    }

    pub fn lenemphphrase(&mut self, name: &str, len: usize) {
        self.class_mut(name).len_phrase = Some(len);
    }

    pub fn begemph(&mut self, name: &str, dots: &str, origin: &AnchoredRule) {
        self.class_mut(name).begemph = Some(make_translation(dots, origin));
    }

    pub fn endemph(&mut self, name: &str, dots: &str, origin: &AnchoredRule) {
        self.class_mut(name).endemph = Some(make_translation(dots, origin));
    }

    pub fn emphmodechars(&mut self, name: &str, chars: &str) {
        self.class_mut(name).emphmodechars = chars.chars().collect();
    }

    pub fn noemphchars(&mut self, name: &str, chars: &str) {
        self.class_mut(name).noemphchars = chars.chars().collect();
    }

    pub fn build(mut self, ctx: &TableContext) -> Option<Indicator> {
        let classes: Vec<ClassIndicator> = self
            .class_order
            .into_iter()
            .enumerate()
            .filter_map(|(i, name)| {
                let mut c = self.classes.remove(&name)?;
                c.character_classes = ctx.character_classes().clone();
                // liblouis's `setDefaults`, which only covers the first three classes
                if i < 3 && c.len_phrase.is_none_or(|len| len == 0) {
                    c.len_phrase = Some(4);
                }
                Some(c)
            })
            .collect();
        if classes.is_empty() {
            None
        } else {
            Some(Indicator { classes })
        }
    }
}

// ── Indicator ────────────────────────────────────────────────────────────────

/// Compiled emphasis indicator for the emphasis classes of a table.
#[derive(Debug, Clone)]
pub struct Indicator {
    classes: Vec<ClassIndicator>,
}

impl Indicator {
    fn marks(&self, chars: &[char], spans: &[EmphasisSpan]) -> Vec<Marks> {
        self.classes
            .iter()
            .map(|class| {
                let mut emphasized = vec![false; chars.len()];
                for span in spans.iter().filter(|s| s.class == class.class_name) {
                    for e in &mut emphasized
                        [span.range.start.min(chars.len())..span.range.end.min(chars.len())]
                    {
                        *e = true;
                    }
                }
                class.mark(chars, &emphasized)
            })
            .collect()
    }

    /// Where liblouis's emphasis buffer marks anything for an emphasis class, which keeps
    /// some contractions from spanning the position (`checkEmphasisChange`)
    pub fn marked(&self, input: &str, spans: &[EmphasisSpan]) -> Vec<bool> {
        let chars: Vec<char> = input.chars().collect();
        let marks = self.marks(&chars, spans);
        (0..=chars.len())
            .map(|at| marks.iter().any(|m| m.any(at)))
            .collect()
    }

    /// Returns sparse `(position, translation)` pairs for the given input and emphasis
    /// spans, in the order liblouis's `insertEmphasesAt` inserts them: at each position
    /// first the end indicators, the one whose emphasis began last first, then the begin
    /// indicators, the one whose emphasis reaches furthest first, then the letter
    /// indicators.
    ///
    /// Position `n` (where `n = input.chars().count()`) is valid and used for
    /// translations that must appear after all input characters.
    pub fn precompute(
        &self,
        input: &str,
        spans: &[EmphasisSpan],
    ) -> Vec<(usize, ResolvedTranslation)> {
        let chars: Vec<char> = input.chars().collect();
        let marks = self.marks(&chars, spans);
        let classes: Vec<(&ClassIndicator, &Marks)> = self.classes.iter().zip(&marks).collect();
        let mut result = Vec::new();
        for at in 0..=chars.len() {
            let mut ends: Vec<(usize, &ClassIndicator, &Marks)> = classes
                .iter()
                .map(|&(class, marks)| (ClassIndicator::end_count(marks, at), class, marks))
                .filter(|(count, ..)| *count > 0)
                .collect();
            // the first defined of those that began equally long ago ends first
            ends.sort_by_key(|(count, ..)| *count);
            for (_, class, marks) in ends {
                if let Some(t) = class.end_indicator(marks, at) {
                    result.push((at, t.clone()));
                }
            }
            let mut begins: Vec<(usize, usize, &ClassIndicator, &Marks)> = classes
                .iter()
                .enumerate()
                .map(|(i, &(class, marks))| (class.begin_count(marks, &chars, at), i, class, marks))
                .filter(|(count, ..)| *count > 0)
                .collect();
            // the last defined of those that reach equally far begins first
            begins.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
            for (_, _, class, marks) in begins {
                for t in class.begin_indicators(marks, at) {
                    result.push((at, t.clone()));
                }
            }
            for &(class, marks) in classes.iter().rev() {
                if marks.symbol.get(at).copied().unwrap_or(false)
                    && let Some(t) = &class.emphletter
                {
                    result.push((at, t.clone()));
                }
            }
        }
        result
    }
}

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
    use crate::parser::RuleParser;

    fn rule(s: &str) -> AnchoredRule {
        AnchoredRule::new(RuleParser::new(s).rule().unwrap(), None, 0)
    }

    fn space_ctx() -> TableContext {
        TableContext::new(
            CharacterClasses::new(&[
                (CharacterClass::Space, &[' ']),
                (CharacterClass::Letter, &['a', 'b', 'c', 'd', 'e', 'f']),
            ]),
            CharacterClasses::default(),
            Default::default(),
        )
    }

    fn pairs(t: &[(usize, ResolvedTranslation)]) -> Vec<(usize, String)> {
        t.iter()
            .map(|(pos, r)| (*pos, r.output().to_string()))
            .collect()
    }

    fn italic(range: std::ops::Range<usize>) -> Vec<EmphasisSpan> {
        vec![EmphasisSpan::new("italic", range)]
    }

    fn word_class() -> Indicator {
        let mut b = IndicatorBuilder::new();
        b.emphclass("italic");
        b.emphletter("italic", "⠨", &rule("emphletter italic 46"));
        b.begemphword("italic", "⠸", &rule("begemphword italic 456"));
        b.endemphword("italic", "⠠", &rule("endemphword italic 6"));
        b.begemphphrase("italic", "⠸⠸", &rule("begemphphrase italic 456-456"));
        b.endemphphrase(
            "italic",
            "⠸",
            &Position::Before,
            &rule("endemphphrase italic before 456"),
        );
        b.build(&space_ctx()).unwrap()
    }

    #[test]
    fn a_single_letter_gets_the_letter_indicator() {
        let result = word_class().precompute("abc", &italic(1..2));
        assert_eq!(pairs(&result), vec![(1, "⠨".to_string())]);
    }

    #[test]
    fn a_word_ending_at_a_space_needs_no_end_indicator() {
        let result = word_class().precompute("ab cd", &italic(0..3));
        assert_eq!(pairs(&result), vec![(0, "⠸".to_string())]);
    }

    #[test]
    fn emphasis_ending_inside_a_word_is_closed() {
        let result = word_class().precompute("abcd", &italic(0..2));
        assert_eq!(
            pairs(&result),
            vec![(0, "⠸".to_string()), (2, "⠠".to_string())]
        );
    }

    #[test]
    fn a_passage_ends_with_the_word_indicator_before_its_last_word() {
        let result = word_class().precompute("ab cd ef ab", &italic(0..11));
        assert_eq!(
            pairs(&result),
            vec![(0, "⠸⠸".to_string()), (9, "⠸".to_string())]
        );
        // three words are not enough
        let result = word_class().precompute("ab cd ef", &italic(0..8));
        assert_eq!(
            pairs(&result),
            vec![
                (0, "⠸".to_string()),
                (3, "⠸".to_string()),
                (6, "⠸".to_string())
            ]
        );
    }

    #[test]
    fn begemph_skips_spaces_unless_noemphchars_says_otherwise() {
        let mut b = IndicatorBuilder::new();
        b.begemph("italic", "⠁", &rule("begemph italic 1"));
        b.endemph("italic", "⠃", &rule("endemph italic 12"));
        let indicator = b.build(&space_ctx()).unwrap();
        let result = indicator.precompute(" ab cd", &italic(0..4));
        assert_eq!(
            pairs(&result),
            vec![(1, "⠁".to_string()), (3, "⠃".to_string())]
        );

        let mut b = IndicatorBuilder::new();
        b.begemph("italic", "⠁", &rule("begemph italic 1"));
        b.endemph("italic", "⠃", &rule("endemph italic 12"));
        b.noemphchars("italic", "%");
        let indicator = b.build(&space_ctx()).unwrap();
        let result = indicator.precompute(" ab cd", &italic(0..4));
        assert_eq!(
            pairs(&result),
            vec![(0, "⠁".to_string()), (4, "⠃".to_string())]
        );
    }

    #[test]
    fn indicators_nest() {
        let mut b = IndicatorBuilder::new();
        b.begemph("italic", "⠁", &rule("begemph italic 1"));
        b.endemph("italic", "⠁", &rule("endemph italic 1"));
        b.begemph("underline", "⠃", &rule("begemph underline 12"));
        b.endemph("underline", "⠃", &rule("endemph underline 12"));
        let indicator = b.build(&space_ctx()).unwrap();
        let outputs = |spans: &[EmphasisSpan]| {
            let result = indicator.precompute("abcd", spans);
            pairs(&result)
                .into_iter()
                .map(|(pos, s)| format!("{pos}{s}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let underline = |range| EmphasisSpan::new("underline", range);
        assert_eq!(
            outputs(&[EmphasisSpan::new("italic", 0..2), underline(0..4)]),
            "0⠃ 0⠁ 2⠁ 4⠃"
        );
        assert_eq!(
            outputs(&[EmphasisSpan::new("italic", 2..4), underline(0..4)]),
            "0⠃ 2⠁ 4⠁ 4⠃"
        );
        // otherwise the first defined class is outermost
        assert_eq!(
            outputs(&[EmphasisSpan::new("italic", 0..4), underline(0..4)]),
            "0⠃ 0⠁ 4⠁ 4⠃"
        );
    }

    #[test]
    fn marks_include_emphasis_without_indicators() {
        let mut b = IndicatorBuilder::new();
        b.emphclass("italic");
        let indicator = b.build(&space_ctx()).unwrap();
        assert!(indicator.precompute("abcd", &italic(1..3)).is_empty());
        assert_eq!(
            indicator.marked("abcd", &italic(1..3)),
            vec![false, true, false, true, false]
        );
    }

    #[test]
    fn back_translated_emphasis() {
        let letter = make_translation("", &rule("emphletter italic 46"));
        let text = |s: &str| ResolvedTranslation::new(s, s, 1, TranslationStage::Main, None);
        let spans = back_translated(&[letter, text("a"), text("b")]);
        assert_eq!(spans, italic(0..1));
    }
}
