# moteado

Small Rust CLI for adding Wikidata Markdown links to UTF-8 text, Markdown
(`.md`), Quarto (`.qmd`), and tabular text (`.csv`, `.tsv`, `.ssv`).
It preserves the original delimiters, quoting, spacing, and line endings;
it does **not** parse or re-export tables.

The name **moteado** was chosen with the [colonizer crate](https://crates.io/crates/colonizer)
(`inspire` mode, which suggests names from the Catalogue of Life). It refers
to the Colima giant whiptail, *Aspidoscelis communis*: a Mexican lizard in
the genus *Aspidoscelis*. See its [Wikidata item](https://www.wikidata.org/wiki/Q2866833)
and [Catalogue of Life record](https://www.catalogueoflife.org/data/taxon/6874D).

[![Colima giant whiptail (Aspidoscelis communis), photographed in Jalisco, Mexico](https://thumb.wikimedia.org/wikipedia/commons/thumb/8/87/Aspidoscelis_communis.jpg/330px-Aspidoscelis_communis.jpg)](https://commons.wikimedia.org/wiki/File:Aspidoscelis_communis.jpg)

*Photo: [Diana Fuentes](https://commons.wikimedia.org/wiki/File:Aspidoscelis_communis.jpg),
[CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/).*

## Usage

From the repository root, write the annotated manuscript and audit log next
to the source in `fixtures/`:

```sh
cargo build --release
./target/release/moteado --help
./target/release/moteado fixtures/manuscript.qmd -o fixtures/linked.qmd
./target/release/moteado --chemical "quillaic acid" \
  --taxon "Quillaja saponaria" --log fixtures/review.log \
  fixtures/manuscript.qmd -o fixtures/linked.qmd
./target/release/moteado --auto-taxa --auto-chemicals \
  fixtures/manuscript.qmd -o fixtures/linked.qmd --log fixtures/review.log
```

To modify the input itself (back it up first if you need the original):

```sh
./target/release/moteado --auto-taxa --auto-chemicals --in-place \
  fixtures/manuscript.qmd --log fixtures/review.log
```

Input defaults to a single positional file; use `-` for stdin. Output defaults
to stdout; the audit log goes to stderr or to `--log FILE`. `-o/--output FILE`
and `--log FILE` write separately; the CLI refuses to overwrite the input or
use the same path for output and log. Repeat `--chemical NAME` / `--taxon NAME`
for explicit targets; `--auto-taxa` and `--auto-chemicals` discover candidates
without listing them in advance. Name lookup needs access to Wikidata; ID-only
linking works offline.

`--in-place` works with any regular UTF-8 input file, not just `.qmd`. It
cannot be combined with `-o/--output` or stdin (`-`), and refuses symlinks.
It writes a temporary file in the same directory, preserves the file's
permissions, and replaces the source only after the complete result is ready.
If a lookup fails, or the file differs when replacement is attempted, the
source stays untouched and the log records the error. Avoid concurrent edits
during the replacement itself. Already-linked files are not rewritten. Keep
a backup if you need the original: replacement also detaches hard links and
does not preserve filesystem metadata beyond permissions.

## What gets linked

Example:

```text
Q27133 and P235; quillaic acid from Quillaja saponaria
```

with `--chemical "quillaic acid" --taxon "Quillaja saponaria"` becomes:

```markdown
[Q27133](https://www.wikidata.org/wiki/Q27133) and [P235](https://www.wikidata.org/wiki/P235); [quillaic acid](https://www.wikidata.org/wiki/Q27108137) from [Quillaja saponaria](https://www.wikidata.org/wiki/Q2382775)
```

Bare uppercase `Q`/`P` followed by a nonzero decimal number is linked by
syntax, **not verified for existence** on Wikidata. Standalone IDs in inline
code also become clickable, retaining their code styling: `` `Q116482353` ``
becomes `` [`Q116482353`](https://www.wikidata.org/wiki/Q116482353) ``,
and `` `wd:Q27108137` `` links to Q27108137. Other inline code, fenced
code (including Quarto executable chunks), existing links, images, HTML,
and HTTP(S) URLs stay untouched. A code span containing additional text is
left intact rather than partially rewriting code. Text uses Markdown link
syntax even in CSV/TSV/SSV fields; the file is not reformatted, but consumers
that require strict field formats should process links downstream instead.

### Automatic discovery

Opt in with `--auto-taxa` and/or `--auto-chemicals`. The CLI finds likely
genus–species binomials (italicized names first, then unitalicized names in
prose or tables), names ending in `acid`, common chemical-looking suffixes
such as `-ine` and `-one`, and capitalized chemical names ending in a letter
such as `Hyperelodione D`. It queries [Wikidata Query Service](https://query.wikidata.org/)
in batches for **exact** English labels/aliases with an InChIKey (`P235`),
or exact scientific names (`P225`). Non-matches are left alone; ambiguous
matches are logged and left unchanged. This avoids querying every word:
ordinary words can also be entity labels, while querying every n-gram of a
manuscript would overwhelm Wikidata and produce misleading links. Up to 300
candidates per category are considered; a limit warning goes into the review
log. Heuristics can miss names with uncommon spelling or punctuation: pass
`--chemical` or `--taxon` to handle one explicitly.

## Progress and audit log

Interactive terminals show a progress bar on stderr while name lookups run;
the spinner keeps moving during a remote batch request. The count is
**candidates processed**, not links found. Piped runs omit terminal animation.
`--log` records the completed count (`PROGRESS`), every inserted link (`LINK`,
with original line and character column, source text and resulting Markdown),
and any ambiguity or failure (`REVIEW`/`ERROR`). For example:

```text
PROGRESS name lookups: 2/2 candidates processed
LINK 1:1 "Q27133" -> [Q27133](https://www.wikidata.org/wiki/Q27133)
LINK 2:6 "quillaic acid" -> [quillaic acid](https://www.wikidata.org/wiki/Q27108137)
```

The log is plain text, without progress-bar control characters. It also
records links made from bare IDs when no name lookups were requested.

## Name resolution and review

Explicit `--chemical`/`--taxon` names use Wikidata's search API and exact
case-insensitive English label/alias matching, then require an InChIKey
(`P235`) for chemicals or a matching scientific taxon name (`P225`) for taxa.
Automatic discovery uses exact, property-backed batch queries; it tolerates
initial capitalization of chemical names. Multiple candidates stay unchanged
and are listed in the review log; a sole individual chemical is preferred
over matching `group of stereoisomers` items, with that decision logged.
The `quillaic acid` lookup currently produces this review note because
Wikidata also has a stereoisomer-group record. Failed lookups and candidate
limits are logged; network failures cause a nonzero exit status. Review
Wikidata changes and scientific nomenclature before publishing.

Run `cargo test` for local deterministic text-linking checks.

