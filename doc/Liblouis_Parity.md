# louis-rs vs. liblouis

louis-rs is a from-scratch Rust reimplementation of [liblouis](https://liblouis.io/) —
not a port. Different data structures, different algorithms, same tables and the same
test suite.

This document says what it can do today, what it can't yet, and where the remaining
work is, so you can tell whether it's usable for your case and pick something up if
it isn't.

Measured at louis-rs `4980ced` against liblouis `011cfb3b`:

|                                |           |
|--------------------------------|-----------|
| Assertions run                 | 2 241 236 |
| Pass                           | **98.6%** |
| Fail                           | 0.9%      |
| Expected failure               | 0.5%      |
| Table files that parse cleanly | 458 / 458 |

Forward and backward combined. Reproduce the assertion figures with:

```sh
export LOUIS_TABLE_PATH=~/src/liblouis/tables:~/src/liblouis
cargo run --release -- check \
    ~/src/liblouis/tests/braille-specs/*.yaml \
    ~/src/liblouis/tests/yaml/*.yaml --summary
```

and the parse figure with:

```sh
cargo build --release
export LOUIS_TABLE_PATH=~/src/liblouis/tables
for f in ~/src/liblouis/tables/*.{ctb,utb,uti,tbl,cti,dis}; do
    ./target/release/louis parse "$f" > /dev/null
done
```

## What works today

**Translation, both directions.** The full multi-stage pipeline: `correct` rules
before the main stage, character and pattern rules in it, `pass2`–`pass4` after —
reversed for back-translation.

**Every table under `tables/` parses** — all 458 of them.

**Contractions and the word-family opcodes** — `word`, `begword`, `midword`,
`endword`, `midendword`, `prfword`, `sufword`, `partword`, `begmidword`, with their
`before`/`after` class constraints.

**`match` and `context` rules** via a purpose-built regexp VM (`src/translator/regexp.rs`)
that compiles patterns at table load time and matches in linear time.

**Braille indicators.** Number sign (`numsign`/`nonumsign`), capitalization at all
three tiers — `capsletter`, `begcapsword`/`endcapsword`, and caps passages via
`begcaps`/`endcaps` or `begcapsphrase`/`endcapsphrase`/`lencapsphrase` — plus letter
sign, `nocontract`, and the generic `begmode`/`endmode` family.

**Emphasis.** Capitals and the typeform classes share liblouis's model
(`src/translator/indication/emphasis.rs`): `markEmphases` resolves begin/end/word/symbol
marks per class, and the translation loop inserts them as `insertEmphasesAt` does,
including the capital start indicator again when a zero-width rule leaves the position
unchanged. `correct` carries the typeforms over to its output.

**Computer braille** as a full pipeline feature — `comp6`, `compbrl`,
`begcomp`/`endcomp`, and a scanner that derives computer-braille spans from
table-defined trigger characters.

**Backward indication** for `capsletter`, `begcapsword`/`endcapsword`,
`numsign`/`nonumsign`, and the no-contract family `letsign`/`nocontractsign`/
`nonumsign` — the last of these both consumes the indicator cell and suppresses the
word-family rules in its scope.

**Caller-controlled table resolution.** `Translator::with_search_path` takes the
directories to search, so a host managing its own table locations doesn't have to
mutate `LOUIS_TABLE_PATH`.

**Hyphenation, natively.** liblouis `.dic` pattern files are parsed directly
(`src/hyphenation.rs`) — no external crate, no build or conversion step. Resolved
through the same search path as any other table. `nocross` rules follow liblouis's
`syllableBreak`/`hyphenateWord`, including its pattern automaton and lack of
`LEFTHYPHENMIN`/`RIGHTHYPHENMIN`.

**Position mapping.** `inputPos`, `outputPos` and `cursorPos`, composed across every
pipeline stage in both directions. One known mismatch remains in the whole suite.

**Display tables** as a pipeline stage, renders last when translating to braille and
first when back-translating. `--display` on `translate` and `trace`. The YAML harness,
like liblouis's checker, reads a block's translation table as its display table when
the file names none.

**Table metadata queries** — find tables by language, contraction grade and the rest.

**A CLI** with `parse`, `translate`, `trace` (shows every rule that fired), `check`
(runs YAML suites) and `query`.

## What doesn't work yet

Honest list, no dates attached:

- **The library API is not stable.** Expect breaking changes.
- **No C ABI and no bindings.** Rust consumers only, for now. PyO3 bindings are
  proposed in [#1](https://github.com/liblouis/louis-rs/issues/1) /
  [PR #12](https://github.com/liblouis/louis-rs/pull/12).
- **A screen-reader mode is missing**: `partialTrans`
  ([#19](https://github.com/liblouis/louis-rs/issues/19)). The option flag is
  accepted; nothing reads it yet.
- **Back-translation returns text but not typeform.** `TranslationResult::emphasis`
  is never populated, and nothing reports whether a back-translation result could
  still change if more cells arrive
  ([#29](https://github.com/liblouis/louis-rs/issues/29)).
- **No Unicode normalization.** NFC was implemented and reverted — it fixed tables
  with precomposed-character rules and broke tables with standalone
  combining-character rules (IPA, Hebrew, Yiddish). NFD is the proposed path.
- **Six opcodes are deliberately not implemented**: `uplow`, `locale`, `backmatch`,
  `compdots`, `nobreak` and `macro`. Two are deprecated upstream, three are
  undocumented in liblouis's own manual, and none appears in any table or test in
  liblouis's corpus. See the ADR of that name in
  [Architecture_Decision_Records.org](Architecture_Decision_Records.org).

## Where the remaining failures are

Most of the failing 0.9% sits in ten files, and several of those share a root cause.
Sizes are per item — *S* is up to a week, *M* about a week, *L* one to three weeks —
and are there so you can pick work by appetite, not to add up. Failure counts are
approximate: `check` reports a per-file percentage, so a count derived from it carries
a few hundred either way on the million-assertion files.

| Area                                                | Failing | Root cause                                                                                                                                                                       | Size |
|-----------------------------------------------------|--------:|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------|
| `afr-za-g2` forward                                 |      13 | Not yet surveyed. The dictionary harness it shared this row with passes since back-translation selects rules as liblouis does and a boundary that holds counts towards a `match` rule's weight. | M    |
| `ml`, `pa`, `hi`, `ta`, `fa`, `th`, `bn`            |    ≈950 | Complex scripts: conjunct and reph forms, and stacked combining marks firing an indicator each instead of once per grapheme cluster.                                             | L    |
| `da-dk-g28-dictionary_harness`                      |       9 | Not yet surveyed; the backward `letsign` ambiguity that made up most of it went with the back-translation rule selection. | S    |
| `hu-hu-g1_dictionary_special_consonants`            |      11 | Four `always` rules defined twice with different dots in `hu-exceptionwords.cti`; liblouis keeps the first, a definition-order tie (table bug).                                   | —    |
| `sw-ke-dictionary`, `sw-ke`                         |      37 | Not yet surveyed; the backward lookup that spliced in unrelated dictionary words went with the back-translation rule selection.                                                                    | M    |
| `en-g3`, `en-ueb`                                   |    ≈600 | A literal hyphen breaks per-segment contraction in compounds — "do-it-yourself" gives `do-x-y\|rself`, not `d-x-yrf`.                                                            | M    |
| `mixed-case`                                        |       5 | liblouis fails the same five tests with the same output.                                                                                                                         | —    |
| all other files                                     |  ≈2 500 | Roughly 100 files, each contributing well under 350.                                                                                                                             | —    |

Two cross-cutting items have no single file behind them:

- **Backward direction, generally.** Rule selection follows liblouis's
  `back_selectRule` (`src/translator/table/backward.rs`): the side before a rule is the
  last character of the text produced so far, classified by the text classes; the side
  after is the next cell's attributes or `isEndWord`'s scan; rules are tried in
  liblouis's order; and `itsANumber` and `noContractMode` are state set by the rules
  applied, as are capitalization's `nextUpper`/`allUpper`/`allUpperPhrase`. Emphasis
  still has no backward path. *(M)*
- **Engine debt.** Regexp negation of concatenation and alternation is wrong
  (`!(e1e2)` ≠ `(!e1!e2)`); `Attribute::ByOrder` and `Any` are no-ops in
  `match_pattern.rs` and all three are no-ops in `context_pattern.rs`; the
  `syllable` opcode's cross-boundary restriction is unimplemented. A `_N` is honoured
  only at the start of a test (matched from N characters back); elsewhere it is
  dropped. *(M)*

## Forty-four blocks that never run

A YAML file is a sequence of blocks, each with its own table. A block whose table
won't load is reported on stderr and skipped, and the rest of the file still runs.
Forty-four blocks across thirteen files are skipped this way, so those tests report
neither passes nor failures — the one genuine unknown in this document, and the
cheapest work in it.

| Reason                                    | Blocks | What it needs                                                                                                                                                                                           | Size |
|-------------------------------------------|-------:|---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------|
| `multipass-vs-match_common.uti` not found |     25 | The include sits next to the YAML file rather than on `LOUIS_TABLE_PATH`. One missing fixture takes out every block of `multipass-vs-match.yaml`. | S    |
| Parser gaps                               |      6 | An `InvalidAction` and an `EmptyTest` in each of `multipass-forward.yaml` and `multipass-backward.yaml`, plus an `OpcodeExpected` and a surrogate-pair escape.                                          | S    |
| Ambiguous metadata queries                |      9 | `language=grc,region=en` and `language=de,grade=0` match two tables and `system=cmn-traditional` three. `lou_findTable` scores its candidates and always lands on one; we intersect and insist on exactly one.                  | M    |
| Hyphenation dictionaries                  |      2 | `hyph_en_US.dic` rejected for its ISO-8859-1 encoding; `hyphenation.dic` not found.                                                                                                                     | S    |
| `macro.utb` not found                     |      1 | Deliberate non-goal.                                                                                                                                                                                    | —    |
| A query matching no table at all          |      1 | `language=grc,region=es` finds nothing on the search path. Possibly a bug in the spec rather than in us.                                                                                                | S    |

Nine files report zero tests. Seven because every one of their blocks is skipped for
a reason above: `multipass-vs-match.yaml`, `macro.yaml`, `grc-international-es.yaml`,
`ja-kantenji.yaml`, `face-with-tears-of-joy.yaml`, `hyphenation.yaml` and
`hyphenation_nocross_harness.yaml`. `no_8dot_harness.yaml` has no tests left upstream;
they moved to `no.yaml`.

The last is a separate problem: the `hyphenate` and `hyphenateBraille` test modes fall
through a catch-all arm in `src/test.rs` and silently report nothing at all, and
`hu-hu-g1-hyph_harness.yaml` consists only of such tests. `issue-332.yaml` has its
`hyphenate` block skipped the same way while its remaining 19 tests all fail. *(S)*

The `display` test mode runs. Of `de-eurobrl6.yaml`'s three blocks, two are held back
by the ambiguous `{language: de, grade: 0}` query above; with the queries pinned to
`de-g0.utb`, `de-g1.ctb` and `de-g2.ctb`, all 16 of its assertions pass.

## Missing API surface

| Capability                                                                                      | Status                                                                                                                                                    | Size   |
|-------------------------------------------------------------------------------------------------|-----------------------------------------------------------------------------------------------------------------------------------------------------------|--------|
| `partialTrans` ([#19](https://github.com/liblouis/louis-rs/issues/19))                          | Flag accepted, nothing reads it. A screen reader sets it on every keystroke of contracted input; without it, word-final rules fire on an unfinished word. | M      |
| Back-translation finality ([#29](https://github.com/liblouis/louis-rs/issues/29))               | Nothing says whether more cells could change the characters already produced.                                                                             | M      |
| Display tables for library callers                                                              | `TranslationPipeline::with_display` and `louis translate/trace --display` exist; `Translator` doesn't expose it yet.                                       | S      |
| `hyphenate`, `charToDots`, `dotsToChar`, `getEmphClasses`                                       | Working internals, nothing exposed on `Translator`.                                                                                                       | S each |
| C ABI                                                                                           | Not started. cbindgen or Diplomat.                                                                                                                        | M–L    |

## Help wanted

Good entry points, roughly easiest first:

1. **Wake up the 44 blocks that don't run.** Several independent small fixes,
   listed above, and nobody knows what those tests will report. *(S each)*
2. **The three parser gaps.** Small, concrete, and each has a failing file to check
   against. *(S)*
3. **Expose the four missing entry points.** Mechanical. *(S each)*
4. **Survey a failure cluster.** Every row in the table above now names a cause, so
   what's left is the ~2 500 failures spread across roughly 100 smaller files, and
   the residue of the two clusters already surveyed. Reading failure output rather
   than deep engine work — and each of the three clusters that got this treatment
   turned out to be one or two root causes rather than a long tail. *(M)*

Match-rule candidate selection has one failed attempt behind it, so it's a poor first
contribution but well documented in
[Architecture_Decision_Records.org](Architecture_Decision_Records.org).

## Also open

- Rule-level coverage tool for the YAML suite
  ([#27](https://github.com/liblouis/louis-rs/issues/27)) — ADR accepted, prototype
  parked
- gzip+bincode table bundling ([PR #10](https://github.com/liblouis/louis-rs/pull/10))
- `hu-hu-g1` failures ([#4](https://github.com/liblouis/louis-rs/issues/4)): only the
  duplicated-rule ties above remain
- Translation performance — the regexp VM was 59% of translation time, and there is a
  measured 2.0–3.4× in hand plus a larger structural idea (compiling all of a stage's
  `match`/`context` patterns into one automaton) not yet attempted. Everything on the
  subject, including the variants that were measured and disproven, is in
  [Translation_Performance.md](Translation_Performance.md).

---

Working notes live in [TODO.org](../TODO.org); design decisions and the reasoning
behind them in
[Architecture_Decision_Records.org](Architecture_Decision_Records.org); where
translation time goes and what to do about it in
[Translation_Performance.md](Translation_Performance.md).
