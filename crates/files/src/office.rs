//! Local OOXML paragraph translation for PowerPoint and Word.
pub(crate) mod amounts;
mod inheritance;
mod layout;

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Read, Write};
use std::ops::Range;

use quick_xml::escape::escape;
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use thiserror::Error;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const MAX_ARCHIVE: u64 = 128 * 1024 * 1024;
const MAX_TOTAL_UNCOMPRESSED: u64 = 512 * 1024 * 1024;
const MAX_XML: u64 = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;
const DRAWING: &[u8] = b"http://schemas.openxmlformats.org/drawingml/2006/main";
const WORD: &[u8] = b"http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const STRICT_WORD: &[u8] = b"http://purl.oclc.org/ooxml/wordprocessingml/main";
const STRICT_DRAWING: &[u8] = b"http://purl.oclc.org/ooxml/drawingml/main";

#[derive(Debug, Error)]
pub enum Error {
    #[error("unsupported or oversized Office package")]
    Package,
    #[error("unsupported or malformed Office XML")]
    Xml,
    #[error("Office translations do not match the extracted paragraphs or tags")]
    Translation,
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Copy, Debug)]
pub enum TagFormat {
    Anchor,
}

pub const MAX_UNIT_TOKENS: usize = 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    PowerPoint,
    Word,
}

pub struct OfficeDocument {
    format: Format,
    for_translation: bool,
    bytes: Vec<u8>,
    parts: Vec<Part>,
    pub unsupported: Vec<RetainedContent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionReason {
    UnsupportedPart,
    ReadingGuide,
    Range,
    ProtectedStructure,
    InlineStructure,
    TableOfContents,
}

#[derive(Clone, Debug)]
pub struct RetainedContent {
    pub part: String,
    pub paragraph: Option<usize>,
    pub in_table: bool,
    pub count: usize,
    pub source: String,
    pub reason: RetentionReason,
}

impl std::fmt::Display for RetainedContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.part)?;
        if let Some(paragraph) = self.paragraph {
            write!(
                f,
                ": paragraph {paragraph}{}",
                if self.in_table { " (table)" } else { "" }
            )?;
        }
        write!(
            f,
            ": retained {:?} ({}): {}",
            self.reason, self.count, self.source
        )
    }
}

pub struct Part {
    pub name: String,
    archive_index: usize,
    xml: String,
    pub paragraphs: Vec<Paragraph>,
    frames: Vec<layout::Frame>,
}

pub struct Paragraph {
    body: Range<usize>,
    font_size: Option<f64>,
    inherited_font_size: Option<f64>,
    ordered_tags: bool,
    word_location: Option<(usize, bool)>,
    before: String,
    after: String,
    base: Run,
    tags: Vec<Tag>,
    items: Vec<Item>,
}

#[derive(Clone)]
struct Run {
    write_before: String,
    write_after: String,
    key: String,
    props: Option<Range<usize>>,
    hyperlink: bool,
    word: bool,
}

enum Child {
    Run(Run, String),
    Atom(String),
}

enum Tag {
    Style(Run),
    Atom(String),
}

enum Item {
    Text(String, Option<usize>),
    Atom(usize),
}

impl OfficeDocument {
    pub fn read(path: &std::path::Path) -> Result<Self, Error> {
        Self::read_mode(path, false, Self::format_for_path(path))
    }

    pub fn read_for_translation(path: &std::path::Path) -> Result<Self, Error> {
        Self::read_mode(path, true, Self::format_for_path(path))
    }

    fn format_for_path(path: &std::path::Path) -> Format {
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("docx"))
        {
            Format::Word
        } else {
            Format::PowerPoint
        }
    }

    pub fn extension(&self) -> &'static str {
        match self.format {
            Format::PowerPoint => "pptx",
            Format::Word => "docx",
        }
    }

    fn read_mode(
        path: &std::path::Path,
        for_translation: bool,
        format: Format,
    ) -> Result<Self, Error> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(MAX_ARCHIVE + 1)
            .read_to_end(&mut bytes)?;
        Self::open_format(bytes, for_translation, format)
    }

    pub fn open(bytes: Vec<u8>) -> Result<Self, Error> {
        Self::open_mode(bytes, false)
    }

    fn open_mode(bytes: Vec<u8>, for_translation: bool) -> Result<Self, Error> {
        Self::open_format(bytes, for_translation, Format::PowerPoint)
    }

    fn open_format(bytes: Vec<u8>, for_translation: bool, format: Format) -> Result<Self, Error> {
        if bytes.len() as u64 > MAX_ARCHIVE {
            return Err(Error::Package);
        }
        let mut archive = ZipArchive::new(Cursor::new(&bytes))?;
        if archive.len() > MAX_ENTRIES
            || archive.index_for_name("[Content_Types].xml").is_none()
            || archive
                .index_for_name(match format {
                    Format::PowerPoint => "ppt/presentation.xml",
                    Format::Word => "word/document.xml",
                })
                .is_none()
        {
            return Err(Error::Package);
        }
        let mut parts = Vec::new();
        let mut unsupported = Vec::new();
        let mut total = 0u64;
        let mut names = HashSet::new();
        for index in 0..archive.len() {
            let raw = archive.by_index_raw(index)?;
            let name = raw.name()?.into_owned();
            total = total.saturating_add(raw.size());
            if raw.encrypted()
                || raw.is_symlink()
                || raw.enclosed_name().is_none()
                || !names.insert(name.clone())
                || total > MAX_TOTAL_UNCOMPRESSED
            {
                return Err(Error::Package);
            }
            let (editable, report_untouched) = match format {
                Format::PowerPoint => (
                    slide_or_notes(&name),
                    for_translation && unsupported_text_part(&name),
                ),
                Format::Word => (word_body_part(&name), name == "word/comments.xml"),
            };
            if !editable && !report_untouched {
                continue;
            }
            if raw.size() > MAX_XML {
                return Err(Error::Package);
            }
            drop(raw);
            let mut xml = String::new();
            archive
                .by_index(index)?
                .take(MAX_XML + 1)
                .read_to_string(&mut xml)?;
            if xml.len() as u64 > MAX_XML {
                return Err(Error::Package);
            }
            if !editable {
                if has_untranslated_text(&xml)? {
                    unsupported.push(RetainedContent {
                        part: name.clone(),
                        paragraph: None,
                        in_table: false,
                        count: 1,
                        source: String::new(),
                        reason: RetentionReason::UnsupportedPart,
                    });
                }
                continue;
            }
            let (mut paragraphs, skipped) = parse_paragraph_details(&xml, for_translation, format)?;
            let frames = if for_translation && format == Format::PowerPoint {
                layout::frames(&xml)?
            } else {
                Vec::new()
            };
            if for_translation && format == Format::PowerPoint {
                inheritance::resolve_tables(&mut archive, &name, &xml, &frames, &mut paragraphs)?;
            }
            let mut furigana = 0;
            paragraphs.retain(|p| {
                let index = frames.partition_point(|frame| frame.range.end <= p.body.start);
                let retain = frames
                    .get(index)
                    .is_some_and(|frame| frame.contains(p) && frame.likely_furigana(p));
                furigana += usize::from(retain);
                !retain
            });
            if furigana > 0 {
                unsupported.push(RetainedContent {
                    part: name.clone(),
                    paragraph: None,
                    in_table: false,
                    count: furigana,
                    source: String::new(),
                    reason: RetentionReason::ReadingGuide,
                });
            }
            unsupported.extend(
                skipped
                    .iter()
                    .filter(|item| {
                        item.text.chars().any(char::is_alphabetic)
                            || item.reason == RetentionReason::TableOfContents
                    })
                    .map(|item| RetainedContent {
                        part: name.clone(),
                        paragraph: Some(item.index),
                        in_table: item.in_table,
                        count: 1,
                        source: item.text.clone(),
                        reason: item.reason,
                    }),
            );
            parts.push(Part {
                name,
                archive_index: index,
                xml,
                paragraphs,
                frames,
            });
        }
        if parts.is_empty() {
            return Err(Error::Package);
        }
        Ok(Self {
            format,
            for_translation,
            bytes,
            parts,
            unsupported,
        })
    }

    pub fn parts(&self) -> &[Part] {
        &self.parts
    }

    /// The original package is returned byte-for-byte when no paragraph changes.
    pub fn write(
        &self,
        format: TagFormat,
        target_language: &str,
        translations: &[Vec<Option<String>>],
    ) -> Result<Vec<u8>, Error> {
        if translations.len() != self.parts.len() {
            return Err(Error::Translation);
        }
        let mut changed = std::collections::HashMap::new();
        for (part, targets) in self.parts.iter().zip(translations) {
            if targets.len() != part.paragraphs.len() {
                return Err(Error::Translation);
            }
            let mut edits = Vec::new();
            let mut fitted = vec![None; part.paragraphs.len()];
            for (_, indices, fit) in part.fits(format, targets) {
                if fit.applies() {
                    fitted[indices].fill(Some(fit));
                }
            }
            for ((p, target), fit) in part.paragraphs.iter().zip(targets).zip(fitted) {
                if let Some(target) = target {
                    if self.for_translation && !p.amounts_match(format, target, target_language) {
                        return Err(Error::Translation);
                    }
                    let mut rendered = p.rebuild(format, target, target_language)?;
                    if let Some(fit) = fit {
                        rendered =
                            fit.compact_paragraph_with_default(rendered, p.inherited_font_size)?;
                    }
                    edits.push((p.body.clone(), rendered));
                }
            }
            if !edits.is_empty() {
                let mut xml = part.xml.clone();
                edits.sort_unstable_by_key(|(range, _)| range.start);
                for (range, replacement) in edits.into_iter().rev() {
                    xml.replace_range(range, &replacement);
                }
                // Reject malformed XML before copying it into the package.
                if self.format == Format::PowerPoint {
                    paragraphs(&xml)?;
                } else {
                    parse_office_paragraphs(&xml, false, self.format)?;
                }
                changed.insert(part.archive_index, xml);
            }
        }
        if changed.is_empty() {
            return Ok(self.bytes.clone());
        }
        let mut archive = ZipArchive::new(Cursor::new(&self.bytes))?;
        let mut output = ZipWriter::new(Cursor::new(Vec::new()));
        for index in 0..archive.len() {
            let entry = archive.by_index_raw(index)?;
            if let Some(xml) = changed.remove(&index) {
                output.start_file(
                    entry.name()?,
                    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
                )?;
                output.write_all(xml.as_bytes())?;
            } else {
                output.raw_copy_file(entry)?;
            }
        }
        Ok(output.finish()?.into_inner())
    }
}

impl Part {
    pub fn source_context(&self) -> String {
        if !self.name.starts_with("ppt/") {
            return String::new();
        }
        let mut unique = HashSet::new();
        self.paragraphs
            .iter()
            .map(Paragraph::plain_text)
            .filter(|text| unique.insert(text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn fits<'a>(
        &'a self,
        format: TagFormat,
        results: &'a [Option<String>],
    ) -> impl Iterator<Item = (&'a layout::Frame, Range<usize>, layout::Fit)> {
        self.frames.iter().filter_map(move |frame| {
            let start = self
                .paragraphs
                .partition_point(|p| p.body.start < frame.range.start);
            let end = self
                .paragraphs
                .partition_point(|p| p.body.start < frame.range.end);
            let paragraphs = self.paragraphs[start..end]
                .iter()
                .zip(results.iter().skip(start).map(Option::as_deref));
            frame
                .fit(paragraphs, format)
                .map(|fit| (frame, start..end, fit))
        })
    }

    /// Locations requiring a native Word layout check. No geometry is rewritten.
    /// Count outer paragraphs, matching word_location(), not textbox fallback copies.
    pub fn word_layout_risks(&self) -> Vec<usize> {
        if !self.name.starts_with("word/") {
            return Vec::new();
        }
        let mut reader = NsReader::from_str(&self.xml);
        let mut paragraphs = 0;
        let mut depth: usize = 0;
        let mut risks = Vec::new();
        while let Ok((ns, event)) = reader.read_resolved_event() {
            let word = matches!(&ns, ResolveResult::Bound(n) if n.as_ref() == WORD || n.as_ref() == STRICT_WORD);
            match event {
                Event::Start(ref tag) | Event::Empty(ref tag) => {
                    let name = tag.local_name();
                    if word && name.as_ref() == b"p" {
                        if depth == 0 {
                            paragraphs += 1;
                        }
                        if matches!(event, Event::Start(_)) {
                            depth += 1;
                        }
                    }
                    let floating = name.as_ref() == b"anchor"
                        && matches!(&ns, ResolveResult::Bound(n) if n.as_ref().ends_with(b"/wordprocessingDrawing"));
                    let vml = name.as_ref() == b"shape"
                        && matches!(&ns, ResolveResult::Bound(n) if n.as_ref() == b"urn:schemas-microsoft-com:vml")
                        && tag.attributes().flatten().any(|a| {
                            a.key.as_ref() == b"style"
                                && a.value.windows(17).any(|w| w == b"position:absolute")
                        });
                    if depth > 0 && (floating || vml) {
                        risks.push(paragraphs);
                    }
                    if word && name.as_ref() == b"tblpPr" {
                        risks.push(paragraphs + 1);
                    }
                    if word
                        && name.as_ref() == b"trHeight"
                        && tag.attributes().flatten().any(|a| {
                            a.key.local_name().as_ref() == b"hRule" && a.value.as_ref() == b"exact"
                        })
                    {
                        risks.push(paragraphs + 1);
                    }
                }
                Event::End(tag) if word && tag.local_name().as_ref() == b"p" => {
                    depth = depth.saturating_sub(1);
                }
                Event::Eof => break,
                _ => {}
            }
        }
        risks.sort_unstable();
        risks.dedup();
        risks
    }

    pub fn overflow_frames(&self, format: TagFormat, results: &[Option<String>]) -> Vec<usize> {
        self.fits(format, results)
            .filter_map(|(frame, _, fit)| fit.overflow.then_some(frame.index))
            .collect()
    }
}

fn unsupported_text_part(name: &str) -> bool {
    name.ends_with(".xml")
        && [
            "ppt/charts/",
            "ppt/slideLayouts/",
            "ppt/slideMasters/",
            "ppt/notesMasters/",
            "ppt/diagrams/",
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn has_untranslated_text(xml: &str) -> Result<bool, Error> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut in_text = false;
    loop {
        match reader.read_event().map_err(|_| Error::Xml)? {
            Event::Start(node) => in_text = matches!(node.local_name().as_ref(), b"t" | b"v"),
            Event::End(_) => in_text = false,
            Event::Text(text) if in_text => {
                if text
                    .decode()
                    .map_err(|_| Error::Xml)?
                    .chars()
                    .any(char::is_alphabetic)
                {
                    return Ok(true);
                }
            }
            Event::CData(text) if in_text => {
                if text
                    .decode()
                    .map_err(|_| Error::Xml)?
                    .chars()
                    .any(char::is_alphabetic)
                {
                    return Ok(true);
                }
            }
            Event::DocType(_) => return Err(Error::Xml),
            Event::Eof => return Ok(false),
            _ => {}
        }
    }
}

fn slide_or_notes(name: &str) -> bool {
    ["ppt/slides/slide", "ppt/notesSlides/notesSlide"]
        .iter()
        .any(|prefix| {
            name.strip_prefix(prefix)
                .and_then(|tail| tail.strip_suffix(".xml"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
}

fn word_body_part(name: &str) -> bool {
    matches!(
        name,
        "word/document.xml" | "word/footnotes.xml" | "word/endnotes.xml"
    ) || ["word/header", "word/footer"].into_iter().any(|prefix| {
        name.strip_prefix(prefix)
            .and_then(|stem| stem.strip_suffix(".xml"))
            .is_some_and(|index| !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()))
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Paragraph,
    Properties,
    EndProperties,
    Run,
    RunProperties,
    DefaultRunProperties,
    Text,
    Break,
    Field,
    Link,
    WordLink,
    WordRange,
    WordRangeStart,
    WordRangeEnd,
    WordPrefix,
    WordSuffix,
    WordFieldChar,
    WordFieldText,
    WordDrawing,
    Proofing,
    Bullet,
    SpacingAfter,
    LineSpacing,
    Other,
}

#[derive(Clone)]
struct Node {
    kind: NodeKind,
    full: Range<usize>,
    body: Range<usize>,
    children: Vec<Node>,
    text: String,
    in_word_table: bool,
}

fn kind(namespace: ResolveResult<'_>, name: &[u8], format: Format) -> NodeKind {
    if format == Format::Word
        && name == b"AlternateContent"
        && matches!(&namespace, ResolveResult::Bound(ns) if ns.as_ref() == b"http://schemas.openxmlformats.org/markup-compatibility/2006")
    {
        return NodeKind::WordDrawing;
    }
    if format == Format::Word
        && name == b"t"
        && matches!(&namespace, ResolveResult::Bound(ns) if [DRAWING, STRICT_DRAWING].contains(&ns.as_ref()))
    {
        return NodeKind::Text;
    }
    let namespaces = match format {
        Format::PowerPoint => [DRAWING, STRICT_DRAWING],
        Format::Word => [WORD, STRICT_WORD],
    };
    if !matches!(namespace, ResolveResult::Bound(ns) if namespaces.contains(&ns.as_ref())) {
        return NodeKind::Other;
    }
    match name {
        b"drawing" | b"pict" if format == Format::Word => NodeKind::WordDrawing,
        b"p" => NodeKind::Paragraph,
        b"pPr" => NodeKind::Properties,
        b"endParaRPr" => NodeKind::EndProperties,
        b"r" => NodeKind::Run,
        b"rPr" => NodeKind::RunProperties,
        b"spcAft" => NodeKind::SpacingAfter,
        b"lnSpc" => NodeKind::LineSpacing,
        b"defRPr" => NodeKind::DefaultRunProperties,
        b"t" => NodeKind::Text,
        b"br" => NodeKind::Break,
        b"tab" | b"cr" | b"footnoteReference" | b"endnoteReference" | b"footnoteRef"
        | b"endnoteRef" | b"commentReference"
            if format == Format::Word =>
        {
            NodeKind::Break
        }
        b"hyperlink" if format == Format::Word => NodeKind::WordLink,
        b"fldSimple" if format == Format::Word => NodeKind::Field,
        b"fldChar" if format == Format::Word => NodeKind::WordFieldChar,
        b"instrText" if format == Format::Word => NodeKind::WordFieldText,
        b"bookmarkStart" | b"commentRangeStart" if format == Format::Word => {
            NodeKind::WordRangeStart
        }
        b"bookmarkEnd" | b"commentRangeEnd" if format == Format::Word => NodeKind::WordRangeEnd,
        b"proofErr" | b"lastRenderedPageBreak" if format == Format::Word => NodeKind::Proofing,
        b"fld" => NodeKind::Field,
        b"buChar" | b"buAutoNum" | b"buBlip" => NodeKind::Bullet,
        b"hlinkClick" | b"hlinkMouseOver" => NodeKind::Link,
        _ => NodeKind::Other,
    }
}

fn paragraphs(xml: &str) -> Result<(Vec<Paragraph>, usize), Error> {
    parse_paragraphs(xml, false)
}

fn parse_paragraphs(xml: &str, for_translation: bool) -> Result<(Vec<Paragraph>, usize), Error> {
    parse_office_paragraphs(xml, for_translation, Format::PowerPoint)
}

fn parse_office_paragraphs(
    xml: &str,
    for_translation: bool,
    format: Format,
) -> Result<(Vec<Paragraph>, usize), Error> {
    parse_paragraph_details(xml, for_translation, format)
        .map(|(paragraphs, skipped)| (paragraphs, skipped.len()))
}

struct RetainedParagraph {
    index: usize,
    in_table: bool,
    text: String,
    reason: RetentionReason,
}

fn visible_word_node(node: &Node, output: &mut String) {
    if node.kind == NodeKind::Text {
        output.push_str(&node.text);
    }
    for child in &node.children {
        visible_word_node(child, output);
    }
}

fn retained_paragraph(node: &Node, index: usize, reason: RetentionReason) -> RetainedParagraph {
    fn toc(node: &Node) -> bool {
        (node.kind == NodeKind::WordFieldText && node.text.split_whitespace().next() == Some("TOC"))
            || node.children.iter().any(toc)
    }
    let mut text = String::new();
    visible_word_node(node, &mut text);
    text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some((end, _)) = text.char_indices().nth(72) {
        text.truncate(end);
        text.push('…');
    }
    RetainedParagraph {
        index,
        in_table: node.in_word_table,
        text,
        reason: if toc(node) {
            RetentionReason::TableOfContents
        } else {
            reason
        },
    }
}

// Only a block-level rich-text control with ordinary, unbound displayed
// content is transparent to extraction. Keep placeholder/typed/bound controls
// intact: Word may regenerate their display from data or a glossary entry.
fn ordinary_word_control(xml: &str) -> bool {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut depth: usize = 0;
    let mut properties = None;
    let mut saw_properties = false;
    loop {
        let Ok(event) = reader.read_event() else {
            return false;
        };
        let opened = matches!(&event, Event::Start(_));
        match event {
            Event::Start(tag) | Event::Empty(tag) => {
                let name = tag.local_name();
                if depth == 1 && name.as_ref() == b"sdtPr" {
                    properties = Some(depth + 1);
                    saw_properties = true;
                } else if properties == Some(depth)
                    && !matches!(
                        name.as_ref(),
                        b"rPr" | b"id" | b"alias" | b"tag" | b"placeholder"
                    )
                {
                    return false;
                } else if depth == 1 && name.as_ref() == b"sdtContent" {
                    return saw_properties;
                }
                if opened {
                    depth += 1;
                }
            }
            Event::End(_) => {
                if properties == Some(depth) {
                    properties = None;
                }
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return false;
                }
            }
            Event::Eof => return false,
            _ => {}
        }
    }
}

fn parse_paragraph_details(
    xml: &str,
    for_translation: bool,
    format: Format,
) -> Result<(Vec<Paragraph>, Vec<RetainedParagraph>), Error> {
    let join_wrapped = for_translation && format == Format::PowerPoint;
    let mut reader = NsReader::from_str(xml);
    let mut stack: Vec<Node> = Vec::new();
    let mut nodes = Vec::new();
    let mut xml_depth = 0usize;
    let mut word_table_depth = 0usize;
    let mut word_locked_at = None;
    let mut word_field_depth = 0usize;
    let mut word_marks: HashMap<_, usize> = HashMap::new();
    let mut word_intervals = Vec::new();
    loop {
        let start = reader.buffer_position() as usize;
        let (ns, event) = reader.read_resolved_event().map_err(|_| Error::Xml)?;
        let word_element = format == Format::Word
            && matches!(&ns, ResolveResult::Bound(namespace) if [WORD, STRICT_WORD].contains(&namespace.as_ref()));
        let start_kind = match &event {
            Event::Start(e) | Event::Empty(e) => kind(ns, e.local_name().as_ref(), format),
            _ => NodeKind::Other,
        };
        if matches!(&event, Event::Start(_)) {
            xml_depth += 1;
            if word_element
                && let Event::Start(e) = &event
                && e.local_name().as_ref() == b"tbl"
            {
                word_table_depth += 1;
            }
        }
        if word_element && let Event::Start(tag) | Event::Empty(tag) = &event {
            let name = tag.local_name();
            if matches!(
                start_kind,
                NodeKind::WordRangeStart | NodeKind::WordRangeEnd
            ) && let Some(id) = tag
                .attributes()
                .flatten()
                .find(|attr| attr.key.local_name().as_ref() == b"id")
            {
                let key = (
                    name.as_ref().starts_with(b"bookmark"),
                    id.value.into_owned(),
                );
                if start_kind == NodeKind::WordRangeStart {
                    if let Some(previous) = word_marks.insert(key, start) {
                        word_intervals.push((previous..start, false));
                    }
                } else if let Some(previous) = word_marks.remove(&key) {
                    word_intervals.push((previous..start, true));
                }
            }
            match name.as_ref() {
                b"fldChar" => {
                    match tag
                        .attributes()
                        .flatten()
                        .find(|attr| attr.key.local_name().as_ref() == b"fldCharType")
                        .map(|attr| attr.value)
                        .as_deref()
                    {
                        Some(b"begin") => word_field_depth += 1,
                        Some(b"end") => word_field_depth = word_field_depth.saturating_sub(1),
                        _ => {}
                    }
                }
                b"ins" | b"del" | b"moveFrom" | b"moveTo" | b"sdt" | b"txbxContent"
                    if matches!(&event, Event::Start(_))
                        && word_locked_at.is_none()
                        && (name.as_ref() != b"sdt"
                            || !stack.is_empty()
                            || word_field_depth != 0
                            || !ordinary_word_control(&xml[start..])) =>
                {
                    word_locked_at = Some(xml_depth);
                }
                _ => {}
            }
        }
        if let Event::End(e) = &event {
            if word_element && e.local_name().as_ref() == b"tbl" {
                word_table_depth = word_table_depth.saturating_sub(1);
            }
            if word_locked_at == Some(xml_depth) {
                word_locked_at = None;
            }
            xml_depth = xml_depth.saturating_sub(1);
        }
        let end = reader.buffer_position() as usize;
        match event {
            Event::Start(_) if !stack.is_empty() || start_kind == NodeKind::Paragraph => {
                if stack.len() >= 64 {
                    return Err(Error::Xml);
                }
                stack.push(Node {
                    kind: if stack.is_empty() && (word_locked_at.is_some() || word_field_depth > 0)
                    {
                        NodeKind::Other
                    } else {
                        start_kind
                    },
                    full: start..end,
                    body: end..end,
                    children: Vec::new(),
                    text: String::new(),
                    in_word_table: word_table_depth > 0,
                });
            }
            Event::Empty(_) if !stack.is_empty() => {
                stack.last_mut().unwrap().children.push(Node {
                    kind: start_kind,
                    full: start..end,
                    body: end..end,
                    children: Vec::new(),
                    text: String::new(),
                    in_word_table: word_table_depth > 0,
                });
            }
            Event::End(_) if !stack.is_empty() => {
                let mut node = stack.pop().unwrap();
                node.full.end = end;
                node.body.end = start;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else {
                    nodes.push(node);
                }
            }
            Event::Text(event) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&event.decode().map_err(|_| Error::Xml)?);
                }
            }
            Event::CData(event) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&event.decode().map_err(|_| Error::Xml)?);
                }
            }
            Event::GeneralRef(event) if !stack.is_empty() => {
                let entity = event.decode().map_err(|_| Error::Xml)?;
                let escaped = format!("&{entity};");
                let value = quick_xml::escape::unescape(&escaped).map_err(|_| Error::Xml)?;
                stack.last_mut().unwrap().text.push_str(&value);
            }
            Event::DocType(_) => return Err(Error::Xml),
            Event::Eof if stack.is_empty() => break,
            Event::Eof => return Err(Error::Xml),
            _ => {}
        }
    }
    // Bookmarks outside all paragraph text keep the same membership when that
    // text is replaced. Do not extend this to inline ranges or comment anchors.
    let valid_edges: HashSet<_> = word_intervals
        .iter()
        .filter(|(_, balanced)| *balanced)
        .flat_map(|(span, _)| [span.start, span.end])
        .collect();
    let mut edges = HashMap::new();
    for node in &nodes {
        let bookmark = |n: &Node| {
            valid_edges.contains(&n.full.start)
                && matches!(n.kind, NodeKind::WordRangeStart | NodeKind::WordRangeEnd)
                && xml[n.full.clone()]
                    .split_whitespace()
                    .next()
                    .is_some_and(|name| {
                        name.ends_with("bookmarkStart") || name.ends_with("bookmarkEnd")
                    })
        };
        let whitespace_run = |n: &Node| {
            n.kind == NodeKind::Run
                && n.children.iter().all(|c| match c.kind {
                    NodeKind::RunProperties | NodeKind::Proofing | NodeKind::Break => true,
                    NodeKind::Text => c.text.trim().is_empty(),
                    _ => false,
                })
        };
        let prefix: Vec<_> = node
            .children
            .iter()
            .take_while(|n| {
                bookmark(n)
                    || whitespace_run(n)
                    || matches!(n.kind, NodeKind::Properties | NodeKind::Proofing)
            })
            .collect();
        let suffix: Vec<_> = node
            .children
            .iter()
            .rev()
            .take_while(|n| {
                bookmark(n)
                    || whitespace_run(n)
                    || matches!(n.kind, NodeKind::EndProperties | NodeKind::Proofing)
            })
            .collect();
        for (group, kind) in [
            (prefix, NodeKind::WordPrefix),
            (suffix, NodeKind::WordSuffix),
        ] {
            if group.iter().any(|n| bookmark(n)) {
                for n in group {
                    if bookmark(n) || whitespace_run(n) {
                        edges.entry(n.full.start).or_insert(kind);
                    }
                }
            }
        }
    }
    // A local inline range must be handled as a pair by balanced_word_content.
    // Do not detach just its opening marker from the existing tagged path.
    for (span, balanced) in &word_intervals {
        let from = nodes.partition_point(|p| p.full.end <= span.start);
        if *balanced
            && nodes.get(from).is_some_and(|p| p.full.end > span.end)
            && (!edges.contains_key(&span.start) || !edges.contains_key(&span.end))
        {
            edges.remove(&span.start);
            edges.remove(&span.end);
        }
    }

    // A complete paragraph strictly inside a balanced range cannot move outside
    // its endpoints when its body is replaced. Keep endpoint paragraphs intact
    // unless their range can be handled locally by balanced_word_content.
    // Malformed/unclosed ranges remain locked in their entirety.
    word_intervals.extend(
        word_marks
            .into_values()
            .map(|start| (start..xml.len(), false)),
    );
    let mut locks = vec![false; nodes.len()];
    for (span, balanced) in word_intervals {
        let from = nodes.partition_point(|p| p.full.end <= span.start);
        let to = nodes.partition_point(|p| p.full.start <= span.end);
        if from >= to {
            continue;
        }
        if !balanced {
            locks[from..to].fill(true);
        } else if to - from > 1
            || nodes[from].full.start > span.start
            || nodes[to - 1].full.end <= span.end
        {
            // Only paragraphs containing an endpoint are ambiguous. Intervening
            // text keeps the same bookmark/comment membership after translation.
            if nodes[from].full.start <= span.start && !edges.contains_key(&span.start) {
                locks[from] = true;
            }
            if nodes[to - 1].full.end > span.end && !edges.contains_key(&span.end) {
                locks[to - 1] = true;
            }
        }
    }
    for node in &mut nodes {
        for child in &mut node.children {
            if let Some(kind) = edges.get(&child.full.start) {
                child.kind = *kind;
            }
        }
    }

    let mut out = Vec::new();
    let mut skipped = Vec::new();
    let mut nodes = nodes.into_iter().zip(locks).enumerate().peekable();
    while let Some((index, (mut node, locked))) = nodes.next() {
        if locked || node.kind != NodeKind::Paragraph {
            skipped.push(retained_paragraph(
                &node,
                index + 1,
                if locked {
                    RetentionReason::Range
                } else {
                    RetentionReason::ProtectedStructure
                },
            ));
            continue;
        }
        let mut paragraph = Paragraph::from_node(xml, &node, for_translation, format);
        while join_wrapped
            && let Ok(Some(current)) = &paragraph
            && let Some((_, (next, false))) = nodes.peek()
            && let Some(joined) = join_paragraphs(xml, &node, current, next)
            && let Ok(Some(merged)) = Paragraph::from_node(xml, &joined, true, format)
        {
            nodes.next();
            node = joined;
            paragraph = Ok(Some(merged));
        }
        match paragraph {
            Ok(Some(mut p)) => {
                if format == Format::Word {
                    p.word_location = Some((index + 1, node.in_word_table));
                }
                out.push(p);
            }
            Ok(None) => {}
            Err(()) => skipped.push(retained_paragraph(
                &node,
                index + 1,
                RetentionReason::InlineStructure,
            )),
        }
    }
    Ok((out, skipped))
}

fn join_paragraphs(xml: &str, first: &Node, paragraph: &Paragraph, next: &Node) -> Option<Node> {
    // Only siblings in the same text body can be separated by XML whitespace alone.
    if !xml[first.full.end..next.full.start].trim().is_empty()
        || xml[first.full.start..first.body.start] != xml[next.full.start..next.body.start]
    {
        return None;
    }
    let properties = |node: &Node| {
        let props = node
            .children
            .iter()
            .find(|child| child.kind == NodeKind::Properties)?;
        let head = &xml[props.full.start..props.body.start];
        let formatting = props
            .children
            .iter()
            .filter(|child| !matches!(child.kind, NodeKind::SpacingAfter | NodeKind::LineSpacing))
            .map(|child| &xml[child.full.clone()])
            .collect::<Vec<_>>();
        Some((head, formatting))
    };
    if properties(first) != properties(next)
        || first.children.iter().chain(&next.children).any(|child| {
            matches!(child.kind, NodeKind::Break | NodeKind::Field)
                || child
                    .children
                    .iter()
                    .any(|child| child.kind == NodeKind::Bullet)
        })
    {
        return None;
    }

    let left_text = paragraph.plain_text();
    let next_paragraph = Paragraph::from_node(xml, next, true, Format::PowerPoint)
        .ok()
        .flatten()?;
    let right_text = next_paragraph.plain_text();
    let left =
        left_text.trim_end_matches(|c: char| c.is_whitespace() || "」』）】\"'”’".contains(c));
    let right = right_text.trim_start();
    let last = left.chars().next_back()?;
    let initial = right.chars().next()?;
    if "。！？.!?：:；;".contains(last)
        || "・•●○■□◆◇※（(［[".contains(initial)
        || left_text.len() + right_text.len() > 3600
    {
        return None;
    }
    let has_kana = |text: &str| text.chars().any(|c| matches!(c, 'ぁ'..='ヿ'));
    let same_style = paragraph.base.key == next_paragraph.base.key;
    let japanese_continuation = has_kana(left)
        && (has_kana(right) || matches!(initial, '一'..='龯'))
        && ("、をにでとがのはてたもへや".contains(last)
            || left.ends_with("たり")
            || left.ends_with("だから")
            || left.chars().count() >= 16);
    let latin_continuation = last.is_ascii() && initial.is_ascii_lowercase() && same_style;
    if !japanese_continuation && !latin_continuation {
        return None;
    }
    let mut joined = first.clone();
    if last.is_ascii()
        && initial.is_ascii_lowercase()
        && !left_text.ends_with(char::is_whitespace)
        && !right_text.starts_with(char::is_whitespace)
    {
        let text = joined
            .children
            .iter_mut()
            .rev()
            .filter(|child| child.kind == NodeKind::Run)
            .flat_map(|child| child.children.iter_mut())
            .find(|child| child.kind == NodeKind::Text && !child.text.is_empty())?;
        text.text.push(' ');
    }
    if let Some(properties) = joined
        .children
        .iter_mut()
        .find(|child| child.kind == NodeKind::Properties)
        && let Some(next_properties) = next
            .children
            .iter()
            .find(|child| child.kind == NodeKind::Properties)
    {
        // The merged paragraph ends where the last source paragraph ended.
        *properties = next_properties.clone();
    }
    joined.full.end = next.full.end;
    joined.body.end = next.body.end;
    let end = joined
        .children
        .iter()
        .position(|child| child.kind == NodeKind::EndProperties)
        .unwrap_or(joined.children.len());
    joined.children.splice(
        end..end,
        next.children
            .iter()
            .filter(|child| !matches!(child.kind, NodeKind::Properties | NodeKind::EndProperties))
            .cloned(),
    );
    Some(joined)
}

// Give a numeric expression that crosses formatting runs the style of its first digit.
fn keep_numbers_together(mut children: Vec<Child>) -> Vec<Child> {
    let mut output = Vec::with_capacity(children.len());
    let mut runs = Vec::new();
    let flush = |runs: &mut Vec<(Run, String)>, output: &mut Vec<Child>| {
        if runs.len() < 2 {
            output.extend(runs.drain(..).map(|(run, text)| Child::Run(run, text)));
            return;
        }
        let mut joined = String::new();
        let mut ends = Vec::with_capacity(runs.len());
        for (_, text) in runs.iter() {
            joined.push_str(text);
            ends.push(joined.len());
        }
        let mut position = 0;
        let mut emit = |to: usize, one_style: bool| {
            while position < to {
                let index = ends.partition_point(|&end| end <= position);
                let end = if one_style { to } else { to.min(ends[index]) };
                let style = if one_style {
                    let digit = joined[position..end]
                        .find(|c: char| c.is_ascii_digit() || ('０'..='９').contains(&c));
                    digit.map_or(index, |offset| {
                        ends.partition_point(|&boundary| boundary <= position + offset)
                    })
                } else {
                    index
                };
                let run = &runs[style].0;
                let fragment = &joined[position..end];
                if let Some(Child::Run(previous, text)) = output.last_mut()
                    && !previous.hyperlink
                    && previous.key == run.key
                {
                    text.push_str(fragment);
                } else {
                    output.push(Child::Run(run.clone(), fragment.to_owned()));
                }
                position = end;
            }
        };
        for range in amounts::expressions(&joined) {
            let index = ends.partition_point(|&end| end <= range.start);
            if range.end > ends[index] {
                emit(range.start, false);
                emit(range.end, true);
            }
        }
        emit(joined.len(), false);
        runs.clear();
    };
    for child in children.drain(..) {
        match child {
            Child::Run(run, text) if !run.hyperlink => runs.push((run, text)),
            other => {
                flush(&mut runs, &mut output);
                output.push(other);
            }
        }
    }
    flush(&mut runs, &mut output);
    output
}

fn balanced_word_content(xml: &str, contents: &[Node]) -> Result<Vec<Node>, ()> {
    let mut result = Vec::new();
    let mut i = 0;
    let attribute = |node: &Node, key: &[u8]| -> Option<Vec<u8>> {
        let mut reader = quick_xml::Reader::from_str(&xml[node.full.clone()]);
        let (Event::Start(tag) | Event::Empty(tag)) = reader.read_event().ok()? else {
            return None;
        };
        tag.attributes()
            .flatten()
            .find(|a| a.key.local_name().as_ref() == key)
            .map(|a| a.value.into_owned())
    };
    let field_type = |run: &Node| -> Option<Vec<u8>> {
        if run.kind != NodeKind::Run {
            return None;
        }
        let node = run
            .children
            .iter()
            .find(|n| n.kind == NodeKind::WordFieldChar)?;
        attribute(node, b"fldCharType")
    };
    while i < contents.len() {
        let node = &contents[i];
        if node.kind == NodeKind::WordRangeStart {
            let end = if contents
                .get(i + 1)
                .is_some_and(|n| n.kind == NodeKind::WordRangeEnd)
            {
                i + 1
            } else {
                i + 2
            };
            let close = contents
                .get(end)
                .filter(|n| n.kind == NodeKind::WordRangeEnd)
                .ok_or(())?;
            let bookmark = |marker: &Node| {
                xml[marker.full.clone()]
                    .split_whitespace()
                    .next()
                    .is_some_and(|name| {
                        name.ends_with("bookmarkStart") || name.ends_with("bookmarkEnd")
                    })
            };
            if bookmark(node) != bookmark(close)
                || attribute(node, b"id")
                    .zip(attribute(close, b"id"))
                    .is_none_or(|(a, b)| a != b)
                || (end == i + 2 && contents[i + 1].kind != NodeKind::Run)
            {
                return Err(());
            }
            let mut wrapper = node.clone();
            wrapper.full.end = close.full.end;
            wrapper.kind = if end == i + 1 {
                NodeKind::Field
            } else {
                wrapper.children.push(contents[i + 1].clone());
                NodeKind::WordRange
            };
            result.push(wrapper);
            i = end + 1;
        } else if field_type(node).as_deref() == Some(b"begin") {
            let mut depth = 1usize;
            let mut end = i + 1;
            while depth != 0 {
                let run = contents
                    .get(end)
                    .filter(|n| n.kind == NodeKind::Run)
                    .ok_or(())?;
                if run.children.iter().any(|n| {
                    !matches!(
                        n.kind,
                        NodeKind::WordFieldChar
                            | NodeKind::WordFieldText
                            | NodeKind::RunProperties
                            | NodeKind::Text
                    )
                }) {
                    return Err(());
                }
                match field_type(run).as_deref() {
                    Some(b"begin") => depth += 1,
                    Some(b"end") => depth -= 1,
                    _ => {}
                }
                end += 1;
            }
            let mut wrapper = node.clone();
            wrapper.full.end = contents[end - 1].full.end;
            wrapper.kind = NodeKind::Field;
            result.push(wrapper);
            i = end;
        } else {
            result.push(node.clone());
            i += 1;
        }
    }
    Ok(result)
}

fn is_japanese(ch: char) -> bool {
    matches!(ch, 'ぁ'..='ゖ' | 'ァ'..='ヺ' | '㐀'..='䶿' | '一'..='鿿')
}

// Preserve non-text drawings verbatim. Text boxes remain unsupported: translating
// the surrounding text must not silently claim their contents were translated.
fn textless_drawing(node: &Node) -> bool {
    !matches!(node.kind, NodeKind::Text | NodeKind::Paragraph)
        && (node.text.trim().is_empty() || node.text.trim().parse::<f64>().is_ok())
        && node.children.iter().all(textless_drawing)
}

fn split_word_run(xml: &str, run: &Node, result: &mut Vec<Child>) -> Result<(), ()> {
    let props = run
        .children
        .first()
        .filter(|p| p.kind == NodeKind::RunProperties);
    let heading = &xml[run.full.start..run.body.start];
    let closing = &xml[run.body.end..run.full.end];
    let properties = props.map_or("", |p| &xml[p.full.clone()]);
    let key = props
        .map(|_| rewrite_properties(properties, None))
        .transpose()
        .map_err(|_| ())?
        .unwrap_or_default();
    for item in &run.children[usize::from(props.is_some())..] {
        match item.kind {
            NodeKind::Proofing => {}
            NodeKind::WordDrawing if textless_drawing(item) => result.push(Child::Atom(format!(
                "{heading}{properties}{}{closing}",
                &xml[item.full.clone()]
            ))),
            NodeKind::Break => result.push(Child::Atom(format!(
                "{heading}{properties}{}{closing}",
                &xml[item.full.clone()]
            ))),
            NodeKind::Text
                if item.children.is_empty()
                    && item.body.start != item.full.end
                    && !["<g", "</g", "<x", "</x", "<a i=", "</a>"]
                        .iter()
                        .any(|prefix| item.text.contains(prefix)) =>
            {
                let opening = layout::rewrite_tag(
                    &xml[item.full.start..item.body.start],
                    &[(b"xml:space", "preserve")],
                )
                .map_err(|_| ())?;
                result.push(Child::Run(
                    Run {
                        write_before: format!("{heading}{properties}{opening}"),
                        write_after: format!("{}{closing}", &xml[item.body.end..item.full.end]),
                        key: key.clone(),
                        props: None,
                        hyperlink: false,
                        word: true,
                    },
                    item.text.clone(),
                ));
            }
            _ => return Err(()),
        }
    }
    Ok(())
}

impl Paragraph {
    fn from_node(
        xml: &str,
        node: &Node,
        protect_amounts: bool,
        format: Format,
    ) -> Result<Option<Self>, ()> {
        let mut before = String::new();
        let mut after = String::new();
        let mut children = Vec::new();
        let normalized;
        let content = if format == Format::Word {
            normalized = balanced_word_content(xml, &node.children)?;
            normalized.as_slice()
        } else {
            &node.children
        };
        let mut ordered_tags = format == Format::Word
            && content
                .iter()
                .any(|node| matches!(node.kind, NodeKind::Field | NodeKind::WordRange));
        for child in content {
            let full = &xml[child.full.clone()];
            match child.kind {
                NodeKind::Properties | NodeKind::WordPrefix => before.push_str(full),
                NodeKind::Proofing => {}
                NodeKind::EndProperties | NodeKind::WordSuffix => after.push_str(full),
                NodeKind::Break | NodeKind::Field => children.push(Child::Atom(full.to_owned())),
                NodeKind::Run
                    if format == Format::Word
                        && child
                            .children
                            .iter()
                            .filter(|n| n.kind != NodeKind::RunProperties)
                            .count()
                            > 1 =>
                {
                    split_word_run(xml, child, &mut children)?;
                    ordered_tags |= child.children.iter().any(|n| n.kind == NodeKind::Break);
                }
                NodeKind::Run | NodeKind::WordLink | NodeKind::WordRange => {
                    let word = format == Format::Word;
                    let run_node = if matches!(child.kind, NodeKind::WordLink | NodeKind::WordRange)
                    {
                        match child.children.as_slice() {
                            [run] if run.kind == NodeKind::Run => run,
                            _ => return Err(()),
                        }
                    } else {
                        child
                    };
                    if word
                        && child.kind == NodeKind::Run
                        && run_node
                            .children
                            .iter()
                            .any(|n| matches!(n.kind, NodeKind::Break | NodeKind::WordDrawing))
                        && run_node.children.iter().all(|n| {
                            matches!(n.kind, NodeKind::Break | NodeKind::RunProperties)
                                || (n.kind == NodeKind::WordDrawing && textless_drawing(n))
                        })
                    {
                        // A Word run may contain a protected drawing, tab, or reference.
                        children.push(Child::Atom(full.to_owned()));
                        continue;
                    }
                    let Some(text) = run_node.children.iter().find(|c| c.kind == NodeKind::Text)
                    else {
                        return Err(());
                    };
                    if ["<g", "</g", "<x", "</x", "<a i=", "</a>"]
                        .iter()
                        .any(|prefix| text.text.contains(prefix))
                        || !text.children.is_empty()
                        || text.body.start == text.full.end
                        || run_node
                            .children
                            .iter()
                            .any(|c| !matches!(c.kind, NodeKind::Text | NodeKind::RunProperties))
                    {
                        return Err(());
                    }
                    let props = match run_node.children.as_slice() {
                        [only_text] if only_text.kind == NodeKind::Text => None,
                        [props, text]
                            if props.kind == NodeKind::RunProperties
                                && text.kind == NodeKind::Text =>
                        {
                            Some(props)
                        }
                        _ => return Err(()),
                    };
                    let mut write_before = xml[child.full.start..text.full.start].to_owned();
                    let text_open = &xml[text.full.start..text.body.start];
                    if word {
                        write_before.push_str(
                            &layout::rewrite_tag(text_open, &[(b"xml:space", "preserve")])
                                .map_err(|_| ())?,
                        );
                    } else {
                        write_before.push_str(text_open);
                    }
                    let run = Run {
                        write_before,
                        write_after: xml[text.body.end..child.full.end].to_owned(),
                        key: props
                            .map(|p| rewrite_properties(&xml[p.full.clone()], None))
                            .transpose()
                            .map_err(|_| ())?
                            .unwrap_or_default(),
                        props: props.map(|p| {
                            p.full.start - child.full.start..p.full.end - child.full.start
                        }),
                        hyperlink: matches!(child.kind, NodeKind::WordLink | NodeKind::WordRange)
                            || props.is_some_and(|p| {
                                p.children.iter().any(|n| n.kind == NodeKind::Link)
                            }),
                        word,
                    };
                    if let Some(Child::Run(prev, text_before)) = children.last_mut()
                        && !prev.hyperlink
                        && !run.hyperlink
                        && prev.key == run.key
                    {
                        text_before.push_str(&text.text);
                    } else {
                        children.push(Child::Run(run, text.text.clone()));
                    }
                }
                _ => return Err(()),
            }
        }
        if protect_amounts {
            children = keep_numbers_together(children);
        }
        let has_text = children
            .iter()
            .any(|c| matches!(c, Child::Run(_, text) if text.chars().any(char::is_alphabetic)));
        if !has_text {
            return Ok(None);
        }
        let mut weights: Vec<(&Run, usize)> = Vec::new();
        for child in &children {
            if let Child::Run(run, text) = child
                && !run.hyperlink
            {
                let n = text.chars().count();
                if let Some((_, count)) = weights.iter_mut().find(|(style, _)| style.key == run.key)
                {
                    *count += n;
                } else {
                    weights.push((run, n));
                }
            }
        }
        let base = weights
            .into_iter()
            .rev()
            .max_by_key(|(_, weight)| *weight)
            .map(|(run, _)| run.clone());
        let mut tags = Vec::new();
        let mut items = Vec::new();
        for child in children {
            match child {
                Child::Run(_, text) if text.is_empty() => {}
                Child::Run(run, text)
                    if text.trim().is_empty()
                        && (run.hyperlink || base.as_ref().is_none_or(|b| b.key != run.key)) =>
                {
                    // Spaces in a distinct font or width are layout elements.
                    let original =
                        format!("{}{}{}", run.write_before, escape(&text), run.write_after);
                    items.push(Item::Atom(tags.len()));
                    tags.push(Tag::Atom(original));
                }
                Child::Run(run, text)
                    if !run.hyperlink && base.as_ref().is_some_and(|b| b.key == run.key) =>
                {
                    items.push(Item::Text(text, None))
                }
                Child::Run(run, text) => {
                    items.push(Item::Text(text, Some(tags.len())));
                    tags.push(Tag::Style(run));
                }
                Child::Atom(xml) => {
                    items.push(Item::Atom(tags.len()));
                    tags.push(Tag::Atom(xml));
                }
            }
        }
        // For link-only paragraphs, only the tagged link can supply a run template.
        let base = base
            .or_else(|| {
                tags.iter().find_map(|t| match t {
                    Tag::Style(run) => Some(run.clone()),
                    _ => None,
                })
            })
            .ok_or(())?;
        let font_size = node
            .children
            .iter()
            .flat_map(|child| &child.children)
            .filter(|child| {
                matches!(
                    child.kind,
                    NodeKind::RunProperties | NodeKind::DefaultRunProperties
                )
            })
            .filter_map(|child| layout::font_points(&xml[child.full.clone()]))
            .max_by(f64::total_cmp);
        Ok(Some(Self {
            body: node.body.clone(),
            font_size,
            inherited_font_size: None,
            ordered_tags,
            word_location: None,
            before,
            after,
            base,
            tags,
            items,
        }))
    }

    pub fn word_location(&self) -> Option<(usize, bool)> {
        self.word_location
    }

    pub fn word_translation_needs_review(
        &self,
        format: TagFormat,
        translated: &str,
        language: &str,
    ) -> bool {
        if self.word_location.is_none() {
            return false;
        }
        let translated = self.repair_word_atom_close(format, translated);
        let Ok(visible) = self.visible_target(format, &translated) else {
            return true;
        };
        let original = self.plain_text();
        if original.trim() == visible.trim() {
            return original.split_whitespace().count() > 1 || original.chars().any(is_japanese);
        }
        match language {
            "en" => visible.chars().filter(|&ch| is_japanese(ch)).count() >= 3,
            "ja" => {
                let original = original
                    .split(|ch: char| !ch.is_ascii_alphabetic())
                    .filter(|word| word.len() >= 3)
                    .collect::<HashSet<_>>();
                let mut consecutive = 0;
                visible
                    .split(|ch: char| !ch.is_ascii_alphabetic())
                    .filter(|word| !word.is_empty())
                    .any(|word| {
                        if original.contains(word) {
                            consecutive += 1;
                        } else {
                            consecutive = 0;
                        }
                        consecutive >= 2
                    })
            }
            _ => false,
        }
    }

    pub fn has_tags(&self) -> bool {
        !self.tags.is_empty()
    }

    pub fn plain_text(&self) -> String {
        self.items
            .iter()
            .map(|item| match item {
                Item::Text(s, _) => s.as_str(),
                Item::Atom(_) => " ",
            })
            .collect()
    }

    pub fn source(&self, format: TagFormat) -> String {
        let mut text = String::new();
        for item in &self.items {
            match item {
                Item::Text(s, None) => text.push_str(s),
                Item::Text(s, Some(id)) => text.push_str(&format.wrap(id + 1, s, false)),
                Item::Atom(id) => text.push_str(&format.wrap(id + 1, "", true)),
            }
        }
        text
    }

    pub fn valid(&self, format: TagFormat, text: &str) -> bool {
        self.rebuild(format, text, "").is_ok()
    }

    /// Counts a change only after the same repair and validation used for export.
    pub fn changes_text(&self, format: TagFormat, text: &str, language: &str) -> bool {
        let text = self.repair_word_atom_close(format, text);
        self.with_word_spaces(format, &text, language)
            .and_then(|text| self.visible_target(format, &text))
            .is_ok_and(|visible| {
                !visible.trim().is_empty() && visible.trim() != self.plain_text().trim()
            })
    }

    pub fn amounts_match(&self, format: TagFormat, text: &str, language: &str) -> bool {
        let text = self.repair_word_atom_close(format, text);
        self.with_word_spaces(format, &text, language)
            .and_then(|text| self.visible_target(format, &text))
            .is_ok_and(|visible| amounts::matches(&self.plain_text(), &visible, language))
    }

    // The model sometimes closes an ordinary style twice before resuming at a
    // protected Word atom. Remove only the surplus close. The regular parser
    // still requires every original tag and validates each tag's contents.
    fn repair_word_atom_close<'a>(
        &self,
        format: TagFormat,
        text: &'a str,
    ) -> std::borrow::Cow<'a, str> {
        use std::borrow::Cow;
        // An ordinary style may be left open immediately before the next style.
        // Repair only one missing close in a style-only Word paragraph. Links,
        // fields, drawings, unknown/duplicate IDs and any remaining malformed
        // structure are still rejected by the normal rebuild validation.
        if self.base.word
            && matches!(format, TagFormat::Anchor)
            && self
                .tags
                .iter()
                .all(|tag| matches!(tag, Tag::Style(run) if !run.hyperlink))
        {
            let opens: Vec<_> = text.match_indices("<a i=\"").map(|(i, _)| i).collect();
            if opens.len() == self.tags.len() && text.matches("</a>").count() + 1 == opens.len() {
                let missing: Vec<_> = opens
                    .windows(2)
                    .filter(|pair| !text[pair[0]..pair[1]].contains("</a>"))
                    .map(|pair| pair[1])
                    .collect();
                if let [at] = missing.as_slice() {
                    let mut corrected = text.to_owned();
                    corrected.insert_str(*at, "</a>");
                    return Cow::Owned(corrected);
                }
            }
        }
        if !self.base.word
            || !matches!(format, TagFormat::Anchor)
            || !self.tags.iter().any(|tag| matches!(tag, Tag::Atom(_)))
        {
            return Cow::Borrowed(text);
        }

        let mut offset = 0;
        let mut previous_style = None;
        loop {
            let rest = &text[offset..];
            match format.next(rest) {
                Ok(Some(tag)) => {
                    previous_style =
                        matches!(self.tags.get(tag.id), Some(Tag::Style(run)) if !run.hyperlink)
                            .then_some(tag.id);
                    offset += tag.end;
                }
                Ok(None) => break,
                Err(_) => {
                    let Some(id) = previous_style else { break };
                    let Some(close) = rest.find("</a>") else {
                        break;
                    };
                    let Some(after) = rest.get(close + "</a>".len()..) else {
                        break;
                    };
                    if rest[..close].contains(['<', '>']) {
                        break;
                    }
                    let following = after.trim_start();
                    if !matches!(format.next(following), Ok(Some(tag))
                        if tag.start == 0
                            && tag.id == id + 1
                            && tag.body == format.atom_body()
                            && matches!(self.tags.get(tag.id), Some(Tag::Atom(_))))
                    {
                        break;
                    }
                    let mut corrected = text.to_owned();
                    corrected.replace_range(offset + close..offset + close + "</a>".len(), "");
                    return Cow::Owned(corrected);
                }
            }
        }
        Cow::Borrowed(text)
    }

    fn with_word_spaces<'a>(
        &self,
        format: TagFormat,
        text: &'a str,
        language: &str,
    ) -> Result<std::borrow::Cow<'a, str>, Error> {
        use std::borrow::Cow;
        if !self.base.word || language != "en" || self.tags.is_empty() {
            return Ok(Cow::Borrowed(text));
        }

        // A style boundary between contiguous Latin letters may be inside a name or identifier.
        // Source spaces and Japanese text provide evidence for a word boundary in English.
        let edge = |item: &Item, first: bool| match item {
            Item::Text(text, _) if first => text.chars().next(),
            Item::Text(text, _) => text.chars().next_back(),
            Item::Atom(_) => None,
        };
        let can_separate = |left: Option<char>, right: Option<char>| {
            left.zip(right).is_some_and(|(left, right)| {
                [left, right].iter().any(|&c| {
                    c.is_whitespace() || matches!(c, 'ぁ'..='ヿ' | '㐀'..='䶿' | '一'..='鿿')
                })
            })
        };
        let mut boundaries = vec![(false, false); self.tags.len()];
        for (index, item) in self.items.iter().enumerate() {
            if let Item::Text(_, Some(id)) = item {
                boundaries[*id] = (
                    can_separate(
                        index
                            .checked_sub(1)
                            .and_then(|i| edge(&self.items[i], false)),
                        edge(item, true),
                    ),
                    can_separate(
                        edge(item, false),
                        self.items.get(index + 1).and_then(|item| edge(item, true)),
                    ),
                );
            }
        }

        let mut insertions = Vec::new();
        let mut previous = None::<(u8, bool)>;
        let mut observe =
            |fragment: &str, offset, (before, after), previous: &mut Option<(u8, bool)>| {
                if let (Some(&first), Some(&last)) =
                    (fragment.as_bytes().first(), fragment.as_bytes().last())
                {
                    if first.is_ascii_alphabetic()
                        && previous.is_some_and(|(last, boundary)| {
                            last.is_ascii_alphabetic() && (boundary || before)
                        })
                    {
                        insertions.push(offset);
                    }
                    *previous = Some((last, after));
                }
            };
        let mut offset = 0;
        while let Some(tag) = format.next(&text[offset..])? {
            observe(
                &text[offset..offset + tag.start],
                offset,
                (false, false),
                &mut previous,
            );
            match (self.tags.get(tag.id), tag.body) {
                (Some(Tag::Style(_)), Some(body)) => {
                    observe(
                        body,
                        offset + tag.body_start,
                        boundaries[tag.id],
                        &mut previous,
                    );
                }
                _ => previous = None,
            }
            offset += tag.end;
        }
        observe(&text[offset..], offset, (false, false), &mut previous);
        if insertions.is_empty() {
            return Ok(Cow::Borrowed(text));
        }
        let mut spaced = String::with_capacity(text.len() + insertions.len());
        let mut start = 0;
        for insertion in insertions {
            spaced.push_str(&text[start..insertion]);
            spaced.push(' ');
            start = insertion;
        }
        spaced.push_str(&text[start..]);
        Ok(Cow::Owned(spaced))
    }

    fn visible_target(&self, format: TagFormat, mut target: &str) -> Result<String, Error> {
        let mut out = String::new();
        while let Some(tag) = format.next(target)? {
            out.push_str(&target[..tag.start]);
            match self.tags.get(tag.id) {
                Some(Tag::Style(_)) => out.push_str(tag.body.unwrap_or_default()),
                Some(Tag::Atom(xml))
                    if xml.split_once('<').is_some_and(|(_, tag)| {
                        tag.split([':', ' ', '/', '>'])
                            .take(2)
                            .any(|part| part == "br")
                    }) =>
                {
                    out.push('\n')
                }
                _ => out.push(' '),
            }
            target = &target[tag.end..];
        }
        out.push_str(target);
        Ok(out)
    }

    fn rebuild(
        &self,
        format: TagFormat,
        text: &str,
        target_language: &str,
    ) -> Result<String, Error> {
        if text.trim().is_empty() {
            return Err(Error::Translation);
        }
        let repaired = self.repair_word_atom_close(format, text);
        let text = self.with_word_spaces(format, &repaired, target_language)?;
        let mut result = self.before.clone();
        let mut seen = vec![false; self.tags.len()];
        let mut rest = text.as_ref();
        let mut previous = None;
        while let Some(ParsedTag {
            start,
            id,
            body,
            end,
            ..
        }) = format.next(rest)?
        {
            self.base.write(
                &mut result,
                &rest[..start],
                !self.base.hyperlink,
                target_language,
            )?;
            let tag = self.tags.get(id).ok_or(Error::Translation)?;
            if self.ordered_tags && previous.is_some_and(|old| id <= old) {
                return Err(Error::Translation);
            }
            previous = Some(id);
            if std::mem::replace(&mut seen[id], true) {
                return Err(Error::Translation);
            }
            match tag {
                Tag::Style(run) if body.is_some_and(|s| !s.trim().is_empty() && s != "◇") => {
                    run.write(&mut result, body.unwrap(), true, target_language)?;
                }
                Tag::Atom(xml) if body == format.atom_body() => result.push_str(xml),
                _ => return Err(Error::Translation),
            }
            rest = &rest[end..];
        }
        self.base
            .write(&mut result, rest, !self.base.hyperlink, target_language)?;
        if seen.contains(&false) {
            return Err(Error::Translation);
        }
        if !self.after.is_empty() {
            result.push_str(&rewrite_properties(&self.after, Some(target_language))?);
        }
        Ok(result)
    }
}

impl Run {
    fn write(
        &self,
        out: &mut String,
        text: &str,
        allowed: bool,
        target_language: &str,
    ) -> Result<(), Error> {
        if text.is_empty() {
            return Ok(());
        }
        if !allowed
            || text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\t' | '\r' | '\n'))
        {
            return Err(Error::Translation);
        }
        if !self.word
            && let Some(props) = &self.props
        {
            out.push_str(&self.write_before[..props.start]);
            out.push_str(&rewrite_properties(
                &self.write_before[props.clone()],
                Some(target_language),
            )?);
            out.push_str(&self.write_before[props.end..]);
        } else {
            out.push_str(&self.write_before);
        }
        out.push_str(&escape(text));
        out.push_str(&self.write_after);
        Ok(())
    }
}

/// Only the opening element is adjusted. Nested formatting and relationship IDs stay intact.
fn rewrite_properties(xml: &str, target: Option<&str>) -> Result<String, Error> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let event = reader.read_event().map_err(|_| Error::Xml)?;
    let (element, empty) = match event {
        Event::Start(e) => (e.clone(), false),
        Event::Empty(e) => (e.clone(), true),
        _ => return Err(Error::Xml),
    };
    let attributes = element
        .attributes()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| Error::Xml)?;
    let mut rebuilt = element.clone().into_owned();
    rebuilt.clear_attributes();
    for attr in attributes {
        let key = attr.key.as_ref();
        match target {
            None if matches!(key, b"lang" | b"altLang" | b"dirty" | b"err" | b"smtClean") => {
                continue;
            }
            Some(_) if key == b"err" => continue,
            Some(lang) if key == b"lang" && !lang.is_empty() => {
                let lang = match lang {
                    "en" => "en-US",
                    "ja" => "ja-JP",
                    _ => lang,
                };
                rebuilt.push_attribute(("lang", lang));
            }
            _ => rebuilt.push_attribute(attr),
        }
    }
    let fragment = if empty {
        Event::Empty(rebuilt)
    } else {
        Event::Start(rebuilt)
    };
    let mut writer = quick_xml::Writer::new(Vec::new());
    writer.write_event(fragment)?;
    let mut out = String::from_utf8(writer.into_inner()).map_err(|_| Error::Xml)?;
    out.push_str(&xml[reader.buffer_position() as usize..]);
    Ok(out)
}

struct ParsedTag<'a> {
    start: usize,
    id: usize,
    body: Option<&'a str>,
    body_start: usize,
    end: usize,
}

impl TagFormat {
    fn wrap(self, id: usize, text: &str, atom: bool) -> String {
        match self {
            Self::Anchor => format!("<a i=\"{id}\">{}</a>", if atom { "◇" } else { text }),
        }
    }

    fn atom_body(self) -> Option<&'static str> {
        match self {
            Self::Anchor => Some("◇"),
        }
    }

    fn next(self, text: &str) -> Result<Option<ParsedTag<'_>>, Error> {
        let prefixes: &[&str] = match self {
            Self::Anchor => &["<a i=", "</a>"],
        };
        let Some((start, prefix)) = prefixes
            .iter()
            .filter_map(|&p| text.find(p).map(|i| (i, p)))
            .min()
        else {
            return Ok(None);
        };
        let rest = &text[start + prefix.len()..];
        let (id, body, body_start, end) = match prefix {
            "<a i=" => {
                let (raw_id, content) = rest.split_once('>').ok_or(Error::Translation)?;
                let id = raw_id
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .ok_or(Error::Translation)?;
                let close = "</a>";
                let (body, _) = content.split_once(close).ok_or(Error::Translation)?;
                if prefixes.iter().any(|p| body.contains(p)) {
                    return Err(Error::Translation);
                }
                (
                    id,
                    Some(body),
                    text.len() - content.len(),
                    text.len() - content.len() + body.len() + close.len(),
                )
            }
            _ => return Err(Error::Translation),
        };
        let id = id
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .ok_or(Error::Translation)?;
        Ok(Some(ParsedTag {
            start,
            id,
            body,
            body_start,
            end,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_layout_risks_use_outer_paragraph_positions() {
        let xml = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing"><w:body><w:p><w:r><w:drawing><wp:anchor><w:txbxContent><w:p><w:r><w:t>Box</w:t></w:r></w:p></w:txbxContent></wp:anchor></w:drawing></w:r></w:p><w:p><w:r><w:t>Label</w:t></w:r></w:p><w:tbl><w:tblPr><w:tblpPr/></w:tblPr><w:tr><w:trPr><w:trHeight w:hRule="exact"/></w:trPr><w:tc><w:p><w:r><w:t>Cell</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>"#;
        let part = Part {
            name: "word/document.xml".into(),
            archive_index: 0,
            xml: xml.into(),
            paragraphs: Vec::new(),
            frames: Vec::new(),
        };
        assert_eq!(part.word_layout_risks(), vec![1, 3]);
        assert_eq!(part.xml, xml);
    }

    #[test]
    fn reflows_the_tax_deck_sentences_within_their_own_boxes() {
        let root = r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#;
        let paragraph = |text, after| {
            format!(
                r#"<a:p><a:pPr><a:spcAft><a:spcPts val="{after}"/></a:spcAft><a:buNone/></a:pPr><a:r><a:rPr sz="1200"/><a:t>{text}</a:t></a:r></a:p>"#
            )
        };
        let xml = format!(
            "{root}<txBody>{}{}{}{}</txBody><txBody>{}{}</txBody></root>",
            paragraph("お店で商品を買いました。", 0),
            paragraph("代金 1,000円と、", 0),
            paragraph("税金100円を", 400),
            paragraph("支払いました。", 0),
            paragraph("街を清潔に", 0),
            paragraph("保つために 4.5%", 0),
        );
        let (groups, skipped) = parse_paragraphs(&xml, true).unwrap();
        assert_eq!(skipped, 0);
        let texts: Vec<_> = groups.iter().map(Paragraph::plain_text).collect();
        assert_eq!(
            texts,
            [
                "お店で商品を買いました。",
                "代金 1,000円と、税金100円を支払いました。",
                "街を清潔に保つために 4.5%",
            ]
        );

        let mut edited = xml.clone();
        let paid = "I paid 1,000 yen for the item and 100 yen in tax.";
        edited.replace_range(
            groups[1].body.clone(),
            &groups[1].rebuild(TagFormat::Anchor, paid, "en").unwrap(),
        );
        let (written, skipped) = paragraphs(&edited).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(written[1].plain_text(), paid);
        assert_eq!(written.len(), 4);
        assert_eq!(paragraphs(&xml).unwrap().0.len(), 6);
    }

    #[test]
    fn warns_about_overflow_and_preserves_small_reading_guides() {
        let xml = concat!(
            r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
            r#"<sp><spPr><a:xfrm><a:ext cx="762000" cy="190500"/></a:xfrm></spPr>"#,
            r#"<txBody><a:bodyPr wrap="none" tIns="0" bIns="0"/>"#,
            r#"<a:p><a:r><a:rPr sz="700"/><a:t>ぜいむしょ</a:t></a:r></a:p></txBody></sp>"#,
            r#"<sp><spPr><a:xfrm><a:ext cx="762000" cy="254000"/></a:xfrm></spPr>"#,
            r#"<txBody><a:bodyPr wrap="none" tIns="0" bIns="0"/>"#,
            r#"<a:p><a:r><a:rPr sz="1200"/><a:t>消費税</a:t></a:r></a:p></txBody></sp></root>"#,
        );
        let (paragraphs, _) = parse_paragraphs(xml, true).unwrap();
        let frames = layout::frames(xml).unwrap();
        assert!(frames[0].contains(&paragraphs[0]));
        assert!(!frames[0].contains(&paragraphs[1]));
        assert!(frames[0].likely_furigana(&paragraphs[0]));
        assert!(!frames[1].likely_furigana(&paragraphs[1]));
        assert!(
            frames[1]
                .fit(
                    std::iter::once((
                        &paragraphs[1],
                        Some("Consumption tax collected by the store")
                    )),
                    TagFormat::Anchor,
                )
                .is_some()
        );
        assert!(
            frames[1]
                .fit(
                    std::iter::once((&paragraphs[1], Some("Tax"))),
                    TagFormat::Anchor,
                )
                .is_none()
        );
        assert!(has_untranslated_text(r#"<chart><v>消費税</v></chart>"#).unwrap());
        assert!(!has_untranslated_text(r#"<chart><v>4.5%</v></chart>"#).unwrap());
    }

    #[test]
    fn yen_amounts_are_not_split_across_style_tags_and_keep_their_value() {
        let xml = concat!(
            r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:p>"#,
            r#"<a:r><a:rPr sz="900"/><a:t>約</a:t></a:r>"#,
            r#"<a:r><a:rPr sz="2000"/><a:t>1兆6,721</a:t></a:r>"#,
            r#"<a:r><a:rPr sz="900"/><a:t>億円を計上</a:t></a:r></a:p></root>"#,
        );
        let (parts, skipped) = parse_paragraphs(xml, true).unwrap();
        assert_eq!(skipped, 0);
        let p = &parts[0];
        assert!(p.source(TagFormat::Anchor).contains("1兆6,721億円"));
        assert!(p.amounts_match(TagFormat::Anchor, "About 1.6721 trillion yen", "en"));
        assert!(p.amounts_match(TagFormat::Anchor, "About ¥1,672,100,000,000", "en"));
        assert!(!p.amounts_match(TagFormat::Anchor, "About 1.6721 billion yen", "en"));
        assert!(amounts::matches(
            "115兆1,978億円",
            "115 trillion 197.8 billion yen",
            "en"
        ));
        assert!(amounts::matches(
            "約2兆6,002億円",
            "2 trillion and 600.2 billion yen",
            "en"
        ));
        assert!(!p.amounts_match(TagFormat::Anchor, "About 1.6721 trillion", "en"));
        assert!(amounts::matches(
            "about 123 million yen",
            "約１億２，３００万円",
            "ja"
        ));
        assert!(!amounts::matches("123 million yen", "１２３億円", "ja"));
        assert!(amounts::matches(
            "Sales may rise 123 points.",
            "売上は123ポイント伸びるかもしれない。",
            "ja"
        ));
    }

    #[test]
    fn word_preserves_textless_drawings_beside_translated_instructions() {
        let prefix = r#"<root xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006">"#;
        let drawing = r#"<mc:AlternateContent><mc:Choice Requires="wps"><w:drawing><shape/><offset>1050925</offset></w:drawing></mc:Choice><mc:Fallback><w:pict><shape/></w:pict></mc:Fallback></mc:AlternateContent>"#;
        for mixed in [false, true] {
            let middle = if mixed { "" } else { "</w:r><w:r>" };
            let xml = format!(
                "{prefix}<w:p><w:r>{drawing}{middle}<w:t>太枠内に○を記入してください。</w:t></w:r></w:p></root>"
            );
            let (paragraphs, skipped) = parse_office_paragraphs(&xml, true, Format::Word).unwrap();
            assert_eq!(skipped, 0);
            assert_eq!(paragraphs.len(), 1);
            let source = paragraphs[0].source(TagFormat::Anchor);
            let target = source.replace(
                "太枠内に○を記入してください。",
                "Place a circle inside the bold border.",
            );
            let rebuilt = paragraphs[0]
                .rebuild(TagFormat::Anchor, &target, "en")
                .unwrap();
            assert!(rebuilt.contains(drawing));
            assert!(rebuilt.contains("Place a circle"));
            assert!(!rebuilt.contains("太枠"));
        }
        let xml = format!(
            "{prefix}<w:p><w:r><w:drawing><w:txbxContent><w:p><w:r><w:t>図形の文字</w:t></w:r></w:p></w:txbxContent></w:drawing></w:r><w:r><w:t>本文</w:t></w:r></w:p></root>"
        );
        let (paragraphs, skipped) = parse_office_paragraphs(&xml, true, Format::Word).unwrap();
        assert!(paragraphs.is_empty());
        assert_eq!(skipped, 1);
    }

    #[test]
    fn word_repairs_one_missing_close_between_ordinary_styles() {
        let xml = r#"<root xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:p><w:r><w:rPr><w:b/></w:rPr><w:t>それぞれに</w:t></w:r><w:r><w:t>当てはまる項目の太枠内に○を記入</w:t></w:r><w:r><w:rPr><w:i/></w:rPr><w:t>してください。</w:t></w:r></w:p></root>"#;
        let (paragraphs, _) = parse_office_paragraphs(xml, true, Format::Word).unwrap();
        let p = &paragraphs[0];
        let target = r#"<a i="1">Please mark a circle inside the bold box for each applicable item<a i="2">.</a>"#;
        let rebuilt = p.rebuild(TagFormat::Anchor, target, "en").unwrap();
        assert!(rebuilt.contains("Please mark a circle"));
        assert!(!p.valid(TagFormat::Anchor, &target.replace("i=\"2\"", "i=\"1\"")));
        assert!(!p.valid(TagFormat::Anchor, &target.replace("i=\"2\"", "i=\"9\"")));
        assert!(!p.valid(TagFormat::Anchor, r#"<a i="1">Missing second style</a>"#));
        let link = xml
            .replace("<w:r><w:rPr><w:b/>", "<w:hyperlink><w:r><w:rPr><w:b/>")
            .replace(
                "それぞれに</w:t></w:r>",
                "それぞれに</w:t></w:r></w:hyperlink>",
            );
        let (paragraphs, _) = parse_office_paragraphs(&link, true, Format::Word).unwrap();
        assert!(!paragraphs[0].valid(TagFormat::Anchor, target));
    }

    #[test]
    fn word_restores_missing_english_spaces_at_style_boundaries() {
        let xml = concat!(
            r#"<root xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">"#,
            r#"<w:p><w:r><w:rPr><w:b/></w:rPr><w:t>粗利益率は</w:t></w:r>"#,
            r#"<w:r><w:rPr><w:i/></w:rPr><w:t xml:space="default">低下</w:t></w:r>"#,
            r#"<w:r><w:t>し、38.5%となりました。</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>Open </w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>Power</w:t></w:r><w:r><w:t>Point now.</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>価格は</w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t>１億円</w:t></w:r>"#,
            r#"<w:r><w:t>です。</w:t></w:r></w:p></root>"#,
        );
        let (paragraphs, skipped) = parse_office_paragraphs(xml, true, Format::Word).unwrap();
        assert_eq!(skipped, 0);
        let format = TagFormat::Anchor;
        let p = &paragraphs[0];
        let spacing = r#"<a i="1">The gross profit margin</a><a i="2">fell</a>to 38.5%."#;
        let rebuilt = p.rebuild(format, spacing, "en").unwrap();
        assert!(rebuilt.contains(r#"<w:rPr><w:i/></w:rPr><w:t xml:space="preserve"> fell</w:t>"#));
        assert!(rebuilt.contains(r#"<w:t xml:space="preserve"> to 38.5%.</w:t>"#));
        assert!(!rebuilt.contains(r#"xml:space="default""#));
        let natural = r#"<a i="1">The gross profit margin</a> <a i="2">fell</a>, to 38.5%."#;
        assert_eq!(p.with_word_spaces(format, natural, "en").unwrap(), natural);
        assert_eq!(p.with_word_spaces(format, spacing, "fr").unwrap(), spacing);

        let p = &paragraphs[1];
        let brand = r#"Open<a i="1">Power</a>Point now."#;
        assert_eq!(
            p.with_word_spaces(format, brand, "en").unwrap(),
            r#"Open<a i="1"> Power</a>Point now."#
        );
        assert!(paragraphs[2].amounts_match(
            format,
            r#"The price is <a i="1">100 million</a>yen."#,
            "en"
        ));
    }

    #[test]
    fn word_unbound_block_rich_text_keeps_control_properties_and_protects_typed_controls() {
        let wrap = |pr: &str| {
            format!(
                r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:sdt><w:sdtPr>{pr}</w:sdtPr><w:sdtContent><w:p><w:r><w:t>Policy objectives</w:t></w:r></w:p></w:sdtContent></w:sdt></w:body></w:document>"#
            )
        };
        let source = wrap(
            r#"<w:id w:val="42"/><w:placeholder><w:docPart w:val="original"/></w:placeholder>"#,
        );
        let (paragraphs, skipped) = parse_office_paragraphs(&source, true, Format::Word).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(paragraphs.len(), 1);
        let p = &paragraphs[0];
        let mut output = source.clone();
        output.replace_range(
            p.body.clone(),
            &p.rebuild(TagFormat::Anchor, "政策目標", "ja").unwrap(),
        );
        assert!(output.contains(
            r#"<w:id w:val="42"/><w:placeholder><w:docPart w:val="original"/></w:placeholder>"#
        ));
        for restricted in [
            "<w:dataBinding/>",
            "<w:dropDownList/>",
            "<w:showingPlcHdr/>",
            "<w:lock/>",
            "<w:date/>",
        ] {
            let (p, skipped) =
                parse_office_paragraphs(&wrap(restricted), true, Format::Word).unwrap();
            assert!(p.is_empty(), "{restricted}");
            assert_eq!(skipped, 1);
        }
    }

    #[test]
    fn word_preserves_links_and_simple_bookmarks_and_leaves_revisions_intact() {
        let xml = concat!(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body>"#,
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:r><w:t>参照先：</w:t></w:r>"#,
            r#"<w:hyperlink r:id="rId5"><w:r><w:rPr><w:b/></w:rPr><w:t>対応表</w:t></w:r></w:hyperlink></w:p>"#,
            r#"<w:p><w:bookmarkStart w:id="1"/><w:r><w:t>保存</w:t></w:r><w:bookmarkEnd w:id="1"/></w:p>"#,
            r#"<w:p><w:ins w:id="2"><w:r><w:t>修正中</w:t></w:r></w:ins></w:p></w:body></w:document>"#,
        );
        let (paragraphs, skipped) = parse_office_paragraphs(xml, true, Format::Word).unwrap();
        assert_eq!(skipped, 1);
        assert_eq!(paragraphs.len(), 2);
        let p = &paragraphs[0];
        assert_eq!(
            p.source(TagFormat::Anchor),
            r#"参照先：<a i="1">対応表</a>"#
        );
        let rendered = p
            .rebuild(TagFormat::Anchor, r#"See <a i="1">mapping table</a>"#, "en")
            .unwrap();
        assert!(rendered.contains(r#"<w:hyperlink r:id="rId5">"#));
        assert!(rendered.contains(r#"<w:t xml:space="preserve">See </w:t>"#));
        assert!(rendered.contains("<w:rPr><w:b/></w:rPr>"));
        assert!(rendered.contains(r#"<w:pStyle w:val="Heading1"/>"#));
        let bookmark = paragraphs[1]
            .rebuild(TagFormat::Anchor, "Save", "en")
            .unwrap();
        assert!(bookmark.contains(r#"<w:bookmarkStart w:id="1"/><w:r><w:t xml:space="preserve">Save</w:t></w:r><w:bookmarkEnd w:id="1"/>"#));
        assert!(paragraphs[1].valid(TagFormat::Anchor, "Save"));
    }

    #[test]
    fn word_repairs_a_surplus_style_close_before_a_protected_atom() {
        let xml = format!(
            r#"<w:document xmlns:w="{}"><w:body><w:p>
            <w:r><w:t>通常の文</w:t></w:r><w:r><w:rPr><w:u w:val="single"/></w:rPr><w:t>内容</w:t></w:r>
            <w:r><w:br/></w:r><w:r><w:t>役割分担の確認</w:t></w:r>
            </w:p></w:body></w:document>"#,
            std::str::from_utf8(WORD).unwrap()
        );
        let (paragraphs, skipped) = parse_office_paragraphs(&xml, true, Format::Word).unwrap();
        assert_eq!(skipped, 0);
        let paragraph = &paragraphs[0];
        let output = r#"Did you confirm the <a i="1">nature</a>, and </a><a i="2">◇</a> the division of roles?"#;
        let format = TagFormat::Anchor;
        let rebuilt = paragraph.rebuild(format, output, "en").unwrap();
        assert!(rebuilt.contains(">nature</w:t>"));
        assert!(rebuilt.contains("<w:br/>"));
        assert!(rebuilt.contains("division of roles?"));
        assert!(!rebuilt.contains("&lt;/a&gt;"));
        assert!(paragraph.amounts_match(format, output, "en"));

        let missing_break = output.replace(r#"<a i="2">◇</a>"#, "");
        let wrong_break = output.replace("◇", "line");
        let duplicate_break = format!(r#"{output}<a i="2">◇</a>"#);
        assert!(!paragraph.valid(format, &missing_break));
        assert!(!paragraph.valid(format, &wrong_break));
        assert!(!paragraph.valid(format, &duplicate_break));
    }

    #[test]
    fn word_translates_text_sharing_a_run_with_tab_or_saved_page_break() {
        let xml = format!(
            r#"<w:document xmlns:w="{}"><w:body><w:tbl><w:tr><w:tc>
            <w:p><w:r><w:rPr><w:b/></w:rPr><w:lastRenderedPageBreak/><w:tab/><w:t>Business Needs</w:t></w:r></w:p>
            <w:p><w:sdt><w:sdtContent><w:r><w:t>Protected form text</w:t></w:r></w:sdtContent></w:sdt></w:p>
            </w:tc></w:tr></w:tbl></w:body></w:document>"#,
            std::str::from_utf8(WORD).unwrap()
        );
        let (paragraphs, retained) = parse_paragraph_details(&xml, true, Format::Word).unwrap();
        assert_eq!(paragraphs.len(), 1);
        let heading = &paragraphs[0];
        assert_eq!(heading.word_location(), Some((1, true)));
        assert_eq!(
            heading.source(TagFormat::Anchor),
            r#"<a i="1">◇</a>Business Needs"#
        );
        let translated = heading
            .rebuild(TagFormat::Anchor, r#"<a i="1">◇</a>事業上の必要性"#, "ja")
            .unwrap();
        assert!(translated.contains("<w:tab/>"));
        assert!(translated.contains("事業上の必要性"));
        assert!(!translated.contains("lastRenderedPageBreak"));
        assert_eq!(retained[0].text, "Protected form text");
        assert!(retained[0].in_table);
    }

    #[test]
    fn word_translates_inside_balanced_ranges_and_preserves_endpoint_paragraphs() {
        let xml = concat!(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
            r#"<w:p><w:commentRangeStart w:id="4"/><w:r><w:t>始点</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>範囲内</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>終点</w:t></w:r><w:commentRangeEnd w:id="4"/><w:r><w:commentReference w:id="4"/></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>ページ </w:t></w:r><w:r><w:fldChar w:fldCharType="begin"/></w:r>"#,
            r#"<w:r><w:instrText xml:space="preserve"> PAGE </w:instrText></w:r>"#,
            r#"<w:r><w:fldChar w:fldCharType="separate"/></w:r><w:r><w:t>1</w:t></w:r>"#,
            r#"<w:r><w:fldChar w:fldCharType="end"/></w:r><w:r><w:t>です。</w:t></w:r></w:p>"#,
            r#"</w:body></w:document>"#,
        );
        let (p, skipped) = parse_office_paragraphs(xml, true, Format::Word).unwrap();
        assert_eq!(skipped, 2);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].plain_text(), "範囲内");
        assert_eq!(
            p[1].source(TagFormat::Anchor),
            r#"ページ <a i="1">◇</a>です。"#
        );
        let rebuilt = p[1]
            .rebuild(TagFormat::Anchor, r#"Page <a i="1">◇</a>."#, "en")
            .unwrap();
        assert!(rebuilt.contains(r#"<w:instrText xml:space="preserve"> PAGE </w:instrText>"#));
        assert!(rebuilt.contains(r#"<w:fldChar w:fldCharType="end"/>"#));
    }

    #[test]
    fn word_keeps_unclosed_ranges_locked_and_translates_nested_range_interiors() {
        let prefix = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#;
        let inner = concat!(
            r#"<w:p><w:bookmarkStart w:id="11" w:name="outer"/><w:bookmarkStart w:id="12" w:name="inner"/><w:r><w:t>Heading</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>Body inside both bookmarks.</w:t></w:r></w:p>"#,
            r#"<w:p><w:bookmarkEnd w:id="12"/><w:bookmarkEnd w:id="11"/><w:r><w:t>End</w:t></w:r></w:p>"#,
        );
        let xml = format!("{prefix}{inner}</w:body></w:document>");
        let (p, skipped) = parse_office_paragraphs(&xml, true, Format::Word).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(p.len(), 3);
        let mut output = xml.clone();
        output.replace_range(
            p[1].body.clone(),
            &p[1]
                .rebuild(TagFormat::Anchor, "両方の範囲内の本文。", "ja")
                .unwrap(),
        );
        assert_eq!(
            output,
            xml.replace(
                "<w:t>Body inside both bookmarks.",
                r#"<w:t xml:space="preserve">両方の範囲内の本文。"#
            )
        );
        let unclosed = xml.replace(r#"<w:bookmarkEnd w:id="11"/>"#, "");
        let (p, _) = parse_office_paragraphs(&unclosed, true, Format::Word).unwrap();
        assert!(p.is_empty());
    }

    #[test]
    fn word_heading_bookmarks_keep_their_original_edges_and_ids() {
        let xml = concat!(
            r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
            r#"<w:p><w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:bookmarkStart w:id="1" w:name="_Toc1"/><w:bookmarkStart w:id="2" w:name="_Toc2"/><w:r><w:lastRenderedPageBreak/><w:t>C</w:t></w:r><w:r><w:t>olour</w:t></w:r><w:bookmarkEnd w:id="2"/></w:p>"#,
            r#"<w:p><w:bookmarkEnd w:id="1"/><w:r><w:t>Next</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>Before</w:t></w:r><w:bookmarkStart w:id="3"/><w:r><w:t>Inside</w:t></w:r></w:p>"#,
            r#"<w:p><w:r><w:t>Inside</w:t></w:r><w:bookmarkEnd w:id="3"/><w:r><w:t>After</w:t></w:r></w:p>"#,
            r#"</w:body></w:document>"#,
        );
        let (p, skipped) = parse_office_paragraphs(xml, true, Format::Word).unwrap();
        assert_eq!(skipped, 2); // Inline cross-paragraph endpoints stay protected.
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].source(TagFormat::Anchor), "Colour");
        let rebuilt = p[0].rebuild(TagFormat::Anchor, "色", "ja").unwrap();
        assert_eq!(
            rebuilt,
            concat!(
                r#"<w:pPr><w:pStyle w:val="Heading1"/></w:pPr><w:bookmarkStart w:id="1" w:name="_Toc1"/><w:bookmarkStart w:id="2" w:name="_Toc2"/>"#,
                r#"<w:r><w:t xml:space="preserve">色</w:t></w:r><w:bookmarkEnd w:id="2"/>"#,
            )
        );
        assert!(
            p[1].rebuild(TagFormat::Anchor, "次", "ja")
                .unwrap()
                .starts_with(r#"<w:bookmarkEnd w:id="1"/>"#)
        );
        let tail = r#"<w:r><w:t xml:space="preserve"> </w:t></w:r><w:r><w:br/></w:r>"#;
        let with_tail = xml.replace(
            r#"<w:bookmarkEnd w:id="2"/></w:p>"#,
            &format!(r#"<w:bookmarkEnd w:id="2"/>{tail}</w:p>"#),
        );
        let (p, _) = parse_office_paragraphs(&with_tail, true, Format::Word).unwrap();
        assert_eq!(p[0].source(TagFormat::Anchor), "Colour");
        assert!(
            p[0].rebuild(TagFormat::Anchor, "色", "ja")
                .unwrap()
                .ends_with(&format!(r#"<w:bookmarkEnd w:id="2"/>{tail}"#))
        );
    }

    #[test]
    fn reports_overflow_without_changing_shape_or_paragraph_geometry() {
        let xml = concat!(
            r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
            r#"<sp><spPr><a:xfrm><a:ext cx="1524000" cy="889000"/></a:xfrm>"#,
            r#"<a:prstGeom prst="homePlate"/></spPr><txBody><a:bodyPr wrap="none"/>"#,
            r#"<a:p><a:pPr latinLnBrk="1"/><a:r><a:rPr sz="1300"/><a:t>一般公募</a:t></a:r></a:p></txBody></sp>"#,
            r#"<graphicFrame><xfrm><a:off x="0" y="1524000"/><a:ext cx="3048000" cy="252000"/></xfrm>"#,
            r#"<a:tbl><a:tblGrid><a:gridCol w="3048000"/></a:tblGrid><a:tr h="252000"><a:tc><a:txBody>"#,
            r#"<a:bodyPr/><a:p><a:r><a:rPr sz="1100"/><a:t>インフラ、防災・減災、安全・安心分野</a:t></a:r></a:p>"#,
            r#"</a:txBody><a:tcPr marT="54000" marB="36000"/></a:tc></a:tr></a:tbl></graphicFrame></root>"#,
        );
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, contents) in [
            ("[Content_Types].xml", "<Types/>"),
            ("ppt/presentation.xml", "<presentation/>"),
            ("ppt/slides/slide1.xml", xml),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(contents.as_bytes()).unwrap();
        }
        let original = zip.finish().unwrap().into_inner();
        let deck = OfficeDocument::open_mode(original.clone(), true).unwrap();
        assert_eq!(
            deck.write(TagFormat::Anchor, "en", &[vec![None, None]])
                .unwrap(),
            original
        );
        let arrow = "Public Call for Data Disclosure Requests";
        let cell = "Infrastructure, Disaster Prevention and Mitigation, and Safety and Security";
        let targets = vec![Some(arrow.into()), Some(cell.into())];
        assert!(
            !deck.parts()[0]
                .overflow_frames(TagFormat::Anchor, &targets)
                .is_empty()
        );
        let out = deck.write(TagFormat::Anchor, "en", &[targets]).unwrap();
        let parsed = OfficeDocument::open(out).unwrap();
        let slide = &parsed.parts()[0];
        assert_eq!(slide.paragraphs[0].plain_text(), arrow);
        assert_eq!(slide.paragraphs[1].plain_text(), cell);
        assert!(slide.xml.contains(r#"wrap="none""#));
        assert!(slide.xml.contains(r#"latinLnBrk="1""#));
        assert!(slide.xml.contains(r#"marT="54000" marB="36000""#));
        assert!(
            slide.paragraphs[1]
                .font_size
                .is_some_and(|size| (8.0..=11.0).contains(&size))
        );
        assert!(slide.xml.contains(r#"<a:ext cx="1524000" cy="889000"/>"#));
        assert!(slide.xml.contains(r#"<a:tr h="252000">"#));
    }

    #[test]
    fn fits_translated_text_beside_a_retained_paragraph() {
        let xml = concat!(
            r#"<root xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
            r#"<sp><spPr><a:xfrm><a:ext cx="2540000" cy="635000"/></a:xfrm></spPr><txBody><a:bodyPr/>"#,
            r#"<a:p><a:r><a:rPr sz="1400"/><a:t>Keep this label</a:t></a:r></a:p>"#,
            r#"<a:p><a:r><a:rPr sz="1400"/><a:t>短い文章</a:t></a:r></a:p>"#,
            r#"</txBody></sp></root>"#,
        );
        let (p, _) = paragraphs(xml).unwrap();
        let frame = layout::frames(xml).unwrap().remove(0);
        let fit = frame.fit(
            p.iter().zip([None, Some("This translated sentence needs several lines inside the original text box.")]),
            TagFormat::Anchor,
        ).unwrap();
        assert!(fit.applies());
        assert!(!fit.overflow);
        // The source paragraph remains absent from the replacement set; its
        // original 14pt height must still leave room for the translated text.
        let changed = fit
            .compact_paragraph(
                p[1].rebuild(TagFormat::Anchor, "Translated text", "en")
                    .unwrap(),
            )
            .unwrap();
        assert!(!changed.contains(r#"sz="1400""#));
    }

    #[test]
    fn preserves_namespaces_relationships_and_fields_when_rebuilding() {
        let xml = concat!(
            r#"<root xmlns:d="http://schemas.openxmlformats.org/drawingml/2006/main" "#,
            r#"xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">"#,
            r#"<d:p><d:pPr lvl="1"/><d:r><d:t>Read the following </d:t></d:r>"#,
            r#"<d:r><d:rPr><d:hlinkClick r:id="rId5"/></d:rPr><d:t>report</d:t></d:r>"#,
            r#"<d:fld id="field-1" type="slidenum"><d:t>1</d:t></d:fld></d:p></root>"#,
        );
        let (found, skipped) = paragraphs(xml).unwrap();
        assert_eq!(skipped, 0);
        let p = &found[0];
        assert_eq!(
            p.source(TagFormat::Anchor),
            r#"Read the following <a i="1">report</a><a i="2">◇</a>"#
        );
        {
            let format = TagFormat::Anchor;
            let translated = format!(
                "次の{}を確認{}",
                format.wrap(1, "報告書", false),
                format.wrap(2, "", true)
            );
            assert!(!p.valid(
                format,
                &format!("次の{}を確認", format.wrap(1, "報告書", false))
            ));
            assert!(!p.valid(format, &format!("{translated}{}", format.wrap(2, "", true))));
            let replacement = p.rebuild(format, &translated, "ja").unwrap();
            assert!(replacement.contains(r#"<d:hlinkClick r:id="rId5"/>"#));
            assert!(
                replacement.contains(r#"<d:fld id="field-1" type="slidenum"><d:t>1</d:t></d:fld>"#)
            );
            let mut updated = xml.to_owned();
            updated.replace_range(p.body.clone(), &replacement);
            assert_eq!(
                paragraphs(&updated).unwrap().0[0].source(format),
                translated
            );
        }
    }
    #[test]
    fn styled_whitespace_round_trips_as_an_opaque_run() {
        let xml = concat!(
            r#"<a:p xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#,
            r#"<a:r><a:t>Read the following</a:t></a:r>"#,
            r#"<a:r><a:rPr sz="1400"/><a:t>&#160;　</a:t></a:r>"#,
            r#"<a:r><a:t>report</a:t></a:r></a:p>"#,
        );
        let (found, skipped) = paragraphs(xml).unwrap();
        assert_eq!(skipped, 0);
        let paragraph = &found[0];
        assert_eq!(
            paragraph.source(TagFormat::Anchor),
            r#"Read the following<a i="1">◇</a>report"#
        );
        let out = paragraph
            .rebuild(
                TagFormat::Anchor,
                r#"Read the following<a i="1">◇</a>report"#,
                "",
            )
            .unwrap();
        assert!(out.contains("<a:rPr sz=\"1400\"/><a:t>\u{a0}　</a:t>"));
    }
}
