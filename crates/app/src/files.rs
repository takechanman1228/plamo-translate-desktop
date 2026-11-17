//! File UI is a child of `view`, sharing its worker without a second model instance.
use std::sync::Arc;

use ::files::markdown::Markdown;
use ::files::office::{MAX_UNIT_TOKENS, OfficeDocument, TagFormat};
use ::files::pdf::Pdf;
use engine::units::{Unit, UnitRequest, UnitResult, UnitTuning};

use super::*;

const OFFICE_TAGS: TagFormat = TagFormat::Anchor;

#[derive(Clone)]
enum FileDocument {
    Text(Arc<::files::Document>),
    Markdown(Arc<Markdown>),
    Pdf(Arc<Pdf>),
    Office {
        deck: Arc<OfficeDocument>,
        japanese: bool,
        units: usize,
    },
}

impl FileDocument {
    fn read(path: &Path) -> Result<Self, ::files::FileError> {
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
        {
            return Ok(Self::Pdf(Arc::new(Pdf::read(path)?)));
        }
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        {
            return Ok(Self::Markdown(Arc::new(Markdown::read(path)?)));
        }
        if path.extension().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("pptx") || extension.eq_ignore_ascii_case("docx")
        }) {
            let deck = Arc::new(OfficeDocument::read_for_translation(path)?);
            let paragraphs = || deck.parts().iter().flat_map(|part| &part.paragraphs);
            let sample = paragraphs()
                .map(|p| p.plain_text())
                .scan(0, |size, text| {
                    if *size >= 16_384 {
                        return None;
                    }
                    *size += text.len();
                    Some(text)
                })
                .collect::<Vec<_>>()
                .join("\n");
            let japanese = lang::looks_japanese(&sample);
            let units = paragraphs().count();
            Ok(Self::Office {
                deck,
                japanese,
                units,
            })
        } else {
            Ok(Self::Text(Arc::new(::files::read_document(path)?)))
        }
    }

    fn japanese(&self) -> bool {
        match self {
            Self::Text(doc) => lang::looks_japanese(doc.source()),
            Self::Pdf(doc) => {
                let sample = doc
                    .units()
                    .iter()
                    .take(100)
                    .map(|s| &s.text[..s.text.floor_char_boundary(512)])
                    .collect::<Vec<_>>()
                    .join("\n");
                lang::looks_japanese(&sample)
            }
            Self::Markdown(doc) => {
                let sample = doc
                    .segments()
                    .iter()
                    .take(100)
                    .map(|s| &s.text[..s.text.floor_char_boundary(512)])
                    .collect::<Vec<_>>()
                    .join("\n");
                lang::looks_japanese(&sample)
            }
            Self::Office { japanese, .. } => *japanese,
        }
    }

    fn empty(&self) -> bool {
        match self {
            Self::Text(doc) => doc.source().trim().is_empty(),
            Self::Markdown(doc) => doc.segments().is_empty(),
            Self::Pdf(doc) => doc.units().is_empty(),
            Self::Office { units, .. } => *units == 0,
        }
    }
}

#[derive(Clone)]
enum FileOutput {
    Text(String),
    Markdown(Arc<Vec<u8>>),
    Pdf(Arc<Vec<Option<String>>>),
    Office(Vec<Vec<Option<String>>>),
}

struct CompletedFile {
    output: FileOutput,
    warnings: Vec<String>,
    applied: Vec<String>,
    has_changes: bool,
}

struct UnitTranslation {
    results: Vec<Option<UnitResult>>,
    completed: usize,
}

impl UnitTranslation {
    fn new(count: usize) -> Self {
        Self {
            results: vec![None; count],
            completed: 0,
        }
    }
    fn text_targets(self) -> Option<Vec<Option<String>>> {
        (self.completed == self.results.len()).then(|| {
            self.results
                .into_iter()
                .map(|result| match result {
                    Some(UnitResult::Complete { text, .. }) => Some(text),
                    _ => None,
                })
                .collect()
        })
    }
}

fn retained_message(item: &::files::office::RetainedContent) -> String {
    use ::files::office::RetentionReason::*;
    let reason = match item.reason {
        UnsupportedPart => t!("files.retained_part"),
        ReadingGuide => t!("files.retained_reading_guide", count = item.count),
        Range => t!("files.retained_range"),
        ProtectedStructure => t!("files.retained_structure"),
        InlineStructure => t!("files.retained_inline"),
        TableOfContents => t!("files.retained_toc"),
    };
    let location = match item.part.as_str() {
        "word/document.xml" => t!("files.body").into_owned(),
        "word/comments.xml" => t!("files.comments").into_owned(),
        part => part.to_owned(),
    };
    let location = if let Some(paragraph) = item.paragraph {
        t!(
            "files.paragraph_location",
            part = location,
            paragraph = paragraph
        )
        .into_owned()
    } else {
        location
    };
    let location = if item.in_table {
        format!("{location} ({})", t!("files.office_table"))
    } else {
        location
    };
    t!(
        "files.retained_detail",
        location = location,
        reason = reason,
        source = item.source
    )
    .into_owned()
}

fn finish_office(
    deck: &OfficeDocument,
    job: UnitTranslation,
    target: &str,
) -> Option<CompletedFile> {
    if job.completed != job.results.len() {
        return None;
    }
    let mut warnings = deck
        .unsupported
        .iter()
        .map(retained_message)
        .collect::<Vec<_>>();
    let mut has_changes = false;
    let mut applied = Vec::new();
    let mut results = job.results.into_iter();
    let mut parts = Vec::new();
    for part in deck.parts() {
        let targets = part
            .paragraphs
            .iter()
            .enumerate()
            .map(|(index, paragraph)| {
                let (position, in_table) = paragraph.word_location().unwrap_or((index + 1, false));
                let location = if in_table {
                    format!("{position} ({})", t!("files.office_table"))
                } else {
                    position.to_string()
                };
                let outcome = results.next().flatten()?;
                match outcome {
                    UnitResult::Complete { text, .. }
                        if paragraph.amounts_match(OFFICE_TAGS, &text, target) =>
                    {
                        let changed = paragraph.changes_text(OFFICE_TAGS, &text, target);
                        has_changes |= changed;
                        if changed {
                            applied.push(
                                t!("files.applied_office", part = part.name, unit = &location)
                                    .into_owned(),
                            );
                        }
                        if paragraph.word_translation_needs_review(OFFICE_TAGS, &text, target) {
                            warnings.push(
                                t!(
                                    "files.word_language_review",
                                    part = part.name,
                                    unit = location
                                )
                                .into_owned(),
                            );
                        }
                        Some(text)
                    }
                    reason => {
                        let reason = match reason {
                            UnitResult::Cut => t!("files.cut"),
                            UnitResult::TooLong => t!("files.unit_too_long"),
                            UnitResult::Complete { .. } => t!("files.office_unit_amount"),
                            _ => t!("files.unit_invalid"),
                        };
                        warnings.push(
                            t!(
                                "files.office_retained",
                                part = part.name,
                                unit = location,
                                reason = reason
                            )
                            .into_owned(),
                        );
                        None
                    }
                }
            })
            .collect::<Vec<_>>();
        let layout = part.word_layout_risks();
        if !layout.is_empty() {
            warnings.push(
                t!(
                    "files.word_layout_risks",
                    part = part.name,
                    units = layout
                        .iter()
                        .map(usize::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into_owned(),
            );
        }
        let overflow = part.overflow_frames(OFFICE_TAGS, &targets);
        if !overflow.is_empty() {
            warnings.push(
                t!(
                    "files.pptx_overflow",
                    part = part.name,
                    count = overflow.len(),
                    frames = overflow
                        .iter()
                        .map(usize::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into_owned(),
            );
        }
        parts.push(targets);
    }
    Some(CompletedFile {
        output: FileOutput::Office(parts),
        applied,
        warnings,
        has_changes,
    })
}

#[derive(Default)]
pub(super) struct FilePanel {
    loaded: Option<(PathBuf, FileDocument)>,
    output: ::files::Translation,
    unit_translation: Option<UnitTranslation>,
    completed: Option<CompletedFile>,
    progress: Option<(usize, usize)>,
    io_busy: bool,
    cancelling: bool,
    target: String,
    pub(super) message: Option<String>,
    saved: Option<PathBuf>,
    shown_warnings: usize,
    shown_applied: usize,
}

impl FilePanel {
    fn start_plain_units(
        &mut self,
        worker: &engine::worker::Worker,
        options: engine::pfml::TranslationOptions,
        texts: Vec<String>,
    ) -> u64 {
        self.unit_translation = Some(UnitTranslation::new(texts.len()));
        worker.translate_units(
            options,
            UnitRequest {
                units: texts
                    .into_iter()
                    .map(|text| Unit {
                        text,
                        indivisible: false,
                    })
                    .collect(),
                tuning: UnitTuning {
                    reuse_translations: true,
                    ..Default::default()
                },
                max_unit_tokens: MAX_UNIT_TOKENS,
                max_history_tokens: 0,

                validate: Arc::new(|_, text| !text.trim().is_empty()),
            },
        )
    }
}

impl TranslatorView {
    pub(super) fn file_operation_busy(&self) -> bool {
        self.jobs.busy() || self.files.io_busy
    }
    pub(super) fn file_source_is_japanese(&self) -> Option<bool> {
        self.files.loaded.as_ref().map(|(_, doc)| doc.japanese())
    }

    pub(crate) fn choose_file(&mut self, cx: &mut Context<Self>) {
        if self.jobs.busy() || self.files.io_busy {
            return;
        }
        self.file_tab = true;
        self.files.io_busy = true;
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t!("files.choose").into_owned().into()),
        });
        cx.spawn(async move |this, cx| {
            let result = paths.await;
            let _ = this.update(cx, |this, cx| {
                this.files.io_busy = false;
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            this.read_file(path, cx);
                        }
                    }
                    Ok(Err(_)) => {
                        this.files.message = Some(t!("files.picker_failed").into_owned());
                        cx.notify();
                    }
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn read_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.jobs.busy() || self.files.io_busy {
            return;
        }
        self.file_tab = true;
        self.files.io_busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let result = FileDocument::read(&path);
                    (path, result)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.files.io_busy = false;
                match result {
                    (path, Ok(document)) => {
                        let message = if document.empty() {
                            let message = if matches!(document, FileDocument::Pdf(_)) {
                                "files.pdf_empty"
                            } else {
                                "files.empty"
                            };
                            Some(t!(message).into_owned())
                        } else {
                            None
                        };
                        this.files = FilePanel {
                            loaded: Some((path, document)),
                            message,
                            ..Default::default()
                        };
                    }
                    (_, Err(error)) => {
                        this.files.message =
                            Some(t!("files.error", error = error.to_string()).into_owned())
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn start_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.jobs.busy() || self.files.io_busy {
            return;
        }
        if !self.model_ready {
            self.open_models(window, cx);
            return;
        }
        let Some((_, document)) = &self.files.loaded else {
            return;
        };
        if document.empty() {
            return;
        }
        let text = match document {
            FileDocument::Text(doc) => doc.source().to_owned(),
            FileDocument::Office { .. } | FileDocument::Markdown(_) | FileDocument::Pdf(_) => {
                String::new()
            }
        };
        let request = self.build_request(text, document.japanese());
        let options = request
            .document
            .options(appdata::cut_marker(&rust_i18n::locale()).to_owned());
        self.debounce = Task::ready(());
        self.shown_partial |= self.current_job.is_some();
        self.current_job = None;
        self.status = Status::Ready;
        self.files.output = ::files::Translation::default();
        self.files.completed = None;
        self.files.shown_warnings = 0;
        self.files.shown_applied = 0;
        self.files.progress = None;
        self.files.cancelling = false;
        self.files.message = None;
        self.files.saved = None;
        self.files.target = request.options.target.as_str().to_owned();
        let job = match document {
            FileDocument::Text(_) => self
                .worker
                .translate(request.options, options, request.text),
            FileDocument::Markdown(doc) => {
                let units = doc
                    .segments()
                    .iter()
                    .map(|s| Unit {
                        text: s.text.clone(),
                        indivisible: s.has_anchors(),
                    })
                    .collect::<Vec<_>>();
                self.files.unit_translation = Some(UnitTranslation::new(units.len()));
                let validator = Arc::clone(doc);
                self.worker.translate_units(
                    request.options,
                    UnitRequest {
                        units,
                        tuning: UnitTuning::default(),
                        max_unit_tokens: MAX_UNIT_TOKENS,
                        max_history_tokens: ::files::markdown::HISTORY_TOKENS,

                        validate: Arc::new(move |i, text| validator.segments()[i].valid(text)),
                    },
                )
            }
            FileDocument::Pdf(doc) => self.files.start_plain_units(
                &self.worker,
                request.options,
                doc.units().iter().map(|unit| unit.text.clone()).collect(),
            ),
            FileDocument::Office { deck, units, .. } => {
                let mut locations = Vec::with_capacity(*units);
                let mut request_units = Vec::with_capacity(*units);
                for (part_index, part) in deck.parts().iter().enumerate() {
                    for (index, paragraph) in part.paragraphs.iter().enumerate() {
                        locations.push((part_index, index));
                        request_units.push(Unit {
                            text: paragraph.source(OFFICE_TAGS),
                            indivisible: paragraph.has_tags(),
                        });
                    }
                }
                let tuning = UnitTuning {
                    contexts: deck
                        .parts()
                        .iter()
                        .map(|part| part.source_context())
                        .collect(),
                    context_by_unit: locations.iter().map(|&(part, _)| part).collect(),
                    reuse_translations: true,
                    ..Default::default()
                };
                let validator = Arc::clone(deck);
                self.files.unit_translation = Some(UnitTranslation::new(*units));
                self.worker.translate_units(
                    request.options,
                    UnitRequest {
                        units: request_units,
                        tuning,
                        max_unit_tokens: MAX_UNIT_TOKENS,
                        max_history_tokens: 0,

                        validate: Arc::new(move |index, translated| {
                            let (part, paragraph) = locations[index];
                            validator.parts()[part].paragraphs[paragraph]
                                .valid(OFFICE_TAGS, translated)
                        }),
                    },
                )
            }
        };
        self.jobs.start_file(job);
        cx.notify();
    }

    pub(super) fn cancel_file(&mut self, cx: &mut Context<Self>) {
        if self.jobs.busy() && !self.files.cancelling {
            self.files.cancelling = true;
            self.worker.cancel();
            cx.notify();
        }
    }

    pub(super) fn file_token(&mut self, job: u64, text: &str) {
        if self.jobs.file() == Some(job) && !self.files.cancelling {
            self.files.output.append(text);
        }
    }

    /// Return true only for events owned by the file job. The existing text handler still
    /// receives all model/runtime events.
    pub(super) fn file_event(
        &mut self,
        event: &Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(active) = self.jobs.file() else {
            return false;
        };
        let resume = match event {
            Event::Chunk {
                job, index, count, ..
            } if *job == active => {
                self.files.progress = Some((index + 1, *count));
                cx.notify();
                return true;
            }
            Event::Rewind { job, len } if *job == active => {
                self.files.output.rewind(*len);
                return true;
            }
            Event::Cut {
                job,
                source,
                output,
                ..
            } if *job == active => {
                self.files
                    .output
                    .cut(source.0..source.1, output.0..output.1);
                return true;
            }
            Event::Unit { job, index, result } if *job == active => {
                if let Some(batch) = &mut self.files.unit_translation
                    && let Some(slot) = batch.results.get_mut(*index)
                    && slot.is_none()
                {
                    *slot = Some(result.clone());
                    batch.completed += 1;
                    self.files.progress = Some((batch.completed, batch.results.len()));
                }
                cx.notify();
                return true;
            }
            Event::Finished { job, stats } if *job == active => {
                let output = std::mem::take(&mut self.files.output);
                let batch = self.files.unit_translation.take();
                if self.files.cancelling || stats.stop_reason == StopReason::Cancelled {
                    self.files.message = Some(t!("files.cancelled").into_owned());
                } else if stats.stop_reason == StopReason::Finished {
                    self.files.completed = match (&self.files.loaded, batch) {
                        (Some((_, FileDocument::Text(doc))), None) => output
                            .finish(doc.source())
                            .ok()
                            .filter(|(_, warnings)| warnings.len() == stats.cut)
                            .map(|(text, warnings)| CompletedFile {
                                applied: if text.trim() != doc.source().trim() {
                                    vec![
                                        t!(
                                            "files.applied_text",
                                            lines = doc.source().lines().count()
                                        )
                                        .into_owned(),
                                    ]
                                } else {
                                    Vec::new()
                                },
                                has_changes: !text.trim().is_empty()
                                    && text.trim() != doc.source().trim(),
                                output: FileOutput::Text(text),
                                warnings: warnings
                                    .into_iter()
                                    .map(|line| {
                                        t!("files.warning", line = line, reason = t!("files.cut"))
                                            .into_owned()
                                    })
                                    .collect(),
                            }),
                        (Some((_, FileDocument::Markdown(doc))), Some(batch)) => batch
                            .text_targets()
                            .and_then(|targets| match doc.write(&targets) {
                                Ok(export) => Some(export),
                                Err(::files::FileError::NoTranslation) => {
                                    self.files.message = Some(t!("files.no_changes").into_owned());
                                    None
                                }
                                Err(_) => None,
                            })
                            .map(|export| CompletedFile {
                                applied: doc
                                    .segments()
                                    .iter()
                                    .filter(|segment| {
                                        !export.retained_lines.contains(&segment.line)
                                    })
                                    .map(|segment| {
                                        t!("files.applied_line", line = segment.line).into_owned()
                                    })
                                    .collect(),
                                has_changes: export.translated > 0,
                                output: FileOutput::Markdown(Arc::new(export.bytes)),
                                warnings: export
                                    .retained_lines
                                    .into_iter()
                                    .map(|line| {
                                        t!(
                                            "files.warning",
                                            line = line,
                                            reason = t!("files.unit_invalid")
                                        )
                                        .into_owned()
                                    })
                                    .collect(),
                            }),
                        (Some((_, FileDocument::Pdf(doc))), Some(batch)) => {
                            batch.text_targets().map(|mut targets| {
                                doc.check_amounts(&mut targets, &self.files.target);
                                let warnings = doc
                                    .review_pages(&targets)
                                    .into_iter()
                                    .map(|page| {
                                        t!("files.pdf_review_page", page = page).into_owned()
                                    })
                                    .collect();
                                CompletedFile {
                                    applied: doc
                                        .applied_units(&targets)
                                        .into_iter()
                                        .map(|index| {
                                            t!(
                                                "files.applied_pdf",
                                                page = doc.units()[index].page,
                                                unit = index + 1
                                            )
                                            .into_owned()
                                        })
                                        .collect(),
                                    has_changes: !doc.applied_units(&targets).is_empty(),
                                    output: FileOutput::Pdf(Arc::new(targets)),
                                    warnings,
                                }
                            })
                        }
                        (Some((_, FileDocument::Office { deck, .. })), Some(pptx)) => {
                            finish_office(deck, pptx, &self.files.target)
                        }
                        _ => None,
                    };
                }
                if self.files.completed.is_none() && self.files.message.is_none() {
                    self.files.message = Some(t!("files.translation_failed").into_owned());
                }
                self.jobs.finish_file(*job).unwrap_or(false)
            }
            Event::Failed { job, .. } if *job == active => {
                self.files.unit_translation = None;
                self.files.message = Some(t!("files.translation_failed").into_owned());
                self.jobs.finish_file(*job).unwrap_or(false)
            }
            Event::RuntimeFailed(_) => {
                self.files.unit_translation = None;
                self.files.cancelling = false;
                self.files.message = Some(t!("files.translation_failed").into_owned());
                self.jobs.finish_file(active);
                cx.notify();
                return false;
            }
            _ => return false,
        };
        self.files.cancelling = false;
        if resume {
            self.translate_now(window, cx);
        }
        cx.notify();
        true
    }

    pub(super) fn save_file(&mut self, cx: &mut Context<Self>) {
        if self.jobs.busy() || self.files.io_busy {
            return;
        }
        let Some((source, document)) = self.files.loaded.clone() else {
            return;
        };
        let Some(completed) = &self.files.completed else {
            return;
        };
        if !completed.has_changes {
            return;
        }
        let translation = completed.output.clone();
        let target = self.files.target.clone();
        let extension = match &translation {
            FileOutput::Text(_) => "txt",
            FileOutput::Markdown(_) => "md",
            FileOutput::Pdf(_) => "pdf",
            FileOutput::Office(_) => match &document {
                FileDocument::Office { deck, .. } => deck.extension(),
                _ => return,
            },
        };
        let suggested = format!(
            "{}.{target}.{extension}",
            source.file_stem().unwrap_or_default().to_string_lossy()
        );
        let picker =
            cx.prompt_for_new_path(source.parent().unwrap_or(Path::new(".")), Some(&suggested));
        self.files.io_busy = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let selected = picker.await;
            let result = match selected {
                Ok(Ok(Some(destination))) => Some(
                    cx.background_spawn(async move {
                        let bytes = match (document, translation) {
                            (FileDocument::Text(doc), FileOutput::Text(text)) => doc.write(&text),
                            (FileDocument::Markdown(_), FileOutput::Markdown(bytes)) => {
                                ::files::save_new(&source, &destination, &bytes)?;
                                return Ok(destination);
                            }
                            (FileDocument::Pdf(doc), FileOutput::Pdf(targets)) => {
                                doc.write(&targets, &target)?
                            }
                            (FileDocument::Office { deck, .. }, FileOutput::Office(parts)) => {
                                deck.write(OFFICE_TAGS, &target, &parts)?
                            }
                            _ => return Err(::files::FileError::MismatchedResults),
                        };
                        ::files::save_new(&source, &destination, &bytes)?;
                        Ok::<_, ::files::FileError>(destination)
                    })
                    .await,
                ),
                Ok(Err(error)) => Some(Err(::files::FileError::Io(std::io::Error::other(
                    error.to_string(),
                )))),
                _ => None,
            };
            let _ = this.update(cx, |this, cx| {
                this.files.io_busy = false;
                match result {
                    Some(Ok(path)) => {
                        this.files.saved = Some(path);
                        this.files.message = None;
                    }
                    Some(Err(error)) => {
                        this.files.message =
                            Some(t!("files.error", error = error.to_string()).into_owned())
                    }
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_files(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.jobs.busy();
        let mut content = v_flex()
            .gap_3()
            .child(div().text_lg().child(t!("files.title").into_owned()))
            .child(t!("files.preview").into_owned())
            .child(
                Button::new("choose-file")
                    .label(t!("files.choose").into_owned())
                    .disabled(busy || self.files.io_busy)
                    .on_click(cx.listener(|this, _, _, cx| this.choose_file(cx))),
            );
        if let Some((path, doc)) = &self.files.loaded {
            content = content.child(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            );
            let encoding = match doc {
                FileDocument::Text(doc) => doc.converted_encoding,
                FileDocument::Markdown(doc) => doc.converted_encoding(),
                _ => None,
            };
            if let Some(encoding) = encoding {
                content = content.child(t!("files.encoding", encoding = encoding).into_owned());
            }
            if busy {
                let progress = self.files.progress.map_or_else(
                    || t!("files.starting").into_owned(),
                    |(done, total)| t!("files.progress", done = done, total = total).into_owned(),
                );
                content = content.child(progress).child(
                    Button::new("cancel-file")
                        .danger()
                        .label(t!("action.stop").into_owned())
                        .disabled(self.files.cancelling)
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_file(cx))),
                );
            } else {
                content = content.child(
                    Button::new("translate-file")
                        .primary()
                        .label(t!("action.translate").into_owned())
                        .disabled(doc.empty() || self.files.io_busy)
                        .on_click(cx.listener(|this, _, window, cx| this.start_file(window, cx))),
                );
            }
        }
        if let Some(completed) = &self.files.completed {
            let warnings = &completed.warnings;
            content = content
                .child(if !completed.has_changes {
                    t!("files.no_changes").into_owned()
                } else if warnings.is_empty() {
                    t!("files.done").into_owned()
                } else {
                    t!("files.review", count = warnings.len()).into_owned()
                })
                .child(
                    div()
                        .text_sm()
                        .child(t!("files.machine_translation").into_owned()),
                )
                .child(
                    Button::new("save-file")
                        .primary()
                        .label(t!("files.save").into_owned())
                        .disabled(self.files.io_busy || !completed.has_changes)
                        .on_click(cx.listener(|this, _, _, cx| this.save_file(cx))),
                )
                .children(
                    warnings
                        .iter()
                        .take(self.files.shown_warnings.max(100))
                        .map(|warning| div().text_sm().child(warning.clone())),
                );
            if !completed.applied.is_empty() {
                content = content
                    .child(t!("files.applied_count", count = completed.applied.len()).into_owned());
                if self.files.shown_applied < completed.applied.len() {
                    content = content.child(
                        Button::new("show-file-applied")
                            .label(t!("files.show_applied").into_owned())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.files.shown_applied =
                                    this.files.shown_applied.saturating_add(100);
                                cx.notify();
                            })),
                    );
                }
                content = content.children(
                    completed
                        .applied
                        .iter()
                        .take(self.files.shown_applied)
                        .map(|line| div().text_sm().child(line.clone())),
                );
            }
            if warnings.len() > self.files.shown_warnings.max(100) {
                content = content.child(
                    Button::new("more-file-warnings")
                        .label(t!("files.show_more_warnings").into_owned())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.files.shown_warnings =
                                this.files.shown_warnings.max(100).saturating_add(100);
                            cx.notify();
                        })),
                );
            }
        }
        if self.files.io_busy {
            content = content.child(t!("files.working").into_owned());
        }
        if let Some(message) = &self.files.message {
            content = content.child(div().text_color(cx.theme().warning).child(message.clone()));
        }
        if let Some(path) = &self.files.saved {
            let path = path.clone();
            content = content.child(t!("files.saved").into_owned()).child(
                Button::new("reveal-file")
                    .label(t!("files.reveal").into_owned())
                    .on_click(move |_, _, cx| cx.reveal_path(&path)),
            );
        }
        div()
            .id("file-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .p_4()
            .child(content)
            .into_any_element()
    }

    pub(crate) fn allow_close(
        &mut self,
        quit: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.jobs.busy() && !self.files.io_busy {
            return true;
        }
        if window.has_active_dialog(cx) {
            return false;
        }
        let view = cx.entity().downgrade();
        let io_busy = self.files.io_busy;
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                .title(t!("files.close_title").into_owned())
                .child(
                    t!(if io_busy {
                        "files.wait_for_io"
                    } else {
                        "files.close_body"
                    })
                    .into_owned(),
                )
                .footer(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("continue-file")
                                .label(t!("files.continue").into_owned())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .when(!io_busy, |footer| {
                            footer.child(
                                Button::new("cancel-close-file")
                                    .danger()
                                    .label(t!("files.cancel_close").into_owned())
                                    .on_click(move |_, window, cx| {
                                        let _ = view.update(cx, |this, _| this.worker.cancel());
                                        window.close_dialog(cx);
                                        if quit {
                                            cx.quit();
                                        } else {
                                            window.remove_window();
                                        }
                                    }),
                            )
                        }),
                )
        });
        false
    }
}

#[cfg(test)]
mod file_completion_tests {
    use super::{OfficeDocument, UnitResult, UnitTranslation, finish_office};

    #[test]
    fn office_completion_rejects_zero_changes_after_amount_validation() {
        let deck =
            OfficeDocument::open(include_bytes!("../test-data/file-result.pptx").to_vec()).unwrap();
        for outcome in [
            UnitResult::Cut,
            UnitResult::Invalid,
            UnitResult::Complete {
                text: "売上高は100万円です。".into(),
                retried: false,
            },
            UnitResult::Complete {
                text: "Sales were 10 million yen.".into(),
                retried: false,
            },
        ] {
            let done = finish_office(
                &deck,
                UnitTranslation {
                    results: vec![Some(outcome)],
                    completed: 1,
                },
                "en",
            )
            .unwrap();
            assert!(!done.has_changes);
        }
        let done = finish_office(
            &deck,
            UnitTranslation {
                results: vec![Some(UnitResult::Complete {
                    text: "Sales were 1 million yen.".into(),
                    retried: false,
                })],
                completed: 1,
            },
            "en",
        )
        .unwrap();
        assert!(done.has_changes);
        let incomplete = finish_office(&deck, UnitTranslation::new(1), "en");
        assert!(incomplete.is_none());
    }
}
