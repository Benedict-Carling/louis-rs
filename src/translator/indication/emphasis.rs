//! Emphasis and capitalization indication, as liblouis models both.
//!
//! liblouis treats capitals as one more emphasis class, a "mode" driven by the
//! `uppercase` attribute instead of the caller's typeform. Each class marks, per input
//! position, where an indicator begins or ends a passage, begins or ends a word, or marks
//! a single letter (`markEmphases` and its `resolveEmphasis*` steps in
//! `lou_translateString.c`). The translation loop then inserts the indicators of the
//! positions it reaches (`insertEmphasesAt`), so an indicator of a position inside a
//! multi-character rule comes out after that rule, and the capital start indicator of a
//! position is inserted each time the loop visits it.
//!
//! [`Emphases`] holds the classes of a table, [`Marks`] the marks of one input.

use std::collections::{HashMap, HashSet};

use crate::{
    emphasis::EmphasisSpan,
    parser::{AnchoredRule, CharacterClass, Position},
    translator::{ResolvedTranslation, TranslationStage, table::TableContext},
};

const WORD_CHAR: u8 = 1;
const WORD_WHOLE: u8 = 4;
const CAPS_BIT: u64 = 1 << 63;

fn make_translation(dots: &str, origin: &AnchoredRule) -> ResolvedTranslation {
    ResolvedTranslation::new("", dots, 1, TranslationStage::Main, origin.clone())
}

/// liblouis's `EmphasisInfo`: one bit per class in each field
#[derive(Debug, Clone, Copy, Default)]
struct Info {
    begin: u64,
    end: u64,
    word: u64,
    symbol: u64,
}

impl Info {
    fn any(&self) -> u64 {
        self.begin | self.end | self.word | self.symbol
    }
}

/// The indicators of one class (liblouis's `emphRules` row)
#[derive(Debug, Clone, Default)]
struct Rules {
    letter: Option<ResolvedTranslation>,
    begword: Option<ResolvedTranslation>,
    endword: Option<ResolvedTranslation>,
    beg: Option<ResolvedTranslation>,
    end: Option<ResolvedTranslation>,
    begphrase: Option<ResolvedTranslation>,
    endphrase_before: Option<ResolvedTranslation>,
    endphrase_after: Option<ResolvedTranslation>,
    len_phrase: usize,
}

impl Rules {
    fn set_endphrase(&mut self, dots: &str, position: &Position, origin: &AnchoredRule) {
        let t = Some(make_translation(dots, origin));
        match position {
            Position::Before => self.endphrase_before = t,
            Position::After => self.endphrase_after = t,
        }
    }
}

/// An emphasis class, or the capitals mode
#[derive(Debug, Clone)]
struct Class {
    /// The typeform class name, or `None` for capitals
    name: Option<String>,
    bit: u64,
    rules: Rules,
    emphmodechars: Vec<char>,
    noemphchars: Vec<char>,
}

/// The character attributes the classes consult
#[derive(Debug, Clone, Default)]
struct Attributes {
    space: HashSet<char>,
    letter: HashSet<char>,
    uppercase: HashSet<char>,
    seqdelimiter: HashSet<char>,
    capsmodechars: HashSet<char>,
    /// Characters that share a base with an uppercase letter (the `linked` list of
    /// liblouis's `isEmphasizable`)
    caps_emphasizable: HashSet<char>,
    /// Characters that don't cancel capitals mode although not letters (the `linked`
    /// check of `resetsEmphMode`)
    caps_linked: HashSet<char>,
}

/// Builder for [`Emphases`]
#[derive(Debug, Default)]
pub struct IndicatorBuilder {
    classes: Vec<Class>,
    caps: Option<Class>,
    capsmodechars: HashSet<char>,
    /// `base` rules as (attribute, derived, base)
    bases: Vec<(String, char, char)>,
}

impl IndicatorBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    fn class_mut(&mut self, name: &str) -> &mut Class {
        if let Some(i) = self
            .classes
            .iter()
            .position(|c| c.name.as_deref() == Some(name))
        {
            return &mut self.classes[i];
        }
        let bit = 1 << self.classes.len().min(62);
        // liblouis gives the first three classes a default phrase length
        let len_phrase = if self.classes.len() < 3 { 4 } else { 0 };
        self.classes.push(Class {
            name: Some(name.to_string()),
            bit,
            rules: Rules {
                len_phrase,
                ..Rules::default()
            },
            emphmodechars: Vec::new(),
            noemphchars: Vec::new(),
        });
        self.classes.last_mut().unwrap()
    }

    fn caps_mut(&mut self) -> &mut Rules {
        &mut self
            .caps
            .get_or_insert_with(|| Class {
                name: None,
                bit: CAPS_BIT,
                rules: Rules::default(),
                emphmodechars: Vec::new(),
                noemphchars: Vec::new(),
            })
            .rules
    }

    pub fn emphclass(&mut self, name: &str) {
        self.class_mut(name);
    }

    pub fn emphletter(&mut self, name: &str, dots: &str, origin: &AnchoredRule) {
        self.class_mut(name).rules.letter = Some(make_translation(dots, origin));
    }

    pub fn begemphword(&mut self, name: &str, dots: &str, origin: &AnchoredRule) {
        self.class_mut(name).rules.begword = Some(make_translation(dots, origin));
    }

    pub fn endemphword(&mut self, name: &str, dots: &str, origin: &AnchoredRule) {
        self.class_mut(name).rules.endword = Some(make_translation(dots, origin));
    }

    pub fn begemphphrase(&mut self, name: &str, dots: &str, origin: &AnchoredRule) {
        self.class_mut(name).rules.begphrase = Some(make_translation(dots, origin));
    }

    pub fn endemphphrase(
        &mut self,
        name: &str,
        dots: &str,
        position: &Position,
        origin: &AnchoredRule,
    ) {
        self.class_mut(name)
            .rules
            .set_endphrase(dots, position, origin);
    }

    pub fn lenemphphrase(&mut self, name: &str, len: usize) {
        self.class_mut(name).rules.len_phrase = len;
    }

    pub fn begemph(&mut self, name: &str, dots: &str, origin: &AnchoredRule) {
        self.class_mut(name).rules.beg = Some(make_translation(dots, origin));
    }

    pub fn endemph(&mut self, name: &str, dots: &str, origin: &AnchoredRule) {
        self.class_mut(name).rules.end = Some(make_translation(dots, origin));
    }

    pub fn emphmodechars(&mut self, name: &str, chars: &str) {
        self.class_mut(name).emphmodechars.extend(chars.chars());
    }

    pub fn noemphchars(&mut self, name: &str, chars: &str) {
        self.class_mut(name).noemphchars.extend(chars.chars());
    }

    pub fn capsletter(&mut self, dots: &str, origin: &AnchoredRule) {
        self.caps_mut().letter = Some(make_translation(dots, origin));
    }

    pub fn begcapsword(&mut self, dots: &str, origin: &AnchoredRule) {
        self.caps_mut().begword = Some(make_translation(dots, origin));
    }

    pub fn endcapsword(&mut self, dots: &str, origin: &AnchoredRule) {
        self.caps_mut().endword = Some(make_translation(dots, origin));
    }

    pub fn begcaps(&mut self, dots: &str, origin: &AnchoredRule) {
        self.caps_mut().beg = Some(make_translation(dots, origin));
    }

    pub fn endcaps(&mut self, dots: &str, origin: &AnchoredRule) {
        self.caps_mut().end = Some(make_translation(dots, origin));
    }

    pub fn begcapsphrase(&mut self, dots: &str, origin: &AnchoredRule) {
        self.caps_mut().begphrase = Some(make_translation(dots, origin));
    }

    pub fn endcapsphrase(&mut self, dots: &str, position: &Position, origin: &AnchoredRule) {
        self.caps_mut().set_endphrase(dots, position, origin);
    }

    pub fn lencapsphrase(&mut self, len: usize) {
        self.caps_mut().len_phrase = len;
    }

    pub fn capsmodechars(&mut self, chars: &str) {
        self.capsmodechars.extend(chars.chars());
    }

    pub fn base(&mut self, attribute: &str, derived: char, base: char) {
        self.bases.push((attribute.to_string(), derived, base));
    }

    pub fn build(self, ctx: &TableContext) -> Option<Emphases> {
        if self.classes.is_empty() && self.caps.is_none() {
            return None;
        }
        let class = |c| ctx.character_classes().get(&c).unwrap_or_default();
        let uppercase = class(CharacterClass::Uppercase);
        let base_of: HashMap<char, (&str, char)> = self
            .bases
            .iter()
            .map(|(attribute, derived, base)| (*derived, (attribute.as_str(), *base)))
            .collect();
        let mut derived_of: HashMap<char, Vec<(&str, char)>> = HashMap::new();
        for (attribute, derived, base) in &self.bases {
            derived_of
                .entry(*base)
                .or_default()
                .push((attribute.as_str(), *derived));
        }
        let mut caps_emphasizable = HashSet::new();
        let mut caps_linked = HashSet::new();
        let candidates = uppercase
            .iter()
            .chain(base_of.keys())
            .chain(derived_of.keys())
            .copied()
            .collect::<HashSet<char>>();
        for c in candidates {
            let (mode, head) = match base_of.get(&c) {
                Some(&(attribute, base)) => (Some(attribute), base),
                None => (None, c),
            };
            let linked = derived_of.get(&head).map(Vec::as_slice).unwrap_or_default();
            if uppercase.contains(&head) || linked.iter().any(|(_, d)| uppercase.contains(d)) {
                caps_emphasizable.insert(c);
            }
            if linked
                .iter()
                .any(|&(m, d)| mode.is_none_or(|mode| mode == m) && uppercase.contains(&d))
            {
                caps_linked.insert(c);
            }
        }
        let attributes = Attributes {
            space: class(CharacterClass::Space),
            letter: class(CharacterClass::Letter),
            uppercase,
            seqdelimiter: class(CharacterClass::Seqdelimiter),
            capsmodechars: self.capsmodechars,
            caps_emphasizable,
            caps_linked,
        };
        Some(Emphases {
            classes: self.classes,
            caps: self.caps,
            attributes,
        })
    }
}

/// The emphasis classes and the capitals mode of a table
#[derive(Debug, Clone)]
pub struct Emphases {
    classes: Vec<Class>,
    caps: Option<Class>,
    attributes: Attributes,
}

impl Emphases {
    fn is_caps(class: &Class) -> bool {
        class.name.is_none()
    }

    fn is_emphasizable(&self, c: char, class: &Class) -> bool {
        if Self::is_caps(class) {
            self.attributes.caps_emphasizable.contains(&c)
        } else if class.noemphchars.is_empty() {
            !self.attributes.space.contains(&c)
        } else {
            !class.noemphchars.contains(&c)
        }
    }

    fn resets_emph_mode(&self, c: char, class: &Class) -> bool {
        let a = &self.attributes;
        if Self::is_caps(class) {
            if a.uppercase.contains(&c) || a.caps_linked.contains(&c) {
                return false;
            }
            !(a.letter.contains(&c) || a.capsmodechars.contains(&c))
        } else if a.letter.contains(&c) {
            false
        } else if class.emphmodechars.is_empty() {
            a.space.contains(&c)
        } else {
            !class.emphmodechars.contains(&c)
        }
    }

    fn is_emphasized(&self, c: char, class: &Class, typeform: bool) -> bool {
        if !self.is_emphasizable(c, class) {
            return false;
        }
        if Self::is_caps(class) {
            self.attributes.uppercase.contains(&c)
        } else {
            typeform
        }
    }

    fn is_emph_space(&self, c: char, class: &Class) -> bool {
        let word_enabled = class.rules.begword.is_some();
        if Self::is_caps(class) && !(word_enabled && !self.attributes.capsmodechars.is_empty()) {
            return self.attributes.space.contains(&c);
        }
        !self.is_emphasizable(c, class) && (!word_enabled || self.resets_emph_mode(c, class))
    }

    /// liblouis's `markEmphases` for `chars`, the caller's emphasis given by `spans`
    pub fn mark(&self, chars: &[char], spans: &[EmphasisSpan]) -> Marks<'_> {
        let n = chars.len();
        let mut marker = Marker {
            emphases: self,
            chars,
            buffer: vec![Info::default(); n + 1],
            words: vec![0; n + 2],
        };
        for class in self.classes.iter().chain(&self.caps) {
            let typeform: Vec<bool> = match &class.name {
                Some(name) => {
                    let mut typeform = vec![false; n];
                    for span in spans.iter().filter(|s| &s.class == name) {
                        for t in typeform
                            .iter_mut()
                            .take(span.range.end.min(n))
                            .skip(span.range.start)
                        {
                            *t = true;
                        }
                    }
                    typeform
                }
                None => vec![false; n],
            };
            marker.mark(class, &typeform);
        }
        Marks {
            emphases: self,
            buffer: marker.buffer,
            chars: chars.to_vec(),
        }
    }
}

struct Marker<'a> {
    emphases: &'a Emphases,
    chars: &'a [char],
    buffer: Vec<Info>,
    words: Vec<u8>,
}

fn at(i: isize) -> usize {
    usize::try_from(i).expect("liblouis never marks position -1")
}

impl Marker<'_> {
    fn len(&self) -> usize {
        self.chars.len()
    }

    fn word_char(&self, i: usize) -> bool {
        self.words[i] & WORD_CHAR != 0
    }

    fn mark(&mut self, class: &Class, typeform: &[bool]) {
        let e = self.emphases;
        for i in 0..self.len() {
            if e.is_emph_space(self.chars[i], class) {
                self.words[i] &= !WORD_CHAR;
            } else {
                self.words[i] |= WORD_CHAR;
            }
            self.words[i] &= !WORD_WHOLE;
        }
        self.begin_end(class, typeform);
        let rules = &class.rules;
        if rules.begword.is_some() {
            self.words(class);
            if rules.len_phrase > 0 {
                self.passages(class);
            }
            self.resets(class);
            if rules.endword.is_none() {
                self.all_symbols(class);
            }
        } else if rules.letter.is_some() {
            if rules.beg.is_some() {
                self.single_symbols(class);
            } else {
                self.all_symbols(class);
            }
        }
        if Emphases::is_caps(class) {
            let capsletter = rules.letter.is_some();
            for i in 0..self.len() {
                if !(capsletter && e.attributes.uppercase.contains(&self.chars[i])) {
                    self.buffer[i].symbol &= !class.bit;
                }
            }
        }
    }

    /// liblouis's `resolveEmphasisBeginEnd`
    fn begin_end(&mut self, class: &Class, typeform: &[bool]) {
        let v = class.bit;
        let e = self.emphases;
        let mut last_space: isize = -1;
        let mut emph_start: isize = -1;
        let mut last_word: isize = -1;
        let mut emph = false;
        let phrase_enabled = class.rules.begphrase.is_some();
        let n = self.len() as isize;
        let mut i: isize = 0;
        while i < n {
            let u = at(i);
            let is_space = !self.word_char(u);
            if is_space {
                last_space = i;
                if emph {
                    last_word = i;
                    emph = false;
                }
            }
            if !is_space && e.is_emphasized(self.chars[u], class, typeform[u]) {
                if emph_start < 0 {
                    emph_start = i;
                }
                emph = true;
            } else if !is_space && e.is_emphasizable(self.chars[u], class) && emph_start >= 0 {
                self.buffer[at(emph_start)].begin |= v;
                if emph {
                    if phrase_enabled && emph_start < last_space {
                        self.buffer[at(last_word)].end |= v;
                        emph_start = -1;
                        last_word = -1;
                        emph = false;
                        i = last_space + 1;
                        continue;
                    }
                    self.buffer[u].end |= v;
                } else {
                    self.buffer[at(last_word)].end |= v;
                }
                emph_start = -1;
                last_word = -1;
                emph = false;
            }
            i += 1;
        }
        if emph_start >= 0 {
            self.buffer[at(emph_start)].begin |= v;
            if emph {
                let n = self.len();
                self.buffer[n].end |= v;
            } else {
                self.buffer[at(last_word)].end |= v;
            }
        }
    }

    /// Takes the WORD_CHAR marks off the characters after the last emphasizable one of
    /// a word, and with them a word end mark (part of `resolveEmphasisWords`)
    fn trim_word(&mut self, v: u64, last_char: isize, until: usize) {
        for j in (last_char + 1).max(0) as usize..until {
            self.words[j] &= !WORD_CHAR;
        }
        if last_char >= 0 && self.buffer[at(last_char)].symbol & v == 0 {
            let lc = at(last_char);
            if self.buffer[lc].word & v != 0 && self.buffer[lc].end & v == 0 {
                self.buffer[lc].symbol |= v;
            }
            for j in lc..until.saturating_sub(1) {
                if self.buffer[j + 1].end & v != 0 {
                    self.buffer[j + 1].end &= !v;
                    self.buffer[j + 1].word &= !v;
                    break;
                }
            }
        }
    }

    /// liblouis's `resolveEmphasisWords`
    fn words(&mut self, class: &Class) {
        let v = class.bit;
        let e = self.emphases;
        let n = self.len();
        let mut in_word = false;
        let mut in_emp = false;
        let mut word_start: isize = -1;
        let mut char_cnt = 0;
        let mut last_char: isize = -1;
        let letter_defined = class.rules.letter.is_some();
        let endphraseafter_defined = class.rules.begphrase.is_some()
            && (class.rules.endphrase_after.is_some() || class.rules.end.is_some());

        for i in 0..n {
            if !in_emp && self.buffer[i].begin & v != 0 {
                in_emp = true;
                self.buffer[i].begin &= !v;
                if in_word {
                    word_start = i as isize;
                }
                if !self.word_char(i) {
                    word_start = -1;
                }
            }

            if in_emp && self.buffer[i].end & v != 0 {
                in_emp = false;
                self.buffer[i].end &= !v;
                if in_word && word_start >= 0 {
                    if letter_defined && char_cnt == 1 {
                        self.buffer[at(word_start)].symbol |= v;
                    } else {
                        self.buffer[at(word_start)].word |= v;
                        if self.word_char(i) {
                            self.buffer[i].end |= v;
                            self.buffer[i].word |= v;
                        }
                    }
                }
            }

            if !in_word && self.word_char(i) && e.is_emphasizable(self.chars[i], class) {
                in_word = true;
                if in_emp {
                    word_start = i as isize;
                }
                self.trim_word(v, last_char, i);
            }

            if in_word && !self.word_char(i) {
                if in_emp && word_start >= 0 {
                    if letter_defined && char_cnt == 1 {
                        self.buffer[at(word_start)].symbol |= v;
                    } else {
                        self.buffer[at(word_start)].word |= v;
                    }
                }
                in_word = false;
                word_start = -1;
            }

            if i as isize == word_start {
                last_char = i as isize;
                char_cnt = 1;
            } else if in_word && (endphraseafter_defined || e.is_emphasizable(self.chars[i], class))
            {
                last_char = i as isize;
                if in_emp {
                    char_cnt += 1;
                }
            }
        }

        if in_emp {
            self.buffer[n].end &= !v;
            if in_word && word_start >= 0 {
                if letter_defined && char_cnt == 1 {
                    self.buffer[at(word_start)].symbol |= v;
                } else {
                    self.buffer[at(word_start)].word |= v;
                }
            }
        }

        self.trim_word(v, last_char, n);

        let mut word_start: isize = -1;
        for i in 0..n {
            if self.buffer[i].symbol & v != 0 {
                if (i == 0 || !self.word_char(i - 1)) && (i + 1 == n || !self.word_char(i + 1)) {
                    self.words[i] |= WORD_WHOLE;
                }
            } else if self.buffer[i].word & v != 0 {
                if self.buffer[i].end & v != 0 {
                    if word_start >= 0 && self.word_char(i) {
                        self.words[at(word_start)] &= !WORD_WHOLE;
                    }
                    word_start = -1;
                } else {
                    if i == 0 || !self.word_char(i - 1) {
                        self.words[i] |= WORD_WHOLE;
                    }
                    word_start = i as isize;
                }
            }
        }
    }

    /// liblouis's `convertToPassage`
    fn convert_to_passage(
        &mut self,
        class: &Class,
        pass_start: usize,
        pass_end: usize,
        word_start: usize,
    ) {
        let v = class.bit;
        for i in pass_start..=pass_end {
            self.buffer[i].symbol &= !v;
            self.buffer[i].word &= !v;
            self.words[i] &= !WORD_WHOLE;
        }
        self.buffer[pass_start].begin |= v;
        let rules = &class.rules;
        if rules.end.is_some() || rules.endphrase_after.is_some() {
            self.buffer[pass_end].end |= v;
        } else if let Some(before) = &rules.endphrase_before {
            if rules
                .begword
                .as_ref()
                .is_some_and(|begword| begword.output() == before.output())
            {
                self.buffer[word_start].word |= v;
                self.words[word_start] |= WORD_WHOLE;
            } else {
                self.buffer[word_start].end |= v;
            }
        }
    }

    /// liblouis's `resolveEmphasisPassages`
    fn passages(&mut self, class: &Class) {
        let v = class.bit;
        let n = self.len();
        let is_mode = Emphases::is_caps(class);
        let mut in_word = false;
        let mut last_word_end: isize = -1;
        let mut in_emph_word = false;
        let mut last_emph_symbol: isize = -1;
        let mut in_pass = false;
        let mut last_pass_word_start: isize = -1;
        let mut last_pass_word_end: isize = -1;
        let mut pass_start: isize = -1;
        let mut pass_word_cnt: usize = 0;
        let endphraseafter_defined =
            class.rules.endphrase_after.is_some() || class.rules.end.is_some();
        let len_phrase = class.rules.len_phrase;

        for i in 0..n {
            let ii = i as isize;
            if !in_word && self.word_char(i) {
                in_word = true;
            } else if in_word && !self.word_char(i) {
                in_word = false;
                last_word_end = ii;
            }

            let b = self.buffer[i];
            let mut end_passage = false;
            if !in_emph_word && (b.symbol & v != 0 || (b.word & v != 0 && b.end & v == 0)) {
                if b.symbol & v != 0 {
                    last_emph_symbol = ii;
                } else {
                    in_emph_word = true;
                }
                if in_pass {
                    if !is_mode || self.words[i] & WORD_WHOLE != 0 {
                        last_pass_word_start = ii;
                        pass_word_cnt += 1;
                    } else {
                        end_passage = true;
                    }
                }
            } else if (in_emph_word && b.word & v != 0 && b.end & v != 0) || last_word_end == ii {
                in_emph_word = false;
                if in_pass {
                    last_pass_word_end = ii;
                }
            }

            if !end_passage && !in_pass && (in_emph_word || last_emph_symbol == ii) {
                if !is_mode || self.words[i] & WORD_WHOLE != 0 {
                    in_pass = true;
                    pass_start = ii;
                    last_pass_word_start = ii;
                    last_pass_word_end = -1;
                    pass_word_cnt = 1;
                }
            } else if in_pass {
                if end_passage || (in_word && !(in_emph_word || last_emph_symbol == ii)) {
                    in_pass = false;
                    if last_pass_word_end < last_pass_word_start {
                        last_pass_word_end = ii;
                    }
                    if !endphraseafter_defined && last_pass_word_end != last_word_end {
                        pass_word_cnt = pass_word_cnt.saturating_sub(1);
                    }
                    if pass_word_cnt >= len_phrase {
                        self.convert_to_passage(
                            class,
                            at(pass_start),
                            at(last_pass_word_end),
                            at(last_pass_word_start),
                        );
                    }
                } else if i == n - 1 && pass_word_cnt >= len_phrase {
                    if last_pass_word_end < last_pass_word_start {
                        last_pass_word_end = n as isize;
                    }
                    self.convert_to_passage(
                        class,
                        at(pass_start),
                        at(last_pass_word_end),
                        at(last_pass_word_start),
                    );
                }
            }
        }
    }

    /// liblouis's `resolveEmphasisSingleSymbols`
    fn single_symbols(&mut self, class: &Class) {
        let v = class.bit;
        for i in 0..self.len() {
            if self.buffer[i].begin & v != 0 && self.buffer[i + 1].end & v != 0 {
                self.buffer[i].begin &= !v;
                self.buffer[i + 1].end &= !v;
                self.buffer[i].symbol |= v;
            }
        }
    }

    /// liblouis's `resolveEmphasisAllSymbols`
    fn all_symbols(&mut self, class: &Class) {
        let v = class.bit;
        let n = self.len();
        if class.rules.begword.is_none() {
            let mut in_emph = false;
            for i in 0..n {
                if in_emph {
                    if self.buffer[i].end & v != 0 {
                        in_emph = false;
                        self.buffer[i].end &= !v;
                    }
                } else if self.buffer[i].begin & v != 0 {
                    in_emph = true;
                    self.buffer[i].begin &= !v;
                }
                if in_emph {
                    self.buffer[i].symbol |= v;
                }
            }
        } else if class.rules.endword.is_none() {
            let mut in_pass = false;
            let mut in_word = false;
            let mut word_start = 0;
            for i in 0..n {
                if in_pass && (self.buffer[i].end & v != 0 || self.buffer[i].word & v != 0) {
                    in_pass = false;
                }
                if in_pass {
                    continue;
                }
                if self.buffer[i].begin & v != 0 {
                    in_pass = true;
                    continue;
                }
                if !in_word && self.buffer[i].word & v != 0 {
                    in_word = true;
                    word_start = i;
                }
                if in_word {
                    if self.buffer[i].word & v != 0 && self.buffer[i].end & v != 0 {
                        in_word = false;
                        self.buffer[i].end &= !v;
                        self.buffer[i].word &= !v;
                        self.buffer[word_start].word &= !v;
                        for j in word_start..i {
                            self.buffer[j].symbol |= v;
                        }
                    } else if !self.word_char(i) {
                        in_word = false;
                    }
                }
            }
        }
    }

    /// liblouis's `resolveEmphasisResets`
    fn resets(&mut self, class: &Class) {
        let v = class.bit;
        let e = self.emphases;
        let n = self.len();
        let mut in_word = false;
        let mut in_pass = false;
        let mut word_start: isize = -1;
        let mut word_reset = false;
        let mut letter_cnt = 0;
        let mut pass_end: isize = -1;
        let letter_defined = class.rules.letter.is_some();
        let chars = self.chars;
        let resets = |i: usize| e.resets_emph_mode(chars[i], class);

        let mut i = 0;
        while i < n {
            let ii = i as isize;
            if in_pass {
                if self.buffer[i].end & v != 0 {
                    in_pass = false;
                } else if self.buffer[i].word & v != 0 {
                    in_pass = false;
                    pass_end = ii;
                }
            }
            if in_pass {
                i += 1;
                continue;
            }
            if self.buffer[i].begin & v != 0 {
                in_pass = true;
                i += 1;
                continue;
            }
            if !in_word {
                if self.buffer[i].word & v != 0 {
                    if resets(i) && letter_defined && pass_end != ii {
                        self.buffer[i].symbol |= v;
                        if self.words[i + 1] & WORD_CHAR != 0 {
                            self.buffer[i + 1].word |= v;
                            if self.words[i] & WORD_WHOLE != 0 {
                                self.words[i + 1] |= WORD_WHOLE;
                            }
                            if pass_end == ii {
                                pass_end += 1;
                            }
                        }
                        self.buffer[i].word &= !v;
                        self.words[i] &= !WORD_WHOLE;
                        i += 1;
                        continue;
                    }
                    in_word = true;
                    word_start = ii;
                    letter_cnt = 0;
                    word_reset = false;
                } else if self.buffer[i].symbol & v != 0 && resets(i) {
                    self.buffer[i].symbol &= !v;
                }
            }

            if in_word {
                if !self.word_char(i)
                    || (self.buffer[i].word & v != 0 && self.buffer[i].end & v != 0)
                {
                    in_word = false;
                    if letter_defined && letter_cnt == 1 && word_start != pass_end {
                        let ws = at(word_start);
                        self.buffer[ws].symbol |= v;
                        self.buffer[ws].word &= !v;
                        self.words[ws] &= !WORD_WHOLE;
                        self.buffer[i].end &= !v;
                        self.buffer[i].word &= !v;
                    }
                    if word_reset || resets(i) {
                        self.buffer[i].end &= !v;
                        self.buffer[i].word &= !v;
                    }
                    if ii == word_start {
                        self.words[at(word_start)] &= !WORD_WHOLE;
                        self.buffer[i].end &= !v;
                        self.buffer[i].word &= !v;
                    }
                } else {
                    if resets(i) {
                        if letter_defined && letter_cnt == 1 && word_start != pass_end {
                            let ws = at(word_start);
                            self.buffer[ws].symbol |= v;
                            self.buffer[ws].word &= !v;
                            self.words[ws] &= !WORD_WHOLE;
                        }
                        word_reset = true;
                        i += 1;
                        continue;
                    }
                    if word_reset {
                        if word_start == pass_end {
                            self.buffer[at(pass_end)].word &= !v;
                        }
                        pass_end = -1;
                        word_reset = false;
                        word_start = ii;
                        letter_cnt = 0;
                        self.buffer[i].word |= v;
                    }
                    letter_cnt += 1;
                }
            }
            i += 1;
        }

        if in_word {
            if letter_defined && letter_cnt == 1 && word_start != pass_end {
                let ws = at(word_start);
                self.buffer[ws].symbol |= v;
                self.buffer[ws].word &= !v;
                self.words[ws] &= !WORD_WHOLE;
                self.buffer[n].end &= !v;
                self.buffer[n].word &= !v;
            }
            if word_reset {
                self.buffer[n].end &= !v;
                self.buffer[n].word &= !v;
            }
        }
    }
}

/// The emphasis and capital marks of one input
#[derive(Debug, Clone)]
pub struct Marks<'a> {
    emphases: &'a Emphases,
    buffer: Vec<Info>,
    chars: Vec<char>,
}

impl Marks<'_> {
    /// Whether any class marks position `pos`
    pub fn marked_at(&self, pos: usize) -> bool {
        self.buffer.get(pos).is_some_and(|b| b.any() != 0)
    }

    /// liblouis's `checkEmphasisChange`: whether a mark falls inside `pos..pos + len`
    pub fn changes_within(&self, pos: usize, len: usize) -> bool {
        (pos + 1..pos + len).any(|i| self.marked_at(i))
    }

    /// The capital end indicators of `at`
    pub fn caps_end_at(&self, at: usize) -> Vec<ResolvedTranslation> {
        let mut out = Vec::new();
        self.caps_end(at, &mut out);
        out
    }

    /// The emphasis end and start indicators of `at`
    pub fn emphasis_at(&self, at: usize) -> Vec<ResolvedTranslation> {
        let mut out = Vec::new();
        self.emphasis_end(at, &mut out);
        self.emphasis_begin(at, &mut out);
        out
    }

    /// The capital start indicators of `pos`, inserted each time the loop visits it
    pub fn caps_begin_at(&self, pos: usize) -> Vec<ResolvedTranslation> {
        let mut out = Vec::new();
        self.caps_begin(pos, &mut out);
        out
    }

    fn caps_end(&self, at: usize, out: &mut Vec<ResolvedTranslation>) {
        if let Some(caps) = &self.emphases.caps
            && self.buffer[at].any() & caps.bit != 0
        {
            self.insert_end(at, caps, out);
        }
    }

    fn caps_begin(&self, at: usize, out: &mut Vec<ResolvedTranslation>) {
        if let Some(caps) = &self.emphases.caps
            && at < self.buffer.len()
            && self.buffer[at].any() & caps.bit != 0
        {
            self.insert_begin(at, caps, out);
            self.insert_symbol(at, caps, out);
        }
    }

    fn emphasis_end(&self, at: usize, out: &mut Vec<ResolvedTranslation>) {
        let mut counts: Vec<usize> = self
            .emphases
            .classes
            .iter()
            .map(|class| self.end_count(at, class))
            .collect();
        while let Some(min) = (0..counts.len())
            .filter(|&i| counts[i] > 0)
            .min_by_key(|&i| counts[i])
        {
            counts[min] = 0;
            self.insert_end(at, &self.emphases.classes[min], out);
        }
    }

    fn emphasis_begin(&self, at: usize, out: &mut Vec<ResolvedTranslation>) {
        let classes = &self.emphases.classes;
        let mut counts: Vec<usize> = classes
            .iter()
            .map(|class| self.begin_count(at, class))
            .collect();
        loop {
            // of equal counts the later class goes first
            let mut max: Option<usize> = None;
            for i in (0..counts.len()).rev() {
                if counts[i] > 0 && max.is_none_or(|m| counts[i] > counts[m]) {
                    max = Some(i);
                }
            }
            let Some(max) = max else { break };
            counts[max] = 0;
            self.insert_begin(at, &classes[max], out);
        }
        for class in classes.iter().rev() {
            if self.buffer[at].any() & class.bit != 0 {
                self.insert_symbol(at, class, out);
            }
        }
    }

    fn end_count(&self, at: usize, class: &Class) -> usize {
        let v = class.bit;
        if self.buffer[at].end & v == 0 {
            return 0;
        }
        1 + (0..at)
            .rev()
            .take_while(|&i| (self.buffer[i].begin | self.buffer[i].word) & v == 0)
            .count()
    }

    fn begin_count(&self, at: usize, class: &Class) -> usize {
        let v = class.bit;
        let n = self.chars.len();
        if self.buffer[at].begin & v != 0 {
            1 + (at + 1..n)
                .take_while(|&i| self.buffer[i].end & v == 0)
                .count()
        } else if self.buffer[at].word & v != 0 {
            let a = &self.emphases.attributes;
            1 + (at + 1..n)
                .take_while(|&i| {
                    let c = self.chars[i];
                    self.buffer[i].end & v == 0
                        && !a.seqdelimiter.contains(&c)
                        && !self.emphases.is_emph_space(c, class)
                })
                .count()
        } else {
            0
        }
    }

    fn insert_symbol(&self, at: usize, class: &Class, out: &mut Vec<ResolvedTranslation>) {
        if self.buffer[at].symbol & class.bit != 0
            && let Some(t) = &class.rules.letter
        {
            out.push(t.clone());
        }
    }

    fn insert_begin(&self, at: usize, class: &Class, out: &mut Vec<ResolvedTranslation>) {
        let v = class.bit;
        let b = self.buffer[at];
        let rules = &class.rules;
        if b.begin & v != 0
            && let Some(t) = rules.begphrase.as_ref().or(rules.beg.as_ref())
        {
            out.push(t.clone());
        }
        if b.word & v != 0
            && b.end & v == 0
            && let Some(t) = &rules.begword
        {
            out.push(t.clone());
        }
    }

    fn insert_end(&self, at: usize, class: &Class, out: &mut Vec<ResolvedTranslation>) {
        let v = class.bit;
        let b = self.buffer[at];
        let rules = &class.rules;
        if b.end & v == 0 {
            return;
        }
        let t = if b.word & v != 0 {
            rules.endword.as_ref()
        } else {
            rules
                .end
                .as_ref()
                .or(rules.endphrase_after.as_ref())
                .or(rules.endphrase_before.as_ref())
        };
        if let Some(t) = t {
            out.push(t.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{CharacterClasses, RuleParser};

    fn rule(s: &str) -> AnchoredRule {
        AnchoredRule::new(RuleParser::new(s).rule().unwrap(), None, 0)
    }

    fn ctx() -> TableContext {
        TableContext::new(
            CharacterClasses::new(&[
                (CharacterClass::Space, &[' ']),
                (
                    CharacterClass::Letter,
                    &['a', 'b', 'c', 'd', 'A', 'B', 'C', 'D'],
                ),
                (CharacterClass::Uppercase, &['A', 'B', 'C', 'D']),
                (CharacterClass::Lowercase, &['a', 'b', 'c', 'd']),
            ]),
            CharacterClasses::default(),
            Default::default(),
        )
    }

    fn caps_builder() -> IndicatorBuilder {
        let mut b = IndicatorBuilder::new();
        for (upper, lower) in [('A', 'a'), ('B', 'b'), ('C', 'c'), ('D', 'd')] {
            b.base("uppercase", upper, lower);
        }
        b.capsletter("⠨", &rule("capsletter 46"));
        b.begcapsword("⠘", &rule("begcapsword 45"));
        b.endcapsword("⠠", &rule("endcapsword 6"));
        b
    }

    /// The indicators inserted at each position when the loop visits every one of them
    fn visit_all(emphases: &Emphases, input: &str, spans: &[EmphasisSpan]) -> Vec<String> {
        let chars: Vec<char> = input.chars().collect();
        let marks = emphases.mark(&chars, spans);
        (0..=chars.len())
            .map(|pos| {
                marks
                    .caps_end_at(pos)
                    .into_iter()
                    .chain(marks.emphasis_at(pos))
                    .chain(marks.caps_begin_at(pos))
                    .map(|t| t.output().to_string())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn capital_tiers() {
        let emphases = caps_builder().build(&ctx()).unwrap();
        assert_eq!(visit_all(&emphases, "Ab", &[]), vec!["⠨", "", ""]);
        assert_eq!(visit_all(&emphases, "AB a", &[]), vec!["⠘", "", "", "", ""]);
        assert_eq!(visit_all(&emphases, "ABc", &[]), vec!["⠘", "", "⠠", ""]);
    }

    #[test]
    fn capital_start_repeats_on_each_visit() {
        let emphases = caps_builder().build(&ctx()).unwrap();
        let marks = emphases.mark(&['A'], &[]);
        assert_eq!(marks.caps_begin_at(0)[0].output(), "⠨");
        assert!(marks.caps_end_at(0).is_empty());
    }

    #[test]
    fn marks_inside_a_word() {
        let emphases = caps_builder().build(&ctx()).unwrap();
        let chars: Vec<char> = "aBCd".chars().collect();
        let marks = emphases.mark(&chars, &[]);
        assert!(marks.changes_within(0, 2));
        assert!(!marks.changes_within(1, 1));
        assert_eq!(marks.caps_begin_at(1)[0].output(), "⠘");
        assert_eq!(marks.caps_end_at(3)[0].output(), "⠠");
    }

    #[test]
    fn emphasis_words_and_passages() {
        let mut b = IndicatorBuilder::new();
        b.emphclass("italic");
        b.begemphword("italic", "⠸", &rule("begemphword italic 456"));
        b.endemphword("italic", "⠠", &rule("endemphword italic 6"));
        b.begemphphrase("italic", "⠸⠸", &rule("begemphphrase italic 456-456"));
        b.endemphphrase(
            "italic",
            "⠠⠄",
            &Position::After,
            &rule("endemphphrase italic after 6-3"),
        );
        b.lenemphphrase("italic", 2);
        let emphases = b.build(&ctx()).unwrap();
        let italic = |range| [EmphasisSpan::new("italic", range)];
        assert_eq!(
            visit_all(&emphases, "ab cd", &italic(3..5)),
            vec!["", "", "", "⠸", "", ""]
        );
        assert_eq!(
            visit_all(&emphases, "abcd", &italic(0..2)),
            vec!["⠸", "", "⠠", "", ""]
        );
        assert_eq!(
            visit_all(&emphases, "ab cd", &italic(0..5)),
            vec!["⠸⠸", "", "", "", "", "⠠⠄"]
        );
    }
}
