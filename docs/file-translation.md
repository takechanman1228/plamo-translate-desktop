# File translation

Choose one TXT, Markdown, DOCX or PPTX file in the Files tab. Language, style,
context and glossary settings are shared with text translation. Processing stays
local. Save creates a new file; the original and existing destinations are never
replaced. There is no batch queue or resume after the app exits.

## Supported content

| Format | Translated content | Preserved content and limits |
| --- | --- | --- |
| TXT | The same paragraph/chunk path as pasted text | Up to 4 MiB; UTF-8, optionally with BOM, or BOM-marked UTF-16. UTF-16 saves as UTF-8. Uniform line endings are preserved. Legacy encodings must be converted explicitly before opening. |
| Markdown | Paragraphs (including soft wraps), list items, headings and table cells | Code, math, link targets, front matter and raw HTML remain intact. Unsupported inline structures and raw HTML are reported by line. Punctuation is escaped where needed to preserve syntax. |
| DOCX | Supported body, table, header, footer, footnote, endnote and text-box paragraphs | Drawings, links, supported bookmarks and fields retain their structure. Revisions, bound/typed controls and unsupported ranges remain in source. Compatibility fallback drawings remain unchanged. Word may therefore display source text when using a fallback representation. |
| PPTX | Supported slide, table and speaker-note paragraphs | Paragraph boundaries and inherited bullet styles stay intact. Charts, SmartArt and layout/master text remain in source. Only font sizes may be reduced; shapes, table geometry and paragraph spacing are not moved or resized. |

PDF file translation is outside this feature. The browser extension's existing
PDF viewer is unaffected.

## Reviewing a result

Open **Show / hide review locations** for applied/retained paragraph or line
locations and layout warnings. Applied means that validated text was written;
it is not a measure of translation accuracy. An unchanged or wholly rejected
result cannot be saved. Cancellation discards the unfinished result.

Word pagination, floating objects, fixed-height table rows and text boxes need
native Office review. PowerPoint fitting is an estimate, not a renderer: font
substitution, complex scripts and shapes can still overflow. Text inside images
is not detected or translated. Verify numbers, negation, conditions and terminology
against the original before sharing a translation.

## Implementation limits

Office archives are limited to 128 MiB compressed, 512 MiB expanded, 4,096 entries
and 8 MiB per XML part. Encrypted archives, unsafe ZIP entries and DOCTYPE-bearing
parsed XML are rejected. Internal relationships resolve supported content parts;
URI-escaped paths and nonstandard package layouts may be rejected. Unsupported
parts are copied, never rewritten just to normalize their XML.

Tagged translation units are capped at 1,024 input tokens to avoid splitting
protected runs. Long or invalid units retain source text. Amount checks cover
selected scaled yen expressions for Japanese/English, not every number or language.

## Verification

The format adapters have no model, native Office or PDF runtime dependency:

```sh
cargo test -p files --release --locked
cargo test --workspace --release --locked
cargo fmt --all --check
cargo clippy --workspace --all-targets --release --locked -- -D warnings
```

CI includes model-free adapter tests on Linux, macOS and Windows. Unit tests cover
nested Word text boxes, protected fields/ranges, inherited PowerPoint bullets,
Markdown structure and non-overwriting saves. Native Office opening and translation
quality on representative documents require separate acceptance checks; unit-test
success does not establish layout or semantic accuracy.
