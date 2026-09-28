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

**Emphasis.** Capitals, the other modes and the typeform classes share liblouis's model
(`src/translator/indication/emphasis.rs`): `markEmphases` resolves begin/end/word/symbol
marks per class (sections, words, passages, resets, letters), and the translation loop
inserts them in `insertEmphasesAt`'s order, including those at positions a rule spans
and the capital start indicator again when a zero-width rule leaves the position
unchanged. `correct` carries the typeforms over to its output. Back-translation reads
the emphasis indicators and reports the emphasis in `TranslationResult::emphasis` when
`TranslationOptions::with_emphasis_output` asks for it, as liblouis fills a typeform
buffer. `repword` and `rependword` are translated.

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
- **Nothing reports whether a back-translation result could still change** if more
  cells arrive ([#29](https://github.com/liblouis/louis-rs/issues/29)).
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
| `ml`, `ta`, `hi`, `pa`, `th`                        |      78 | Definition-order ties (table bugs): a `word` rule against an equally long `postpunc`/`prepunc`/`always` for the same punctuation (en-in-g1, en-us-g1, pu-in-g1), and `begmidword` against `endword` for the same conjunct in `malayalam.cti`. The "one indicator per grapheme cluster" theory did not hold: the causes were context `*` copying, lookbacks, `$w`–`$z`, context rule ranking, `decpoint`, `repeated`, `nocont` and undefined-character escapes. | —    |
| `da-dk_1993`, `da-dk-6dot`, `spaces`             |      16 | Definition-order ties (table bugs): `context $w["-"]$w` against `context $sw["-"]$sw` in the Danish tables (e.g. `da-dk-g16_1993.ctb:257`/`:258`), and `correct "\x200b"` in both `en-ueb-chardefs.uti:33` and `spaces.uti:58`. | —    |
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
  applied, as are capitalization's `nextUpper`/`allUpper`/`allUpperPhrase` and the emphasis
  of the text. *(M)*
- **Engine debt.** `correct`, `pass2`–`pass4` and forward `context` rules run on a port
  of liblouis's pass engine (`src/translator/pass.rs`): tests are programs run left to
  right without backtracking, rules are tried by their leading literal characters.
  Where liblouis then falls back on definition order, the rule that moves on further,
  then the one that looked at more input, then at more conditions, wins; exact ties are
  table bugs (see the ADR). Backward `context` rules with a leading literal still go
  through the pattern engine, where regexp negation of concatenation and alternation is
  wrong (`!(e1e2)` ≠ `(!e1!e2)`) and grouping never matches; `omit` and `;` ignore a
  grouping, and the `syllable` opcode's cross-boundary restriction is unimplemented. *(M)*

## Four blocks that never run

A YAML file is a sequence of blocks, each with its own table. A block whose table
won't load is reported on stderr and skipped, and the rest of the file still runs.
Tables are looked up in the YAML file's directory first, as `lou_checkyaml` changes
into it, and a metadata query takes the table `lou_findTable` scores best. Four blocks
are still skipped:

| Reason                          | Blocks | What it needs                                                                                                                                                                  | Size |
|---------------------------------|-------:|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|------|
| A tied metadata query           |      3 | `language=grc,region=en` scores `grc-international-en.utb` and `grc-international-en-composed.utb` equally; `lou_findTable` takes whichever the directory lists first.  | —    |
| `macro` opcode                  |      1 | Deliberate non-goal; the reference build cannot compile `macro.utb` either.                                                                                                    | —    |

The `hyphenate` and `hyphenateBraille` test modes run as `lou_hyphenate` does. Five of
`hu-hu-g1-hyph_harness.yaml`'s tests fail with the output liblouis gives too: its
checker prints hyphenation mismatches without counting them.

The `display` test mode runs, and all 16 assertions of `de-eurobrl6.yaml` pass.

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

1. **Expose the four missing entry points.** Mechanical. *(S each)*
2. **Survey a failure cluster.** Every row in the table above now names a cause, so
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
