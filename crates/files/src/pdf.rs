//! Local text-layer PDF translation. The original pages are copied into the output.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use pdfium_render::prelude::*;
use thiserror::Error;

const MAX_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PAGES: u16 = 100;
const MAX_PAGE_CHARS: usize = 32_000;
const MAX_TOTAL_CHARS: usize = 400_000;
const WIDTH: f32 = 595.0;
const HEIGHT: f32 = 842.0;
const MARGIN: f32 = 44.0;
const SIZE: f32 = 11.0;
const LEADING: f32 = 16.0;
const CONTENT: f32 = WIDTH - 2.0 * MARGIN;

#[derive(Debug, Error)]
pub enum Error {
    #[error(
        "PDFium is unavailable; use the local PDF-enabled app or configure PLAMO_PDFIUM_LIBRARY"
    )]
    Library,
    #[error(
        "a Japanese-capable TrueType font is required; set PLAMO_PDF_FONT or install IPA Gothic"
    )]
    Font,
    #[error("PDF exceeds the local preview size or page limit")]
    Limit,
    #[error("password-protected or encrypted PDF files are unsupported")]
    Encrypted,
    #[error("signed PDFs and interactive PDF forms are unsupported")]
    Interactive,
    #[error("PDF could not be processed: {0}")]
    Pdf(#[source] PdfiumError),
}

impl From<PdfiumError> for Error {
    fn from(error: PdfiumError) -> Self {
        if matches!(
            error,
            PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError)
        ) {
            Self::Encrypted
        } else {
            Self::Pdf(error)
        }
    }
}

pub struct Pdf {
    bytes: Vec<u8>,
    font: Vec<u8>,
    units: Vec<Unit>,
    pages: Vec<Page>,
}

pub struct Unit {
    pub text: String,
    pub page: usize,
}

struct Page {
    rows: Vec<Vec<Cell>>,
    skipped: bool,
    has_text: bool,
}

struct Cell {
    index: usize,
    start: f32,
}

#[derive(Clone, Debug)]
struct Glyph {
    ch: char,
    x: f32,
    right: f32,
    y: f32,
    size: f32,
}

struct Col {
    text: String,
    x: f32,
    right: f32,
}

struct Line {
    cols: Vec<Col>,
    y: f32,
    size: f32,
}

fn resources_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.parent()?.parent()?;
    (path.file_name()? == "Contents").then(|| path.join("Resources"))
}

fn pdfium() -> Result<(MutexGuard<'static, ()>, Pdfium), Error> {
    // 0.8 lacks the library-wide Send/Sync guarantees. Keep initialization,
    // documents and shutdown in one call, and never overlap two instances.
    static PDFIUM: Mutex<()> = Mutex::new(());
    let guard = PDFIUM.lock().map_err(|_| Error::Library)?;
    let bind = || {
        let path = std::env::var_os("PLAMO_PDFIUM_LIBRARY")
            .map(PathBuf::from)
            .or_else(|| {
                resources_dir().map(|dir| {
                    dir.join(if cfg!(target_os = "macos") {
                        "libpdfium.dylib"
                    } else if cfg!(windows) {
                        "pdfium.dll"
                    } else {
                        "libpdfium.so"
                    })
                })
            })?;
        if !path.is_absolute() || !path.is_file() {
            return None;
        }
        Pdfium::bind_to_library(&path).ok().map(Pdfium::new)
    };
    Ok((guard, bind().ok_or(Error::Library)?))
}

fn output_font() -> Result<Vec<u8>, Error> {
    let file = std::env::var_os("PLAMO_PDF_FONT")
        .map(PathBuf::from)
        .or_else(|| {
            resources_dir()
                .map(|dir| dir.join("ipafont/ipag.ttf"))
                .filter(|p| p.is_file())
        })
        .or_else(|| {
            resources_dir()
                .map(|dir| dir.join("pdf-font.ttf"))
                .filter(|p| p.is_file())
        })
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Fonts/ipag.ttf"))
        })
        .ok_or(Error::Font)?;
    let f = std::fs::File::open(file).map_err(|_| Error::Font)?;
    let mut bytes = Vec::new();
    f.take(20 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Font)?;
    let font = ttf_parser::Face::parse(&bytes, 0).map_err(|_| Error::Font)?;
    if font.glyph_index('日').is_none() || font.glyph_index('A').is_none() {
        return Err(Error::Font);
    }
    Ok(bytes)
}

fn pt(x: f32) -> PdfPoints {
    PdfPoints::new(x)
}

impl Pdf {
    pub fn read(path: &Path) -> Result<Self, crate::FileError> {
        let mut bytes = Vec::new();
        let f = std::fs::File::open(path)?;
        if f.metadata()?.len() > MAX_BYTES {
            return Err(Error::Limit.into());
        }
        f.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(Error::Limit.into());
        }
        let (units, pages) = Self::parse(&bytes)?;
        let font = if units.is_empty() {
            Vec::new()
        } else {
            output_font()?
        };
        Ok(Self {
            bytes,
            font,
            units,
            pages,
        })
    }

    fn parse(bytes: &[u8]) -> Result<(Vec<Unit>, Vec<Page>), Error> {
        let (_guard, pdfium) = pdfium()?;
        let doc = pdfium.load_pdf_from_byte_slice(bytes, None)?;
        if !matches!(
            doc.permissions().security_handler_revision(),
            Ok(PdfSecurityHandlerRevision::Unprotected)
        ) {
            return Err(Error::Encrypted);
        }
        if !doc.signatures().is_empty()
            || doc
                .form()
                .is_some_and(|f| f.form_type() != PdfFormType::None)
        {
            return Err(Error::Interactive);
        }
        if doc.pages().len() > MAX_PAGES {
            return Err(Error::Limit);
        }

        let mut units = Vec::new();
        let mut pages = Vec::new();
        let mut count = 0;
        for (i, page) in doc.pages().iter().enumerate() {
            let mut glyphs = Vec::new();
            let mut skipped = !page.annotations().is_empty();
            let text = page.text()?;
            if text.chars().len() > MAX_PAGE_CHARS {
                return Err(Error::Limit);
            }
            for g in text.chars().iter() {
                let Some(mut ch) = g.unicode_char() else {
                    skipped = true;
                    continue;
                };
                if g.is_generated().unwrap_or(false) || ch.is_control() {
                    continue;
                }
                if ch == '\u{f0b7}' {
                    ch = '•';
                }
                if ch == '\u{fffd}'
                    || ('\u{e000}'..='\u{f8ff}').contains(&ch)
                    || g.angle_degrees().map_or(true, |a| a.abs() > 2.0)
                {
                    skipped = true;
                    continue;
                }
                let (Ok((x, y)), Ok(b)) = (g.origin(), g.loose_bounds()) else {
                    skipped = true;
                    continue;
                };
                let size = g.scaled_font_size().value.abs();
                if [x.value, y.value, b.right().value, size]
                    .into_iter()
                    .all(f32::is_finite)
                    && size > 0.0
                {
                    glyphs.push(Glyph {
                        ch,
                        x: x.value,
                        y: y.value,
                        right: b.right().value.max(x.value),
                        size,
                    });
                }
            }
            count += glyphs.len();
            if count > MAX_TOTAL_CHARS {
                return Err(Error::Limit);
            }
            let needs_review = uncertain_geometry(&glyphs, page.width().value);
            let (blocks, estimated) = group_page(glyphs, page.width().value);
            let mut rows = Vec::new();
            for row in blocks {
                let start = if row.len() == 1 {
                    row[0].x
                } else {
                    page.width().value * 0.04
                };
                rows.push(
                    row.into_iter()
                        .map(|col| {
                            let index = units.len();
                            units.push(Unit {
                                text: col.text,
                                page: i + 1,
                            });
                            Cell {
                                index,
                                start: ((col.x - start) / page.width().value).clamp(0.0, 0.88),
                            }
                        })
                        .collect(),
                );
            }
            pages.push(Page {
                has_text: !rows.is_empty(),
                rows,
                skipped: skipped || needs_review || estimated,
            });
        }
        Ok((units, pages))
    }

    pub fn units(&self) -> &[Unit] {
        &self.units
    }

    /// Units whose target will actually be drawn, rather than retained for an
    /// empty result or missing glyph. Indices refer to units(), starting at zero.
    pub fn applied_units(&self, targets: &[Option<String>]) -> Vec<usize> {
        let face = ttf_parser::Face::parse(&self.font, 0).ok();
        self.units
            .iter()
            .zip(targets)
            .enumerate()
            .filter_map(|(i, (unit, target))| {
                (usable(target.as_deref(), face.as_ref())
                    && target
                        .as_ref()
                        .is_some_and(|t| t.trim() != unit.text.trim()))
                .then_some(i)
            })
            .collect()
    }

    /// Preserve a source cell if a recognized yen amount changes in translation.
    pub fn check_amounts(&self, targets: &mut [Option<String>], language: &str) {
        for (unit, target) in self.units.iter().zip(targets) {
            if target
                .as_ref()
                .is_some_and(|t| !crate::office::amounts::pdf_matches(&unit.text, t, language))
            {
                *target = None;
            }
        }
    }

    pub fn review_pages(&self, targets: &[Option<String>]) -> Vec<usize> {
        let mut warnings: Vec<_> = self
            .pages
            .iter()
            .enumerate()
            .filter_map(|(i, page)| (!page.has_text || page.skipped).then_some(i + 1))
            .collect();
        let face = ttf_parser::Face::parse(&self.font, 0).ok();
        warnings.extend(self.units.iter().zip(targets).filter_map(|(unit, target)| {
            (!usable(target.as_deref(), face.as_ref())).then_some(unit.page)
        }));
        warnings.sort_unstable();
        warnings.dedup();
        warnings
    }

    pub fn write(
        &self,
        targets: &[Option<String>],
        target: &str,
    ) -> Result<Vec<u8>, crate::FileError> {
        if targets.len() != self.units.len() || targets.is_empty() {
            return Err(crate::FileError::MismatchedResults);
        }
        self.write_pdf(targets, target).map_err(Into::into)
    }

    fn write_pdf(&self, targets: &[Option<String>], target: &str) -> Result<Vec<u8>, Error> {
        let (_guard, pdfium) = pdfium()?;
        let source = pdfium.load_pdf_from_byte_slice(&self.bytes, None)?;
        let mut output = pdfium.create_new_pdf()?;
        let face = ttf_parser::Face::parse(&self.font, 0).map_err(|_| Error::Font)?;
        let font = output
            .fonts_mut()
            .load_true_type_from_bytes(&self.font, true)?;
        let japanese = target == "ja";
        for (i, page) in self.pages.iter().enumerate() {
            let index = output.pages().len();
            output
                .pages_mut()
                .copy_page_from_document(&source, i as u16, index)?;
            if !page.has_text {
                continue;
            }
            let review = page.skipped
                || page
                    .rows
                    .iter()
                    .flatten()
                    .any(|c| !usable(targets[c.index].as_deref(), Some(&face)));
            // Render translated cells in their source row; wrapped cells keep their own column.
            let mut lines = Vec::new();
            for row in &page.rows {
                let stacked = row
                    .windows(2)
                    .any(|cells| (cells[1].start - cells[0].start) * CONTENT < 34.0);
                let mut drawn: Vec<Vec<(f32, String)>> = Vec::new();
                for (j, cell) in row.iter().enumerate() {
                    let left = if stacked { 0.0 } else { cell.start * CONTENT };
                    let right = row
                        .get(j + 1)
                        .map_or(CONTENT, |next| (next.start * CONTENT).max(left + 1.0));
                    let unit = &self.units[cell.index];
                    let candidate = targets[cell.index].as_deref();
                    let retained = if japanese {
                        "[原本参照]"
                    } else {
                        "[See original]"
                    };
                    let text = if usable(candidate, Some(&face)) {
                        candidate.unwrap_or(retained)
                    } else if usable(Some(&unit.text), Some(&face)) {
                        &unit.text
                    } else {
                        retained
                    };
                    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    let chunks = wrap(
                        &text,
                        &face,
                        if stacked { CONTENT } else { right - left - 8.0 },
                    );
                    let offset = if stacked { drawn.len() } else { 0 };
                    for (i, chunk) in chunks.into_iter().enumerate() {
                        let k = i + offset;
                        if drawn.len() <= k {
                            drawn.push(Vec::new());
                        }
                        drawn[k].push((MARGIN + left, chunk.to_owned()));
                    }
                }
                lines.extend(drawn);
                if row.len() == 1 {
                    lines.push(Vec::new());
                }
            }
            let start_y = HEIGHT - 78.0;
            let per_page = ((start_y - MARGIN) / LEADING) as usize + 1;
            for (sheet, chunk) in lines.chunks(per_page).enumerate() {
                let mut page = output
                    .pages_mut()
                    .create_page_at_end(PdfPagePaperSize::from_points(pt(WIDTH), pt(HEIGHT)))?;
                let title = if japanese {
                    format!("機械翻訳  |  原本 {} ページ  |  {}", i + 1, sheet + 1)
                } else {
                    format!(
                        "Machine translation  |  Original page {}  |  {}",
                        i + 1,
                        sheet + 1
                    )
                };
                page.objects_mut().create_text_object(
                    pt(MARGIN),
                    pt(HEIGHT - MARGIN),
                    &title,
                    font,
                    pt(12.0),
                )?;
                if review {
                    let note = if japanese {
                        "未翻訳の箇所や読み順は原本で確認してください。"
                    } else {
                        "Check the original for retained text and reading order."
                    };
                    page.objects_mut().create_text_object(
                        pt(MARGIN),
                        pt(HEIGHT - 61.0),
                        note,
                        font,
                        pt(9.0),
                    )?;
                }
                for (n, line) in chunk.iter().skip_while(|line| line.is_empty()).enumerate() {
                    for (x, s) in line {
                        page.objects_mut().create_text_object(
                            pt(*x),
                            pt(start_y - n as f32 * LEADING),
                            s,
                            font,
                            pt(SIZE),
                        )?;
                    }
                }
            }
        }
        output.save_to_bytes().map_err(Into::into)
    }
}

fn usable(text: Option<&str>, face: Option<&ttf_parser::Face<'_>>) -> bool {
    text.is_some_and(|s| {
        !s.trim().is_empty()
            && face.is_some_and(|face| {
                s.chars()
                    .all(|c| c.is_whitespace() || face.glyph_index(c).is_some())
            })
    })
}

fn wrap<'a>(mut text: &'a str, face: &ttf_parser::Face<'_>, limit: f32) -> Vec<&'a str> {
    let mut out = Vec::new();
    let scale = SIZE / f32::from(face.units_per_em());
    while !text.is_empty() {
        let mut width = 0.0;
        let mut end = text.len();
        let mut last_space = 0;
        for (i, ch) in text.char_indices() {
            width += face
                .glyph_index(ch)
                .and_then(|g| face.glyph_hor_advance(g))
                .map_or(SIZE, |width| f32::from(width) * scale);
            if width > limit && i > 0 {
                end = if last_space > 0 { last_space } else { i };
                break;
            }
            if ch == ' ' {
                last_space = i;
            }
        }
        out.push(text[..end].trim_end());
        text = text[end..].trim_start();
    }
    out
}

fn latin_word(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, ')' | ']' | '%' | ',' | '.')
}

fn columns(x: &mut [Glyph]) -> Vec<Col> {
    x.sort_by(|a, b| a.x.total_cmp(&b.x));
    let mut cols: Vec<Col> = Vec::new();
    for g in x {
        let col = cols.last_mut();
        if let Some(c) = col
            && (g.x - c.right < (g.size * 1.3).max(12.0) || g.ch.is_whitespace())
        {
            // Deduplicate a glyph accidentally drawn twice (common with faux-bold text).
            if c.text.ends_with(g.ch) && g.right > c.right - 0.5 && g.right < c.right + 0.5 {
                continue;
            }
            if !g.ch.is_whitespace()
                && g.x - c.right > g.size * 0.20
                && c.text.chars().next_back().is_some_and(latin_word)
                && latin_word(g.ch)
            {
                c.text.push(' ');
            }
            c.text.push(g.ch);
            c.right = c.right.max(g.right);
        } else if !g.ch.is_whitespace() {
            cols.push(Col {
                text: g.ch.to_string(),
                x: g.x,
                right: g.right,
            });
        }
    }
    for c in &mut cols {
        c.text.truncate(c.text.trim_end().len());
    }
    cols
}

fn uncertain_geometry(glyphs: &[Glyph], width: f32) -> bool {
    // Very small text, note markers and borderline gutter detection merit a page review.
    glyphs.iter().filter(|g| g.size < 6.5).count() > 5
        || glyphs.iter().any(|g| g.x < -12.0 || g.x > width + 12.0)
}

fn read_lines(mut glyphs: Vec<Glyph>) -> Vec<Line> {
    glyphs.retain(|g| !g.ch.is_control() && !matches!(g.ch, '\u{200b}' | '\u{200e}' | '\u{200f}'));
    glyphs.sort_by(|a, b| b.y.total_cmp(&a.y));
    let mut lines: Vec<Line> = Vec::new();
    let mut from = 0;
    while from < glyphs.len() {
        let y = glyphs[from].y;
        let size = glyphs[from].size;
        let end =
            from + glyphs[from..].partition_point(|g| y - g.y <= (size * 0.26).clamp(1.4, 4.5));
        let cols = columns(&mut glyphs[from..end]);
        if !cols.is_empty() {
            lines.push(Line { cols, y, size });
        }
        from = end;
    }
    lines
}

/// Choose column-major order only for a sustained, clear central gutter. A
/// table's short labels/values should remain associated by row.
fn group_page(glyphs: Vec<Glyph>, width: f32) -> (Vec<Vec<Col>>, bool) {
    let mut lines = read_lines(glyphs);
    let mut omitted = false;
    // Keep a page-number footer out of rewritten prose: the translation header
    // identifies the same source page and the original footer is copied intact.
    lines.retain(|line| {
        if line.y >= 70.0 || line.cols.len() != 1 {
            return true;
        }
        let label = line.cols[0].text.trim();
        !label
            .strip_prefix("Page ")
            .or_else(|| label.strip_prefix("ページ"))
            .is_some_and(|n| !n.trim().is_empty() && n.trim().bytes().all(|b| b.is_ascii_digit()))
    });
    // A raised footnote number can form a separate one-glyph baseline, splitting
    // the preceding sentence mid-amount. The source page retains this marker.
    let remove: Vec<bool> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let [col] = line.cols.as_slice() else {
                return false;
            };
            if col.text.len() > 3 || !col.text.bytes().all(|v| v.is_ascii_digit()) {
                return false;
            }
            lines.get(i + 1).is_some_and(|next| {
                next.size > line.size * 1.24
                    && line.y - next.y < next.size * 0.75
                    && next
                        .cols
                        .iter()
                        .any(|n| n.x <= col.x && n.right + next.size >= col.right)
            })
        })
        .collect();
    let mut ii = 0;
    lines.retain(|_| {
        let keep = !remove[ii];
        omitted |= !keep;
        ii += 1;
        keep
    });
    let candidates = lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let [left, right] = line.cols.as_slice() else {
                return None;
            };
            if left.text.chars().count() < 13
                || right.text.chars().count() < 13
                || !(width * 0.40..width * 0.67).contains(&right.x)
            {
                return None;
            }
            Some((i, right.x))
        })
        .collect::<Vec<_>>();
    if candidates.len() >= 4 {
        let (head, x) = candidates[0];
        let tail = candidates.last().map_or(head, |(i, _)| *i);
        if candidates
            .iter()
            .all(|&(_, start)| (x - start).abs() < width * 0.055)
        {
            let gutter = candidates
                .iter()
                .map(|&(_, x)| x)
                .min_by(f32::total_cmp)
                .unwrap()
                - 5.0;
            let fits = |line: &Line| {
                line.cols
                    .iter()
                    .all(|col| col.right < gutter || col.x >= gutter)
                    && line.cols.len() <= 2
            };
            let mut first = head;
            while first > 0
                && (lines[first - 1].cols.len() == 2 || lines[first - 1].size <= lines[head].size)
                && (lines[first - 1].y - lines[first].y < lines[head].size * 4.0)
                && fits(&lines[first - 1])
            {
                first -= 1;
            }
            let mut last = tail + 1;
            while last < lines.len() && lines[last].y > 50.0 && fits(&lines[last]) {
                last += 1;
            }
            if lines[first..last].iter().all(fits) {
                let mut top = Vec::new();
                let mut left = Vec::new();
                let mut right = Vec::new();
                let mut bottom = Vec::new();
                for (i, line) in lines.into_iter().enumerate() {
                    if i < first {
                        top.push(line);
                    } else if i >= last {
                        bottom.push(line);
                    } else {
                        for mut col in line.cols {
                            let target = if col.x < gutter {
                                &mut left
                            } else {
                                col.x -= gutter;
                                col.right -= gutter;
                                &mut right
                            };
                            target.push(Line {
                                cols: vec![col],
                                y: line.y,
                                size: line.size,
                            });
                        }
                    }
                }
                let usable_right = right
                    .iter()
                    .flat_map(|line| &line.cols)
                    .map(|col| col.right)
                    .max_by(f32::total_cmp)
                    .unwrap_or(width - gutter);
                let groups = fold_lines(top, width)
                    .into_iter()
                    .chain(fold_lines(left, gutter))
                    .chain(fold_lines(right, usable_right))
                    .chain(fold_lines(bottom, width))
                    .collect();
                return (groups, true);
            }
        }
    }
    let estimated = omitted || lines.iter().any(|line| line.cols.len() > 1);
    (fold_lines(lines, width), estimated)
}

fn split_number(left: &str, right: &str) -> bool {
    (left.ends_with([',', '第']) && right.starts_with(|c: char| c.is_ascii_digit()))
        || (left.ends_with(|c: char| c.is_ascii_digit()) && right.starts_with(['万', '億', '兆']))
}

fn join_text(old: &mut Col, next: &Col) {
    let is_cjk = |c: char| matches!(c, 'ぁ'..='ヿ' | '㐀'..='䶿' | '一'..='鿿');
    let split_number = split_number(&old.text, &next.text);
    let cjk = old.text.chars().any(is_cjk) && next.text.chars().any(is_cjk);
    if !cjk && !split_number {
        old.text.push(' ');
    }
    old.text.push_str(&next.text);
    old.right = next.right;
}

// A headline can wrap before its final kana even far from the page edge.
// Do not join arbitrary short labels (e.g. 名前 / 住所) or a new sentence.
fn short_kana_tail(left: &str, right: &str) -> bool {
    left.chars().count() >= 5
        && left.chars().any(|c| matches!(c, 'ぁ'..='ヿ' | '一'..='鿿'))
        && (1..=2).contains(&right.chars().count())
        && right
            .chars()
            .all(|c| matches!(c, 'ぁ'..='ヿ' | '！' | '？'))
}

fn fold_lines(lines: Vec<Line>, width: f32) -> Vec<Vec<Col>> {
    let mut result: Vec<Vec<Col>> = Vec::new();
    let mut past_y = 0.0;
    let mut past_size = 0.0;
    for Line { cols, y, size } in lines {
        let near = past_y - y < size * 1.65 && (past_size - size).abs() < 1.0;
        let mut joined = false;
        if let [next] = cols.as_slice()
            && past_y - y < size * 2.2
            && let Some(previous) = result.last_mut()
        {
            let count = previous.len();
            if let Some(prior) = previous.iter_mut().find(|c| {
                (c.x - next.x).abs() < size
                    || (near
                        && count == 1
                        && short_kana_tail(&c.text, &next.text)
                        && next.x >= c.x
                        && next.right <= c.right)
            }) && prior.text.len() + next.text.len() < 2400
            {
                let no_stop = !prior.text.ends_with(['。', '！', '？', '.', '?', '!', ':']);
                let continuation = if count == 1 {
                    split_number(&prior.text, &next.text)
                        || (near
                            && no_stop
                            && (prior.right > width * 0.71
                                || (next.text.chars().count() <= 8
                                    && prior.text.chars().count() > 30)
                                || short_kana_tail(&prior.text, &next.text)))
                } else {
                    near && no_stop && next.text.chars().count() <= 40 && past_y - y < size * 1.35
                };
                if continuation {
                    join_text(prior, next);
                    joined = true;
                }
            }
        }
        if !joined {
            result.push(cols);
        }
        past_y = y;
        past_size = size;
    }
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn joins_wrapped_kana_headline_without_joining_independent_labels() {
        use super::*;
        let line = |text: &str, y| Line {
            cols: vec![Col {
                text: text.into(),
                x: 100.0,
                right: 350.0,
            }],
            y,
            size: 20.0,
        };
        let out = fold_lines(
            vec![
                line("はじめてみよ", 500.0),
                line("う！", 475.0),
                line("名前", 400.0),
                line("住所", 375.0),
            ],
            780.0,
        );
        assert_eq!(out.len(), 3);
        assert_eq!(out[0][0].text, "はじめてみよう！");
        assert!(short_kana_tail("オープンデータ官民ラウンドテーブ", "ル"));
        let mut tail = line("ル", 76.0);
        tail.cols[0].x = 160.0;
        tail.cols[0].right = 180.0;
        let centered = fold_lines(
            vec![line("オープンデータ官民ラウンドテーブ", 100.0), tail],
            900.0,
        );
        assert_eq!(centered.len(), 1);
        assert_eq!(centered[0][0].text, "オープンデータ官民ラウンドテーブル");
    }

    use super::*;

    fn line(text: &str, x: f32, y: f32) -> Vec<Glyph> {
        text.chars()
            .enumerate()
            .map(|(i, ch)| {
                let x = x + i as f32 * 6.0;
                Glyph {
                    ch,
                    x,
                    right: x + 5.0,
                    y,
                    size: 10.0,
                }
            })
            .collect()
    }

    #[test]
    #[ignore = "requires the local PDFium library and IPA Gothic font"]
    fn exported_pdf_keeps_every_line_and_places_continuations_before_next_source_page() {
        let f = tempfile::NamedTempFile::new().unwrap();
        {
            let (_guard, lib) = pdfium().unwrap();
            let mut src = lib.create_new_pdf().unwrap();
            let helvetica = src.fonts_mut().helvetica();
            let mut page = src
                .pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .unwrap();
            page.objects_mut()
                .create_text_object(
                    pt(42.0),
                    pt(720.0),
                    "Original document page",
                    helvetica,
                    pt(11.0),
                )
                .unwrap();
            drop(page);
            src.pages_mut()
                .create_page_at_end(PdfPagePaperSize::a4())
                .unwrap();
            std::fs::write(f.path(), src.save_to_bytes().unwrap()).unwrap();
        }
        let input = Pdf::read(f.path()).unwrap();
        let text = format!(
            "{} EndMarker",
            "Long translation without clipping. ".repeat(170)
        );
        let targets = vec![Some(text)];
        assert_eq!(input.review_pages(&targets), [2]);
        let bytes = input.write(&targets, "en").unwrap();
        let (_guard, lib) = pdfium().unwrap();
        let output = lib.load_pdf_from_byte_slice(&bytes, None).unwrap();
        let pages: Vec<_> = output
            .pages()
            .iter()
            .map(|page| page.text().unwrap().all())
            .collect();
        assert!(pages.len() >= 4);
        assert!(pages.first().unwrap().contains("Original document page"));
        assert!(pages[pages.len() - 2].contains("EndMarker"));
        assert!(pages.last().unwrap().trim().is_empty());
        assert_eq!(
            pages
                .iter()
                .map(|p| p.matches("clipping").count())
                .sum::<usize>(),
            170
        );
    }

    #[test]
    fn extracts_rows_in_position_order_and_keeps_amounts_with_units() {
        let glyphs = [
            line("12.5 million yen", 360.0, 710.0),
            line("Net sales", 50.0, 730.0),
            line("1兆6,721億円", 360.0, 730.0),
            line("Operating profit", 50.0, 710.0),
        ]
        .concat();
        let (rows, _) = group_page(glyphs, 595.0);
        let texts = rows
            .iter()
            .map(|r| r.iter().map(|c| c.text.as_str()).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            [
                vec!["Net sales", "1兆6,721億円"],
                vec!["Operating profit", "12.5 million yen"]
            ]
        );
    }
}
