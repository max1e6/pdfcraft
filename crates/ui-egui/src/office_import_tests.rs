//! Insert pages, Combine files and Create ▸ Multiple files with Office documents: LibreOffice
//! runs on the worker while frames go on, then the tool gets the PDF. The worker is held at a
//! gate, so a test sees the frame thread carry on while a conversion is "running". Whether
//! LibreOffice is installed decides the outcome, and each test checks the one it got: the pages
//! arrive, or the notice says LibreOffice is needed.

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use pdfcraft_engine::{Edit, Session};
use pdfcraft_render::{PageRenderer, RenderRequest, RequestKind};

use super::{Slot, takes_office};
use crate::PdfCraftApp;
use crate::files::FilePurpose;

/// A one-page PDF reading `text`.
fn pdf(text: &str) -> Vec<u8> {
    Session::new().create_from_text("t", text).unwrap().to_vec()
}

/// A Word document reading "Quarterly report" (PdfCraft's own Word export).
fn docx() -> Vec<u8> {
    let mut s = Session::new();
    let id = s.open("t.pdf", None, Arc::new(pdf("Quarterly report")), None).unwrap();
    s.get(id).unwrap().export_office(pdfcraft_engine::compare::OfficeFormat::Docx)
}

fn texts(bytes: &Arc<Vec<u8>>) -> Vec<String> {
    let mut r = PageRenderer::new(bytes.clone(), Default::default());
    (0..r.page_count())
        .map(|p| {
            let out = r.render(RenderRequest { page: p, kind: RequestKind::Text, scale: 1.0, ..Default::default() });
            out.text.map(|t| t.plain_text().trim().to_string()).unwrap_or_default()
        })
        .collect()
}

fn notice(app: &PdfCraftApp) -> String {
    app.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default()
}

/// The app, with a document reading "Existing" open, and the worker held before each file
/// until the returned sender lets it go.
fn harness() -> (Harness<'static, PdfCraftApp>, Sender<()>) {
    let (release, gate) = mpsc::channel();
    let gate = Arc::new(Mutex::new(gate));
    let mut h = Harness::builder().with_size(egui::vec2(1000.0, 700.0)).build_eframe(move |_| {
        let mut app = PdfCraftApp::new();
        app.set_option("language", "en").unwrap();
        app.office_gate = Some(gate);
        // (Before the first frame, so it opens at once rather than on the open worker.)
        app.open_bytes("existing.pdf", None, pdf("Existing")).unwrap();
        app
    });
    h.run_steps(3);
    (h, release)
}

/// Frames run while the worker holds `name`: nothing is used yet, and the progress card says
/// what is being converted.
fn frames_go_on_while_converting(h: &mut Harness<'static, PdfCraftApp>, name: &str) {
    h.run_steps(3);
    let app = h.state();
    assert_eq!(app.importing(), [name]);
    let label = app.progress_notice.as_ref().map(|p| p.label.clone()).unwrap_or_default();
    assert_eq!(label, format!("Converting {name} with LibreOffice… file 1 of 1"));
}

/// Let the worker go and run frames until the batch has gone on to its tool.
fn finish(h: &mut Harness<'static, PdfCraftApp>, release: &Sender<()>) {
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(180);
    while !h.state().pending_imports.is_empty() {
        assert!(Instant::now() < deadline, "the conversion didn't finish");
        std::thread::sleep(Duration::from_millis(50));
        h.step();
    }
    h.run_steps(2);
    assert!(h.state().progress_notice.is_none(), "the progress card goes once the batch is used");
}

/// Converted, or else the notice says LibreOffice is needed.
fn converted(app: &PdfCraftApp) -> bool {
    let message = notice(app);
    let failed = message.contains("Couldn't convert Report.docx");
    assert!(!failed || message.contains("LibreOffice"), "the failure names what is missing: {message}");
    !failed
}

#[test]
fn insert_pages_converts_a_word_document_on_the_worker() {
    let (mut h, release) = harness();
    h.state_mut().use_files(FilePurpose::InsertPages, vec![("Report.docx".into(), docx())]);
    frames_go_on_while_converting(&mut h, "Report.docx");
    let bytes = |h: &Harness<'static, PdfCraftApp>| h.state().session.get(h.state().views[0].id).unwrap().bytes.clone();
    assert_eq!(texts(&bytes(&h)), ["Existing"], "nothing is inserted before the conversion finishes");
    finish(&mut h, &release);
    if converted(h.state()) {
        let pages = texts(&bytes(&h));
        assert_eq!(pages.len(), 2, "{pages:?}");
        assert_eq!(pages[0], "Existing");
        assert!(pages[1].contains("Quarterly report"), "{pages:?}");
    } else {
        assert_eq!(texts(&bytes(&h)), ["Existing"]);
    }
}

#[test]
fn insert_pages_inserts_nothing_if_the_document_changed_during_the_conversion() {
    let (mut h, release) = harness();
    h.state_mut().use_files(FilePurpose::InsertPages, vec![("Report.docx".into(), docx())]);
    frames_go_on_while_converting(&mut h, "Report.docx");
    // The position chosen before belongs to the document as it was.
    assert!(h.state_mut().apply_edit(Edit::RotatePages { pages: vec![0], degrees: 90 }));
    finish(&mut h, &release);
    let app = h.state();
    assert_eq!(app.session.get(app.views[0].id).unwrap().info.pages.len(), 1);
    assert_eq!(notice(app), "The document changed while Report.docx was being converted, so nothing was inserted.");
}

/// The second insertion was picked before the first changed the document; only that change
/// came between, so both arrive, the first at the gap chosen for it.
#[test]
fn word_documents_inserted_one_after_another_all_arrive() {
    let (mut h, release) = harness();
    // A "+" before page 1 in the page grid.
    h.state_mut().views[0].insert_at = Some(0);
    h.state_mut().use_files(FilePurpose::InsertPages, vec![("Report.docx".into(), docx())]);
    h.state_mut().use_files(FilePurpose::InsertPages, vec![("Second.docx".into(), docx())]);
    h.run_steps(2);
    assert_eq!(h.state().importing(), ["Report.docx", "Second.docx"]);
    release.send(()).unwrap();
    finish(&mut h, &release);
    let app = h.state();
    let pages = texts(&app.session.get(app.views[0].id).unwrap().bytes);
    if converted(app) {
        // The second goes after the selection: the pages the first inserted.
        assert_eq!(pages.len(), 3, "{pages:?} ({})", notice(app));
        assert!(pages[0].contains("Quarterly report") && pages[1].contains("Quarterly report"), "{pages:?}");
        assert_eq!(pages[2], "Existing");
    } else {
        assert_eq!(pages, ["Existing"]);
    }
}

#[test]
fn combine_lists_a_word_document_among_pdfs_in_order() {
    let (mut h, release) = harness();
    let files = vec![("a.pdf".into(), pdf("A")), ("Report.docx".into(), docx()), ("b.pdf".into(), pdf("B"))];
    h.state_mut().use_files(FilePurpose::Combine, files);
    frames_go_on_while_converting(&mut h, "Report.docx");
    assert!(h.state().combine_draft.is_empty(), "the list waits for the whole batch");
    finish(&mut h, &release);
    let app = h.state();
    let names: Vec<&str> = app.combine_draft.iter().map(|f| f.name.as_str()).collect();
    if converted(app) {
        assert_eq!(names, ["a.pdf", "Report.docx", "b.pdf"]);
        let report = &app.combine_draft[1];
        assert!(report.problem.is_none() && report.pages == 1, "{:?}", report.problem);
        assert!(texts(&report.bytes)[0].contains("Quarterly report"));
    } else {
        assert_eq!(names, ["a.pdf", "b.pdf"]);
    }
}

#[test]
fn create_multiple_joins_a_word_document_with_the_other_files() {
    let (mut h, release) = harness();
    let files = vec![("notes.txt".into(), b"Some notes".to_vec()), ("Report.docx".into(), docx())];
    h.state_mut().use_files(FilePurpose::CreateMultiple, files);
    frames_go_on_while_converting(&mut h, "Report.docx");
    assert_eq!(h.state().views.len(), 1, "nothing opens before the conversion finishes");
    finish(&mut h, &release);
    let app = h.state();
    let doc = app.session.get(app.views.last().unwrap().id).unwrap();
    assert_eq!(doc.name, "Combined.pdf");
    let titles: Vec<&str> = doc.info.outline.iter().map(|o| o.title.as_str()).collect();
    if converted(app) {
        assert_eq!(titles, ["notes", "Report"]);
        assert!(texts(&doc.bytes)[1].contains("Quarterly report"));
    } else {
        assert_eq!(titles, ["notes"]);
        // What Create Multiple said stays, above why the Word file was left out.
        assert!(notice(app).starts_with("Created a PDF from 1 files\n"), "{}", notice(app));
    }
}

#[test]
fn separate_pdfs_writes_a_word_document_as_its_own_pdf() {
    let dir = std::env::temp_dir().join(format!("pdfcraft-office-separate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (mut h, release) = harness();
    h.state_mut().export_dir_override = Some(dir.to_string_lossy().into_owned());
    h.state_mut().use_files(FilePurpose::CreateMultipleSeparate, vec![("Report.docx".into(), docx())]);
    frames_go_on_while_converting(&mut h, "Report.docx");
    assert!(!dir.join("Report.pdf").exists());
    finish(&mut h, &release);
    let written = std::fs::read(dir.join("Report.pdf"));
    if converted(h.state()) {
        // Not taken for a PDF picked by mistake ("already a PDF").
        assert!(texts(&Arc::new(written.unwrap()))[0].contains("Quarterly report"), "{}", notice(h.state()));
    } else {
        assert!(written.is_err());
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// Files on disk (a picker, a drop, a folder): the Word file is read and converted on the
/// worker, the PDF read in its turn.
#[test]
fn picked_paths_with_a_word_document_go_through_the_worker() {
    let dir = std::env::temp_dir().join(format!("pdfcraft-office-paths-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (report, a) = (dir.join("Report.docx"), dir.join("a.pdf"));
    std::fs::write(&report, docx()).unwrap();
    std::fs::write(&a, pdf("A")).unwrap();
    let (mut h, release) = harness();
    h.state_mut().use_paths(FilePurpose::Combine, &[report, a]);
    frames_go_on_while_converting(&mut h, "Report.docx");
    finish(&mut h, &release);
    let app = h.state();
    let names: Vec<&str> = app.combine_draft.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, if converted(app) { vec!["Report.docx", "a.pdf"] } else { vec!["a.pdf"] });
    assert!(app.combine_draft.iter().all(|f| f.modified.is_some()), "Combine lists when each file was last modified");
    let _ = std::fs::remove_dir_all(dir);
}

/// A batch whose only file couldn't be converted says why, and nothing else.
#[test]
fn a_failed_conversion_is_reported_with_its_reason() {
    let mut app = PdfCraftApp::new();
    app.set_option("language", "en").unwrap();
    let slots = vec![Slot::Office { name: "Report.docx".into(), modified: None }, Slot::Office { name: "Deck.pptx".into(), modified: None }];
    let needs = "opening Word, Excel and PowerPoint files needs LibreOffice, which isn't installed (get it free from libreoffice.org)";
    let results = vec![Err(format!("Report.docx: {needs}")), Err(format!("Deck.pptx: {needs}"))];
    app.finish_import(FilePurpose::CreateMultiple, slots, results, None);
    assert!(app.views.is_empty());
    assert_eq!(notice(&app), format!("Couldn't convert Report.docx, Deck.pptx: {needs}"));
}

/// A batch without an Office document goes straight on, as before.
#[test]
fn a_batch_without_office_documents_is_used_at_once() {
    let (mut h, _release) = harness();
    h.state_mut().use_files(FilePurpose::InsertPages, vec![("extra.pdf".into(), pdf("Extra"))]);
    assert!(h.state().pending_imports.is_empty());
    let app = h.state();
    assert_eq!(texts(&app.session.get(app.views[0].id).unwrap().bytes), ["Existing", "Extra"]);
}

#[test]
fn only_the_tools_that_make_pages_take_office_documents() {
    for purpose in [FilePurpose::InsertPages, FilePurpose::Combine, FilePurpose::CreateMultiple, FilePurpose::CreateMultipleSeparate] {
        assert!(takes_office(purpose), "{purpose:?}");
    }
    for purpose in [FilePurpose::ReplacePages, FilePurpose::Ocr, FilePurpose::RedactWords, FilePurpose::TextFont { page: 0 }] {
        assert!(!takes_office(purpose), "{purpose:?}");
    }
    assert!(crate::files::office_named("Report.DOCX") && crate::files::office_named("a.b.odt"));
    assert!(!crate::files::office_named("report.pdf") && !crate::files::office_named("docx"));
}
