//! Files for Insert pages, Combine files and Create ▸ Multiple files, in the order picked. The
//! Office documents among them (Word, Excel, PowerPoint, OpenDocument, RTF) become PDFs on a
//! worker thread first: LibreOffice takes seconds a file (at most two minutes), which the frame
//! thread must not wait for. Meanwhile the progress card names the file being converted; then
//! the whole batch goes on to its tool, the Office documents as PDFs under their own names. A
//! batch without one goes on at once. (Desktop only: browsers can't run LibreOffice, and their
//! tools refuse Office documents straight away.)

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::SystemTime;

use crate::PdfCraftApp;
use crate::files::{FilePurpose, PickTarget};

/// Batches waiting at once; another is refused, with a notice.
const MAX_QUEUED: usize = 8;

/// Whether `purpose` is a tool that takes Office documents through a [`Batch`].
pub(crate) fn takes_office(purpose: FilePurpose) -> bool {
    matches!(purpose, FilePurpose::InsertPages | FilePurpose::Combine | FilePurpose::CreateMultiple | FilePurpose::CreateMultipleSeparate)
}

/// One file of a batch.
enum Slot {
    /// Read already.
    File { name: String, bytes: Vec<u8>, modified: Option<SystemTime> },
    /// Read when the batch goes on to its tool, so Create Multiple holds one file at a time.
    Path(PathBuf),
    /// Made a PDF by the worker: the batch's next result.
    Office { name: String, modified: Option<SystemTime> },
}

/// An Office document for the worker.
enum Source {
    Bytes(Arc<Vec<u8>>),
    Path(PathBuf),
}

type Converted = Result<Vec<u8>, String>;

/// Tests hold the worker before each file until they send on the gate's other end.
#[cfg(test)]
pub(crate) type Gate = Arc<Mutex<Receiver<()>>>;

/// Files for one of the tools that take Office documents ([`takes_office`]), in order.
#[derive(Default)]
pub(crate) struct Batch {
    slots: Vec<Slot>,
    /// The Office documents among them, for the worker.
    office: Vec<(String, Source)>,
}

impl Batch {
    /// A file read already: an Office document goes to the worker.
    pub(crate) fn file(&mut self, name: String, bytes: Vec<u8>, modified: Option<SystemTime>) {
        if pdfcraft_engine::source_kind(&name, &bytes) == Some(pdfcraft_engine::SourceKind::Office) {
            self.office.push((name.clone(), Source::Bytes(Arc::new(bytes))));
            self.slots.push(Slot::Office { name, modified });
        } else {
            self.slots.push(Slot::File { name, bytes, modified });
        }
    }

    /// A file on disk: one named as an Office document is read on the worker, the rest when the
    /// batch goes on.
    pub(crate) fn path(&mut self, path: PathBuf) {
        let name = crate::files::name_of(&path);
        if crate::files::office_named(&name) {
            let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
            self.office.push((name.clone(), Source::Path(path)));
            self.slots.push(Slot::Office { name, modified });
        } else {
            self.slots.push(Slot::Path(path));
        }
    }
}

struct Job {
    files: Vec<(String, Source)>,
    /// One message per file, in order.
    reply: Sender<Converted>,
}

struct Queue {
    jobs: VecDeque<Job>,
    shutdown: bool,
    repaint: Option<egui::Context>,
}

/// One thread converts every batch's Office documents, one at a time (one LibreOffice at once),
/// in the order the batches came.
pub(crate) struct OfficeWorker {
    queue: Arc<(Mutex<Queue>, Condvar)>,
}

impl OfficeWorker {
    fn new(#[cfg(test)] gate: Option<Gate>) -> std::io::Result<Self> {
        let queue = Arc::new((Mutex::new(Queue { jobs: VecDeque::new(), shutdown: false, repaint: None }), Condvar::new()));
        let worker_queue = queue.clone();
        std::thread::Builder::new().name("pdfcraft-office".into()).spawn(move || {
            run(
                worker_queue,
                #[cfg(test)]
                gate,
            )
        })?;
        Ok(Self { queue })
    }

    fn submit(&self, files: Vec<(String, Source)>, repaint: Option<egui::Context>) -> Result<Receiver<Converted>, String> {
        let (reply, receiver) = mpsc::channel();
        let (lock, ready) = &*self.queue;
        let mut queue = lock.lock().unwrap_or_else(PoisonError::into_inner);
        if queue.shutdown {
            return Err("the conversion worker has stopped".into());
        }
        if queue.jobs.len() >= MAX_QUEUED {
            return Err("too many files are waiting to be converted".into());
        }
        if repaint.is_some() {
            queue.repaint = repaint;
        }
        queue.jobs.push_back(Job { files, reply });
        ready.notify_one();
        Ok(receiver)
    }
}

impl Drop for OfficeWorker {
    fn drop(&mut self) {
        let (lock, ready) = &*self.queue;
        let mut queue = lock.lock().unwrap_or_else(PoisonError::into_inner);
        queue.shutdown = true;
        queue.jobs.clear();
        queue.repaint = None;
        ready.notify_one();
    }
}

fn run(queue: Arc<(Mutex<Queue>, Condvar)>, #[cfg(test)] gate: Option<Gate>) {
    loop {
        let job = {
            let (lock, ready) = &*queue;
            let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
            while state.jobs.is_empty() && !state.shutdown {
                state = ready.wait(state).unwrap_or_else(PoisonError::into_inner);
            }
            if state.shutdown {
                return;
            }
            state.jobs.pop_front()
        };
        let Some(Job { files, reply }) = job else { continue };
        for (name, source) in files {
            #[cfg(test)]
            if let Some(gate) = &gate
                && let Ok(gate) = gate.lock()
            {
                let _ = gate.recv();
            }
            let sent = reply.send(convert_guarded(&name, source)).is_ok();
            let repaint = queue.0.lock().unwrap_or_else(PoisonError::into_inner).repaint.clone();
            if let Some(ctx) = repaint {
                ctx.request_repaint();
            }
            // Nobody waits for the rest.
            if !sent {
                break;
            }
        }
    }
}

/// [`convert`], a panic in it reported as an error (AGENTS.md §4).
fn convert_guarded(name: &str, source: Source) -> Converted {
    pdfcraft_engine::guard(|| convert(name, source)).unwrap_or_else(|_| Err(format!("{name}: the file could not be read")))
}

/// The PDF LibreOffice makes of `source`. A file named like an Office document that isn't one
/// is passed on as it is, for its tool to take (a PDF) or refuse.
fn convert(name: &str, source: Source) -> Converted {
    let bytes = match source {
        Source::Bytes(bytes) => bytes,
        Source::Path(path) => Arc::new(std::fs::read(&path).map_err(|e| e.to_string())?),
    };
    if pdfcraft_engine::source_kind(name, &bytes) != Some(pdfcraft_engine::SourceKind::Office) {
        return Ok(Arc::unwrap_or_clone(bytes));
    }
    pdfcraft_engine::office_to_pdf(name, &bytes).map(Arc::unwrap_or_clone).map_err(|e| e.to_string())
}

/// A batch waiting for the worker.
pub(crate) struct PendingImport {
    purpose: FilePurpose,
    /// The document Insert pages adds to, as it was when the files were picked, and the gap
    /// chosen in its page grid (else the pages go after the selection).
    target: Option<PickTarget>,
    insert_at: Option<usize>,
    dir: Option<PathBuf>,
    slots: Vec<Slot>,
    /// The Office documents' names, in order, for the progress card.
    office: Vec<String>,
    results: Vec<Converted>,
    receiver: Receiver<Converted>,
}

impl PendingImport {
    fn done(&self) -> bool {
        self.results.len() >= self.office.len()
    }
}

impl PdfCraftApp {
    /// Use `batch` for `purpose`: now, unless it holds an Office document, which the worker
    /// converts first (here, with [`PdfCraftApp::run_inline`], for tests). `dir` is Separate
    /// PDFs' output folder.
    pub(crate) fn import(&mut self, purpose: FilePurpose, batch: Batch, dir: Option<PathBuf>) {
        let Batch { slots, office } = batch;
        if office.is_empty() {
            return self.finish_import(purpose, slots, Vec::new(), dir);
        }
        if self.run_inline {
            let results = office.into_iter().map(|(name, source)| convert_guarded(&name, source)).collect();
            return self.finish_import(purpose, slots, results, dir);
        }
        let names: Vec<String> = office.iter().map(|(name, _)| name.clone()).collect();
        if self.office_worker.is_none() {
            match OfficeWorker::new(
                #[cfg(test)]
                self.office_gate.clone(),
            ) {
                Ok(worker) => self.office_worker = Some(worker),
                Err(e) => return self.report_unconverted(&names, &format!("couldn't start the conversion worker: {e}")),
            }
        }
        let Some(worker) = &self.office_worker else { return };
        match worker.submit(office, self.ctx.clone()) {
            Ok(receiver) => {
                let (target, insert_at) = if purpose.edits_active_document() {
                    let insert_at = self.active.and_then(|i| self.views.get_mut(i)).and_then(|v| v.insert_at.take());
                    (self.pick_target(self.active_ids().map(|(_, id)| id)), insert_at)
                } else {
                    (None, None)
                };
                self.pending_imports.push(PendingImport { purpose, target, insert_at, dir, slots, office: names, results: Vec::new(), receiver });
                self.show_import_progress();
            }
            Err(e) => self.report_unconverted(&names, &e),
        }
    }

    /// Collect the worker's results; hand each finished batch to its tool, in the order the
    /// batches came (Insert pages' positions follow one another).
    pub(crate) fn poll_imports(&mut self) {
        for pending in &mut self.pending_imports {
            while !pending.done() {
                match pending.receiver.try_recv() {
                    Ok(result) => pending.results.push(result),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        pending.results.resize_with(pending.office.len(), || Err("the conversion stopped unexpectedly".into()));
                    }
                }
            }
        }
        while self.pending_imports.first().is_some_and(PendingImport::done) {
            let pending = self.pending_imports.remove(0);
            // Insert pages adds at a selection or a gap chosen before the conversion.
            if !self.pick_target_holds(pending.target) {
                let names = pending.office.join(", ");
                self.notify_fmt("The document changed while {name} was being converted, so nothing was inserted.", &[("name", &names)]);
                continue;
            }
            if let Some(view) = self.active.and_then(|i| self.views.get_mut(i))
                && pending.target.is_some()
            {
                view.insert_at = pending.insert_at;
            }
            let before = pending.target;
            self.finish_import(pending.purpose, pending.slots, pending.results, pending.dir);
            // The batches after it were picked for the document as it was: that is only this
            // insertion's doing, so they still apply to it.
            let after = before.and_then(|t| self.pick_target(Some(t.doc())));
            for later in self.pending_imports.iter_mut().filter(|p| p.target.is_some() && p.target == before) {
                later.target = after;
            }
        }
        self.show_import_progress();
    }

    /// Office documents waiting for the worker, for the control channel.
    pub(crate) fn importing(&self) -> Vec<&str> {
        self.pending_imports.iter().flat_map(|p| p.office.iter().skip(p.results.len()).map(String::as_str)).collect()
    }

    /// The progress card names the file LibreOffice is converting, while no other job holds it.
    fn show_import_progress(&mut self) {
        // The card is ours while it shows what we last put there.
        let ours = self.import_progress.take().filter(|label| self.progress_notice.as_ref().is_some_and(|p| p.label == *label));
        let Some(pending) = self.pending_imports.first() else {
            if ours.is_some() {
                self.progress_notice = None;
            }
            return;
        };
        if self.progress_notice.is_some() && ours.is_none() {
            return;
        }
        let (done, total) = (pending.results.len(), pending.office.len());
        let current = done.min(total.saturating_sub(1));
        let name = pending.office.get(current).map_or("", String::as_str);
        let label = crate::i18n::fmt(
            tl!("Converting {name} with LibreOffice… file {d} of {t}"),
            &[("name", name), ("d", &(current + 1).to_string()), ("t", &total.to_string())],
        );
        let fraction = if total == 0 { 0.0 } else { done as f32 / total as f32 };
        self.import_progress = Some(label.clone());
        self.progress_notice = Some(crate::widgets::ProgressNotice { label, fraction, cancellable: false });
    }

    /// Hand the batch to its tool, the converted Office documents (`results`, in order) as PDFs;
    /// then say which couldn't be converted and why, last, so that reason (LibreOffice isn't
    /// installed) is the notice left showing.
    fn finish_import(&mut self, purpose: FilePurpose, slots: Vec<Slot>, results: Vec<Converted>, dir: Option<PathBuf>) {
        // What the tool says ("Created a PDF from 2 files") stays, above why some were left out.
        let said_before = self.toast.clone();
        // Nothing else to use: the tool would only add "none of those files…".
        let nothing_left = !results.is_empty() && results.iter().all(Result::is_err) && slots.iter().all(|s| matches!(s, Slot::Office { .. }));
        let combine = purpose == FilePurpose::Combine;
        let mut results = results.into_iter();
        let mut failed: Vec<(String, String)> = Vec::new();
        let mut items = slots.into_iter().filter_map(|slot| match slot {
            Slot::File { name, bytes, modified } => Some((Ok((name, bytes)), modified)),
            Slot::Path(path) => {
                // Combine lists when each file was last modified.
                let modified = if combine { std::fs::metadata(&path).and_then(|m| m.modified()).ok() } else { None };
                Some((crate::files::read_named(&path), modified))
            }
            Slot::Office { name, modified } => match results.next() {
                Some(Ok(pdf)) => Some((Ok((name, pdf)), modified)),
                Some(Err(error)) => {
                    failed.push((name, error));
                    None
                }
                None => {
                    failed.push((name, "the conversion stopped unexpectedly".into()));
                    None
                }
            },
        });
        if !nothing_left {
            match purpose {
                FilePurpose::InsertPages => self.insert_each(items.by_ref().map(|(file, _)| file)),
                FilePurpose::CreateMultiple => self.stage_create_multiple(items.by_ref().map(|(file, _)| file)),
                FilePurpose::CreateMultipleSeparate => self.stage_create_separate(items.by_ref().map(|(file, _)| file), dir),
                FilePurpose::Combine => {
                    let mut incoming = Vec::new();
                    for (file, modified) in items.by_ref() {
                        match file {
                            Ok((name, bytes)) => {
                                incoming.push(crate::combine_ui::Incoming { name, bytes: Arc::new(bytes), modified, note: None, open_doc: None })
                            }
                            Err((name, e)) => self.notify_fmt("Couldn't read {name}: {e}", &[("name", &name), ("e", &e)]),
                        }
                    }
                    self.stage_combine_with(incoming);
                }
                // Not given batches (`takes_office`).
                _ => {}
            }
        } else {
            // Only Office documents, none converted: go through them for the failures.
            items.by_ref().for_each(drop);
        }
        // (Office documents after a file that stopped Create Multiple aren't reported: it said why.)
        drop(items);
        let mut reasons: Vec<(String, Vec<String>)> = Vec::new();
        for (name, error) in failed {
            // The engine's message names the file, which the notice already does.
            let why = error.strip_prefix(&format!("{name}: ")).unwrap_or(&error).to_string();
            match reasons.iter_mut().find(|(r, _)| *r == why) {
                Some((_, names)) => names.push(name),
                None => reasons.push((why, vec![name])),
            }
        }
        if !reasons.is_empty() {
            let mut lines: Vec<String> = self.toast.as_ref().filter(|t| said_before.as_ref() != Some(*t)).map(|t| t.0.clone()).into_iter().collect();
            lines.extend(
                reasons.iter().map(|(why, names)| crate::i18n::fmt(tl!("Couldn't convert {name}: {e}"), &[("name", &names.join(", ")), ("e", why)])),
            );
            self.notify(lines.join("\n"));
        }
    }

    fn report_unconverted(&mut self, names: &[String], error: &str) {
        self.notify(crate::i18n::fmt(tl!("Couldn't convert {name}: {e}"), &[("name", &names.join(", ")), ("e", error)]));
    }
}

#[cfg(test)]
#[path = "office_import_tests.rs"]
mod tests;
