//! File UI is a child of `view`, sharing its worker without a second model instance.
use std::sync::Arc;

use ::files::markdown::Markdown;
use ::files::office::{MAX_UNIT_TOKENS, OfficeDocument, TagFormat};
use ::files::pdf::Pdf;
use engine::units::{Unit, UnitRequest, UnitResult, UnitTuning};

use gpui_kit::component::{Icon, button::ButtonCustomVariant, progress::Progress};

use super::*;

const OFFICE_TAGS: TagFormat = TagFormat::Anchor;
const FILE_ACCENT: u32 = 0x315cdf;
pub(super) const FILE_PANEL_WIDTH: f32 = 780.;

fn file_action(button: Button, disabled: bool, cx: &App) -> Button {
    button
        .custom(
            ButtonCustomVariant::new(cx)
                .color(rgb(FILE_ACCENT).into())
                .foreground(white())
                .hover(rgb(0x254dc8).into())
                .active(rgb(0x1c3fae).into()),
        )
        .disabled(disabled)
        .when(!disabled, |button| button.bg(rgb(FILE_ACCENT)))
}

fn file_accent(cx: &App) -> Hsla {
    rgb(if cx.theme().is_dark() {
        0x9bb5ff
    } else {
        FILE_ACCENT
    })
    .into()
}

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
    review: FileReview,
    has_changes: bool,
}

#[derive(Default)]
struct FileReview {
    partial_translation: bool,
    update_toc: bool,
    check_slides: bool,
    check_pdf: bool,
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

fn finish_office(
    deck: &OfficeDocument,
    job: UnitTranslation,
    target: &str,
) -> Option<CompletedFile> {
    use ::files::office::RetentionReason;

    if job.completed != job.results.len() {
        return None;
    }
    let mut review = FileReview::default();
    for item in &deck.unsupported {
        match item.reason {
            RetentionReason::TableOfContents => review.update_toc = true,
            RetentionReason::Range
            | RetentionReason::ProtectedStructure
            | RetentionReason::InlineStructure => {
                review.partial_translation |= !item.source.trim().is_empty();
            }
            RetentionReason::UnsupportedPart => {
                review.partial_translation |=
                    item.part.starts_with("ppt/charts/") || item.part.starts_with("ppt/diagrams/");
            }
            RetentionReason::ReadingGuide => {}
        }
    }

    let mut has_changes = false;
    let mut results = job.results.into_iter();
    let mut parts = Vec::new();
    for part in deck.parts() {
        let targets = part
            .paragraphs
            .iter()
            .map(|paragraph| {
                let outcome = results.next().flatten()?;
                if let UnitResult::Complete { text, .. } = outcome
                    && paragraph.amounts_match(OFFICE_TAGS, &text, target)
                {
                    has_changes |= paragraph.changes_text(OFFICE_TAGS, &text, target);
                    return Some(text);
                }
                review.partial_translation = true;
                None
            })
            .collect::<Vec<_>>();
        review.check_slides |= !part.overflow_frames(OFFICE_TAGS, &targets).is_empty();
        parts.push(targets);
    }
    Some(CompletedFile {
        output: FileOutput::Office(parts),
        review,
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
                                has_changes: !text.trim().is_empty()
                                    && text.trim() != doc.source().trim(),
                                output: FileOutput::Text(text),
                                review: FileReview {
                                    partial_translation: !warnings.is_empty(),
                                    ..Default::default()
                                },
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
                                has_changes: export.translated > 0,
                                output: FileOutput::Markdown(Arc::new(export.bytes)),
                                review: FileReview {
                                    partial_translation: !export.retained_lines.is_empty(),
                                    ..Default::default()
                                },
                            }),
                        (Some((_, FileDocument::Pdf(doc))), Some(batch)) => {
                            batch.text_targets().map(|mut targets| {
                                doc.check_amounts(&mut targets, &self.files.target);
                                CompletedFile {
                                    has_changes: !doc.applied_units(&targets).is_empty(),
                                    review: FileReview {
                                        check_pdf: !doc.review_pages(&targets).is_empty(),
                                        ..Default::default()
                                    },
                                    output: FileOutput::Pdf(Arc::new(targets)),
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
        let content = match &self.files.loaded {
            Some((path, document)) => self.render_loaded_file(path, document, cx),
            None => self.render_file_drop_zone(cx),
        };
        let theme = cx.theme();
        div()
            .id("file-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_5()
            .py_6()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(FILE_PANEL_WIDTH))
                    .mx_auto()
                    .gap_3()
                    .child(content)
                    .when(self.files.io_busy, |panel| {
                        panel.child(
                            div()
                                .px_1()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("files.working").into_owned()),
                        )
                    })
                    .when_some(self.files.message.clone(), |panel, message| {
                        panel.child(
                            h_flex()
                                .gap_2()
                                .items_start()
                                .rounded_lg()
                                .border_1()
                                .border_color(theme.warning.opacity(0.42))
                                .bg(theme.warning.opacity(0.08))
                                .px_3()
                                .py_2()
                                .text_sm()
                                .child(Icon::new(Lucide::CircleAlert))
                                .child(div().min_w_0().child(message)),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_file_drop_zone(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let accent = file_accent(cx);
        let drop_enabled = self.current_job.is_none() && !self.file_operation_busy();
        v_flex()
            .id("file-drop-zone")
            .items_center()
            .justify_center()
            .text_center()
            .gap_4()
            .px_5()
            .py_12()
            .rounded_xl()
            .border_1()
            .border_dashed()
            .border_color(theme.border)
            .bg(theme.muted.opacity(0.36))
            .when(drop_enabled, |zone| {
                zone.drag_over::<ExternalPaths>(move |style, paths, _, _| {
                    if paths.paths().len() == 1 {
                        style.border_color(accent).bg(accent.opacity(0.10))
                    } else {
                        style
                    }
                })
            })
            .child(
                h_flex()
                    .justify_center()
                    .size(px(52.))
                    .rounded_xl()
                    .bg(accent.opacity(0.12))
                    .child(
                        Icon::new(Lucide::FileInput)
                            .size(px(24.))
                            .text_color(accent),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .child(t!("files.drop_prompt").into_owned()),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("PDF · Word · PowerPoint · Markdown · TXT"),
                    ),
            )
            .child(
                Button::new("choose-file")
                    .icon(Lucide::FolderOpen)
                    .label(t!("files.choose").into_owned())
                    .map(|button| file_action(button, self.file_operation_busy(), cx))
                    .on_click(cx.listener(|this, _, _, cx| this.choose_file(cx))),
            )
            .into_any_element()
    }

    fn render_loaded_file(
        &self,
        path: &Path,
        doc: &FileDocument,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let accent = file_accent(cx);
        let busy = self.jobs.busy();
        let io_busy = self.files.io_busy;
        let completed = self.files.completed.as_ref();
        let encoding = match doc {
            FileDocument::Text(doc) => doc.converted_encoding,
            FileDocument::Markdown(doc) => doc.converted_encoding(),
            _ => None,
        };
        let filename = path.file_name().unwrap_or_default().to_string_lossy();
        let translate = Button::new("translate-file")
            .label(
                if completed.is_some() {
                    t!("files.translate_again")
                } else {
                    t!("action.translate")
                }
                .into_owned(),
            )
            .disabled(doc.empty() || io_busy)
            .on_click(cx.listener(|this, _, window, cx| this.start_file(window, cx)));
        let actions = h_flex()
            .flex_wrap()
            .gap_2()
            .child(
                Button::new("choose-file")
                    .ghost()
                    .label(t!("files.change_file").into_owned())
                    .disabled(busy || io_busy)
                    .on_click(cx.listener(|this, _, _, cx| this.choose_file(cx))),
            )
            .map(|actions| {
                if busy {
                    actions.child(
                        Button::new("cancel-file")
                            .danger()
                            .outline()
                            .label(t!("action.stop").into_owned())
                            .disabled(self.files.cancelling)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_file(cx))),
                    )
                } else if completed.is_some_and(|result| result.has_changes) {
                    let primary = if let Some(path) = self.files.saved.clone() {
                        Button::new("open-saved-file")
                            .icon(Lucide::FileText)
                            .label(t!("files.open_saved").into_owned())
                            .on_click(move |_, _, cx| cx.open_with_system(&path))
                    } else {
                        self.save_file_button(cx).icon(Lucide::Download)
                    };
                    actions
                        .child(translate.ghost())
                        .child(file_action(primary, io_busy, cx))
                } else {
                    actions.child(file_action(translate, doc.empty() || io_busy, cx))
                }
            });
        v_flex()
            .rounded_xl()
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .items_center()
                    .flex_wrap()
                    .gap_3()
                    .p_4()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w(px(180.))
                            .gap_3()
                            .child(
                                h_flex()
                                    .justify_center()
                                    .flex_shrink_0()
                                    .size(px(40.))
                                    .rounded_lg()
                                    .bg(accent.opacity(0.12))
                                    .child(Icon::new(Lucide::FileText).text_color(accent)),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .font_semibold()
                                    .truncate()
                                    .child(filename.into_owned()),
                            ),
                    )
                    .child(actions),
            )
            .when_some(encoding, |card, encoding| {
                card.child(
                    div()
                        .px_4()
                        .pb_3()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t!("files.encoding", encoding = encoding).into_owned()),
                )
            })
            .when(busy, |card| {
                let (done, total) = self.files.progress.unwrap_or_default();
                card.child(
                    v_flex()
                        .gap_2()
                        .px_4()
                        .pb_4()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(
                            if total == 0 {
                                t!("files.starting")
                            } else {
                                t!("files.progress", done = done, total = total)
                            }
                            .into_owned(),
                        )
                        .when(total > 0, |progress| {
                            progress.child(
                                Progress::new("file-progress")
                                    .color(accent)
                                    .value(100. * done as f32 / total as f32),
                            )
                        }),
                )
            })
            .when_some(completed, |card, result| {
                card.child(self.render_file_result(result, cx))
            })
            .into_any_element()
    }

    fn save_file_button(&self, cx: &Context<Self>) -> Button {
        Button::new("save-file")
            .label(t!("files.save").into_owned())
            .disabled(self.files.io_busy)
            .on_click(cx.listener(|this, _, _, cx| this.save_file(cx)))
    }

    fn render_file_result(&self, result: &CompletedFile, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let saved = self.files.saved.as_ref();
        let status = if !result.has_changes {
            t!("files.no_changes")
        } else if saved.is_some() {
            t!("files.saved")
        } else {
            t!("files.done")
        };
        let review = &result.review;
        let muted = theme.muted_foreground;
        let guidance = [
            Some(if review.partial_translation {
                t!("files.partial_translation")
            } else {
                t!("files.machine_translation")
            }),
            review.update_toc.then(|| t!("files.update_toc")),
            review.check_slides.then(|| t!("files.check_slides")),
            review.check_pdf.then(|| t!("files.check_pdf")),
        ];
        v_flex()
            .gap_2()
            .border_t_1()
            .border_color(theme.border)
            .px_4()
            .py_3()
            .child(
                h_flex()
                    .flex_wrap()
                    .justify_between()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .font_medium()
                            .when(result.has_changes, |row| {
                                row.child(Icon::new(IconName::Check).text_color(file_accent(cx)))
                            })
                            .child(status.into_owned()),
                    )
                    .when_some(saved.cloned(), |row, path| {
                        row.child(
                            h_flex()
                                .flex_wrap()
                                .gap_2()
                                .child(self.save_file_button(cx).small().ghost())
                                .child(
                                    Button::new("reveal-file")
                                        .small()
                                        .ghost()
                                        .icon(Lucide::FolderOpen)
                                        .label(t!("files.reveal").into_owned())
                                        .on_click(move |_, _, cx| cx.reveal_path(&path)),
                                ),
                        )
                    }),
            )
            .when(result.has_changes, |panel| {
                panel.children(
                    guidance
                        .into_iter()
                        .flatten()
                        .map(|line| div().text_sm().text_color(muted).child(line.into_owned())),
                )
            })
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
            assert!(done.review.partial_translation);
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
        assert!(!done.review.partial_translation);
        let incomplete = finish_office(&deck, UnitTranslation::new(1), "en");
        assert!(incomplete.is_none());
    }

    #[test]
    fn retained_chart_text_prompts_review_but_slide_templates_do_not() {
        use ::files::office::{RetainedContent, RetentionReason};

        let mut deck =
            OfficeDocument::open(include_bytes!("../test-data/file-result.pptx").to_vec()).unwrap();
        for (part, partial) in [
            ("ppt/slideLayouts/slideLayout1.xml", false),
            ("ppt/charts/chart1.xml", true),
            ("ppt/diagrams/data1.xml", true),
        ] {
            deck.unsupported = vec![RetainedContent {
                part: part.into(),
                paragraph: None,
                in_table: false,
                count: 1,
                source: String::new(),
                reason: RetentionReason::UnsupportedPart,
            }];
            let result = finish_office(
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
            assert_eq!(result.review.partial_translation, partial, "{part}");
        }
    }
}
