# File translation development status

This fork adds a Files tab for TXT, Markdown, DOCX, PPTX and text-layer PDF. It is a
local trial, not an upstream release or a claim of production translation quality.
The existing text translator and browser extension remain available.

## Scope

- Source files and existing destinations are never overwritten. Save to a new path.
- Results list applied translations and retained source locations. Applied counts
  measure processing, not accuracy. A result with no applicable translation cannot be saved.
- Office export preserves unsupported content and reports it. Font shrinking is the
  only automatic layout adjustment; shape movement, table resizing and pagination
  repair are not performed. Word floating objects and fixed-height rows require review.
- PowerPoint charts, SmartArt, layout/master text and some Word fields, controls and
  text boxes remain untranslated. Check the reported locations before sharing.
- PDF preserves source pages and inserts selectable translation pages. It does not
  recreate diagrams. Complex table relationships and reading order can be wrong.
  OCR and preservation of interactive forms or signatures are unsupported.
- Machine translation can reverse negation or misinterpret conditions and amounts.
  Numerical checks are limited and do not establish semantic correctness.

## Build and trial

Use the pinned Rust toolchain and the platform dependencies in the main README.
On macOS, `scripts/local-dev.sh` provides the SDK/header environment for Cargo.

```sh
scripts/local-dev.sh test --workspace --release --locked
scripts/local-dev.sh clippy --workspace --all-targets --release --locked -- -D warnings
scripts/package-local-macos.sh
```

The local package uses separate settings, logs and a single-instance lock. It disables
updates, clipboard double-copy and residency at startup. Models are not bundled;
select/download a model in the app or configure its model directory.

PDF requires PDFium and a Japanese-capable TrueType font. Packaging options and the
tested PDFium distribution are described in the README. Set `PLAMO_LOCAL_IPAFONT_DIR`
to an unmodified IPAfont00303 directory to copy the font with its license and README.
No runtime binaries, model weights or third-party test documents are committed here.
The PDF continuation test is ignored without these optional dependencies; run it with
`PLAMO_PDFIUM_LIBRARY` and `PLAMO_PDF_FONT` set to absolute paths:

```sh
scripts/local-dev.sh test exported_pdf_keeps_every_line_and_places_continuations_before_next_source_page -- --ignored
```

## Verification limits

Local checks cover source protection, supported XML structures, Markdown syntax,
translation boundaries, cancellation and non-overwriting saves. Representative outputs
have also been opened, edited and saved in native Word and PowerPoint on macOS.
They do not guarantee unchanged page counts, perfect layout, or correct translations.

The development corpus is biased toward public-sector forms and explanatory slides.
Independent business/academic document acceptance and user trial are still pending.
Windows/Linux file translation acceptance has not been performed. PDF packaging is
currently scoped to macOS local trials. Download links elsewhere in the README refer
to upstream releases, which do not contain this fork's file translation changes.
