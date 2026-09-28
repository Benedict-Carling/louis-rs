//! Find tables by their `#+key: value` metadata, scoring every table against a query the way
//! liblouis's `lou_findTable` does.

use std::{cmp::Ordering, collections::HashMap, fmt, fs, path::PathBuf, sync::LazyLock};

#[derive(thiserror::Error, Debug)]
pub enum MetaDataError {
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    #[error("{0:?} is not a key=value pair")]
    InvalidQuery(String),
}

use search_path::SearchPath;

/// A query into the metadata, i.e. a set of `key=value` pairs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query(Vec<(String, String)>);

impl Query {
    /// Parse the `key=value,key=value` form the `query` subcommand takes.
    pub fn parse(query: &str) -> Result<Self, MetaDataError> {
        query
            .split(',')
            .map(|pair| {
                pair.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .ok_or_else(|| MetaDataError::InvalidQuery(pair.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    /// The query's features, with the `unicode-range` a query implies when it names none.
    /// `None` when a key or value is malformed, which matches no table.
    fn features(&self) -> Option<Vec<Feature>> {
        let mut features = Vec::new();
        for (key, value) in &self.0 {
            if key.is_empty() || !key.chars().all(is_valid_char) {
                return None;
            }
            if value.is_empty() || !value.chars().all(is_valid_char) {
                return None;
            }
            features.extend(Feature::parse(key, value)?);
        }
        if !features.iter().any(|f| f.key == "unicode-range") {
            features.push(Feature::new("unicode-range", Value::Plain("ucs4".into())));
        }
        features.sort_by(|a, b| a.key.cmp(&b.key));
        Some(features)
    }
}

impl From<&HashMap<String, String>> for Query {
    fn from(query: &HashMap<String, String>) -> Self {
        Self(query.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
    }
}

impl fmt::Display for Query {
    /// Sorted, so that an error message naming a query does not depend on the order
    /// the pairs happened to be collected in.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut pairs: Vec<_> = self.0.iter().map(|(k, v)| format!("{k}={v}")).collect();
        pairs.sort();
        write!(f, "{}", pairs.join(","))
    }
}

fn is_valid_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_'
}

fn is_language_tag(key: &str) -> bool {
    matches!(key, "language" | "region" | "locale")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Plain(String),
    /// A language tag split into its lowercased subtags
    Tag(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Feature {
    /// lowercased, as liblouis compares keys ignoring case
    key: String,
    value: Value,
}

impl Feature {
    fn new(key: &str, value: Value) -> Self {
        Self {
            key: key.to_string(),
            value,
        }
    }

    /// `locale` is shorthand for a `language` and a `region`.
    fn parse(key: &str, value: &str) -> Option<Vec<Self>> {
        let key = key.to_ascii_lowercase();
        if !is_language_tag(&key) {
            return Some(vec![Self::new(
                &key,
                Value::Plain(value.to_ascii_lowercase()),
            )]);
        }
        let tag = Value::Tag(parse_language_tag(value)?);
        if key == "locale" {
            Some(vec![
                Self::new("language", tag.clone()),
                Self::new("region", tag),
            ])
        } else {
            Some(vec![Self::new(&key, tag)])
        }
    }

    fn cmp(&self, other: &Self) -> Ordering {
        self.key
            .cmp(&other.key)
            .then_with(|| match (&self.value, &other.value) {
                (Value::Tag(a), Value::Tag(b)) => a.cmp(b),
                (Value::Plain(a), Value::Plain(b)) => a.cmp(b),
                (Value::Tag(_), Value::Plain(_)) => Ordering::Less,
                (Value::Plain(_), Value::Tag(_)) => Ordering::Greater,
            })
    }
}

/// Subtags of up to eight alphanumerics separated by `-`, the first alphabetic or `*`.
fn parse_language_tag(value: &str) -> Option<Vec<String>> {
    let mut subtags = Vec::new();
    let mut rest = value;
    if let Some(after) = rest.strip_prefix('*') {
        subtags.push("*".to_string());
        if after.is_empty() {
            return Some(subtags);
        }
        rest = after.strip_prefix('-')?;
    }
    for subtag in rest.split('-') {
        let valid = (1..=8).contains(&subtag.len())
            && subtag
                .chars()
                .all(|c| c.is_ascii_alphabetic() || (!subtags.is_empty() && c.is_ascii_digit()));
        if !valid {
            return None;
        }
        subtags.push(subtag.to_ascii_lowercase());
    }
    Some(subtags)
}

/// How well `tag` falls in the language `range`: 10 for a perfect match, two less for each
/// subtag without an exact counterpart, and 0 for no match (RFC 4647 extended filtering).
fn match_language_tags(tag: &[String], range: &[String]) -> i32 {
    const EXTRA: i32 = -2;
    let mut q = 10;
    if range[0] == "*" {
        q += EXTRA;
    } else if tag[0] != range[0] {
        return 0;
    }
    let (mut tag, mut range) = (&tag[1..], &range[1..]);
    while let Some(wanted) = range.first() {
        let Some(subtag) = tag.first() else {
            return 0;
        };
        if subtag == wanted {
            range = &range[1..];
        } else if subtag.len() == 1 {
            return 0;
        } else {
            q += EXTRA;
        }
        tag = &tag[1..];
    }
    q + EXTRA * tag.len() as i32
}

/// The match quotient of a table's `features` for the `query`, both sorted by key, as
/// liblouis's `matchFeatureLists` computes it without fuzziness.
fn match_quotient(query: &[Feature], table: &[Feature]) -> i32 {
    const POS_MATCH: i32 = 10;
    const NEG_MATCH: i32 = -100;
    const UNDEFINED: i32 = -20;
    const EXTRA: i32 = -1;
    let mut quotient = 0;
    let (mut query, mut table) = (query, table);
    let same_key = |features: &[Feature]| {
        features
            .iter()
            .take_while(|f| f.key == features[0].key)
            .count()
    };
    loop {
        match (query.first(), table.first()) {
            (None, None) => break,
            (None, Some(_)) => {
                quotient += EXTRA;
                table = &table[same_key(table)..];
            }
            (Some(_), None) => {
                quotient += UNDEFINED;
                query = &query[1..];
            }
            (Some(wanted), Some(offered)) => match wanted.key.cmp(&offered.key) {
                Ordering::Less => {
                    quotient += UNDEFINED;
                    query = &query[1..];
                }
                Ordering::Greater => {
                    quotient += EXTRA;
                    table = &table[same_key(table)..];
                }
                Ordering::Equal => {
                    let n = same_key(table);
                    let mut best = NEG_MATCH;
                    if is_language_tag(&wanted.key) {
                        let mut extra_languages = 0;
                        for offered in &table[..n] {
                            let (Value::Tag(tag), Value::Tag(range)) =
                                (&wanted.value, &offered.value)
                            else {
                                continue;
                            };
                            let q = match_language_tags(tag, range);
                            if q > 0 && q > best {
                                best = q;
                            } else if q == 0 {
                                extra_languages += EXTRA;
                            }
                        }
                        if best > 0 {
                            best += (extra_languages + 4) / 5;
                        }
                    } else {
                        for offered in &table[..n] {
                            if best >= 0 {
                                continue;
                            }
                            if wanted.value == offered.value {
                                best = POS_MATCH;
                            } else if wanted.key == "unicode-range"
                                && wanted.value == Value::Plain("ucs4".into())
                                && offered.value == Value::Plain("ucs2".into())
                            {
                                best = POS_MATCH - 1;
                            }
                        }
                    }
                    quotient += best;
                    query = &query[1..];
                    table = &table[n..];
                }
            },
        }
    }
    quotient
}

/// The features in a table's header, the `#+key: value` lines before its first rule, with
/// `region` defaulting to the `language` and `unicode-range` to `ucs2`. `None` when a
/// header line is malformed, which keeps liblouis from indexing the table at all.
fn table_features(content: &str) -> Option<Vec<Feature>> {
    let mut features = Vec::new();
    for line in content.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if !line.starts_with('#') {
            break;
        }
        let Some(line) = line.strip_prefix("#+") else {
            continue;
        };
        let key_len = line.find(|c| !is_valid_char(c)).unwrap_or(line.len());
        if key_len == 0 {
            return None;
        }
        let (key, rest) = line.split_at(key_len);
        let value = rest.strip_prefix(':')?.trim_start_matches([' ', '\t']);
        let first = value.chars().next()?;
        if !is_valid_char(first) && !(is_language_tag(&key.to_ascii_lowercase()) && first == '*') {
            return None;
        }
        if !value[1..].chars().all(is_valid_char) {
            return None;
        }
        features.extend(Feature::parse(key, value)?);
    }
    if !features.iter().any(|f| f.key == "region")
        && let Some(language) = features.iter().find(|f| f.key == "language")
    {
        features.push(Feature::new("region", language.value.clone()));
    }
    if !features.is_empty() && !features.iter().any(|f| f.key == "unicode-range") {
        features.push(Feature::new("unicode-range", Value::Plain("ucs2".into())));
    }
    features.sort_by(Feature::cmp);
    Some(features)
}

/// The metadata of every table on the search path.
pub struct Index(Vec<(PathBuf, Vec<Feature>)>);

static INDEX: LazyLock<Result<Index, MetaDataError>> =
    LazyLock::new(|| Index::build(&SearchPath::new_or("LOUIS_TABLE_PATH", ".")));

/// The index of the tables, built once on first use.
pub fn index() -> Result<&'static Index, &'static MetaDataError> {
    INDEX.as_ref()
}

impl Index {
    /// The tables that match the query best. liblouis takes the first of several equally
    /// good tables in directory listing order, which is no ordering at all, so a tie is left
    /// for the caller to report.
    pub fn find(&self, query: Query) -> Vec<&PathBuf> {
        let Some(query) = query.features() else {
            return Vec::new();
        };
        let scored: Vec<_> = self
            .0
            .iter()
            .map(|(path, features)| (path, match_quotient(&query, features)))
            .collect();
        let best = scored.iter().map(|(_, q)| *q).max().unwrap_or(0);
        if best <= 0 {
            return Vec::new();
        }
        let mut best: Vec<_> = scored
            .into_iter()
            .filter(|(_, q)| *q == best)
            .map(|(path, _)| path)
            .collect();
        best.sort();
        best
    }

    /// Derive the index by walking the whole of `search_path` and reading the
    /// metadata out of every table on it.
    pub fn build(search_path: &SearchPath) -> Result<Self, MetaDataError> {
        let mut index = Vec::new();
        for dir in search_path.iter() {
            for entry in fs::read_dir(dir)? {
                let path = entry?.path();
                if !path.is_file() {
                    continue;
                }
                let Ok(bytes) = fs::read(&path) else {
                    continue;
                };
                let content = String::from_utf8_lossy(&bytes);
                if let Some(features) = table_features(&content)
                    && !features.is_empty()
                {
                    index.push((path, features));
                }
            }
        }
        Ok(Self(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(tables: &[(&str, &str)]) -> Index {
        Index(
            tables
                .iter()
                .map(|(name, header)| (PathBuf::from(name), table_features(header).unwrap()))
                .collect(),
        )
    }

    fn find<'a>(index: &'a Index, query: &str) -> Vec<&'a PathBuf> {
        index.find(Query::parse(query).unwrap())
    }

    #[test]
    fn the_best_scoring_table_wins() {
        let index = index(&[
            (
                "de-g0.utb",
                "#+language: de\n#+grade: 0\n#+type: literary\n",
            ),
            (
                "de-g0-detailed.utb",
                "#+language: de\n#+grade: 0\n#+type: literary\n#+detailed: yes\n",
            ),
            (
                "de-g1.ctb",
                "#+language: de\n#+grade: 1\n#+type: literary\n",
            ),
        ]);
        // the detailed table carries a feature the query does not ask for
        assert_eq!(
            find(&index, "language=de,grade=0"),
            [&PathBuf::from("de-g0.utb")]
        );
    }

    #[test]
    fn a_ucs4_query_prefers_a_ucs4_table() {
        let index = index(&[
            ("ja-ucs2.utb", "#+language: ja\n"),
            ("ja.utb", "#+language: ja\n#+unicode-range: ucs4\n"),
        ]);
        assert_eq!(find(&index, "language=ja"), [&PathBuf::from("ja.utb")]);
    }

    #[test]
    fn language_tags_match_as_ranges() {
        let index = index(&[
            ("en.utb", "#+language: en\n"),
            ("grc.utb", "#+language: grc\n#+region: grc-en\n"),
        ]);
        assert_eq!(
            find(&index, "language=grc,region=grc-en"),
            [&PathBuf::from("grc.utb")]
        );
        assert!(find(&index, "language=fr").is_empty());
    }

    #[test]
    fn equally_good_tables_are_all_returned() {
        let index = index(&[("b.utb", "#+language: de\n"), ("a.utb", "#+language: de\n")]);
        assert_eq!(
            find(&index, "language=de"),
            [&PathBuf::from("a.utb"), &PathBuf::from("b.utb")]
        );
    }

    #[test]
    fn only_the_header_is_metadata() {
        assert_eq!(
            table_features("#+language: de\nletter a 1\n#+grade: 2\n").unwrap(),
            table_features("#+language: de\n").unwrap()
        );
        assert!(table_features("#+language: de and more\n").is_none());
    }
}
