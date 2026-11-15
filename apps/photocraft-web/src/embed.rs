//! Embed mode (`?embed`): PhotoCraft inside an `<iframe>` of a page on the same origin, which owns
//! the document. The host hands the document in and gets it back when it is saved; nothing is
//! picked from disk or downloaded for it. The protocol is in `docs/embedding.md`.
//!
//! Host → app, with `window.postMessage` to the iframe:
//! - `open {name, document, fonts}`: the `.pcraft` to edit (an `ArrayBuffer`) under the file name
//!   File › Save writes back to, and the font files its text uses (`[{family, data}]`), which are
//!   registered first so the document opens without missing-font warnings.
//!   Instead of `document`, `parts {manifest, objects: [{path, data}]}`: a recovery copy as the
//!   host kept it (below), made into a bundle and checked object by object on the way in.
//!   With `unsaved: true` (a recovery copy) the document opens as unsaved changes: closing asks,
//!   and only a save makes it saved.
//! - `save`: File › Save on that document.
//! - `saved {revision}`: the host stored the document of that revision; it counts as saved.
//! - `snapshot`: start a recovery copy now if the document has changes no copy holds yet (the
//!   host asks when its page is hidden).
//! - `snapshot-reset`: the host no longer holds the parts this app sent; the next copy sends all.
//!
//! App → host, to `window.parent`:
//! - `ready {pcraft, rev, version}`: the app runs and waits for `open`; `pcraft` is the `.pcraft`
//!   format version it writes, `rev` the commit it was built from (`PHOTOCRAFT_BUILD_SHA`).
//! - `opened {warnings}` or `open-failed {error}`.
//! - `dirty {dirty}`: whenever the document's unsaved state changes.
//! - `document {name, revision, document}`: File › Save wrote it (asked for by `save`, or the
//!   user's own File › Save); the `ArrayBuffer` is transferred.
//! - `save-failed {error}`.
//! - `snapshot-start {revision}`, then `snapshot-part {revision, path, data}` for each object the
//!   host does not hold yet, then `snapshot {revision, manifest, paths, parts, ms, maxSliceMs,
//!   slices}`: a recovery copy of the document with its unsaved changes, for the host to keep and
//!   hand back with `open {parts, unsaved: true}` if the page goes away unsaved (a crash, a lost
//!   network, a tab the browser closes without asking). A copy starts once the changes have rested
//!   for [`SNAPSHOT_QUIET_MS`] (see [`SnapshotSchedule`]) and on `snapshot`, and is written a
//!   [`SnapshotRun`] step per frame of about [`SNAPSHOT_SLICE_MS`], so it never stalls the GUI;
//!   objects are content-addressed, so a small edit sends only the tiles it changed. It is never a
//!   save: the document stays unsaved.
//! - `snapshot-failed {error}`: a copy could not be written.
//! - `start-failed {error}`: the app could not start (no WebGPU or WebGL2); nothing else follows.
//!
//! Every message is a plain object with `photocraftEmbed: 1` and a `type`. Only messages from the
//! parent window on this page's own origin are read, and only the parent is posted to, on that
//! origin: a page elsewhere can neither drive the app nor receive a document.
//!
//! The host's document opens as page tabs (`PhotocraftApp::artboards_as_tabs`): a design whose
//! pages are artboards gets one tab per page, every one a view of the same document, so `save`,
//! `document` and `dirty` stay about that one document whichever page tab is active. The last
//! page tab cannot be closed from the tab strip.
//!
//! In embed mode the host also owns leaving: the app's own "Leave site?" guard is off (the host
//! has `dirty`), and writes to anything but the host's document fail, so no export or download
//! leaves through the browser. A save records the revision it wrote (the writer is not
//! `write_unconfirmed` here); the document goes back to unsaved until the host's `saved`.

use std::cell::RefCell;
use std::rc::Rc;

#[cfg(target_arch = "wasm32")]
use photocraft_doc::DocId;
#[cfg(target_arch = "wasm32")]
use photocraft_ui_egui::{PhotocraftApp, i18n};

/// The message key and protocol version every message carries.
#[cfg(target_arch = "wasm32")]
pub const PROTOCOL_KEY: &str = "photocraftEmbed";
#[cfg(target_arch = "wasm32")]
pub const PROTOCOL_VERSION: u32 = 1;

/// The longest `?lang=` value read (a BCP 47 tag such as `zh-Hant-TW`).
const MAX_LANG: usize = 35;

/// Is the flag `key` in the page's query (`?cpu&embed`, `?embed=1`)?
pub fn query_has(search: &str, key: &str) -> bool {
    query_pairs(search).any(|(k, _)| k == key)
}

/// The value of `key` in the page's query, the first time it appears.
pub fn query_value<'a>(search: &'a str, key: &str) -> Option<&'a str> {
    query_pairs(search).find(|(k, _)| *k == key).map(|(_, v)| v)
}

fn query_pairs(search: &str) -> impl Iterator<Item = (&str, &str)> {
    search.trim_start_matches('?').split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').unwrap_or((p, "")))
}

/// The interface language a `?lang=` value asks for: a language tag of letters, digits, `-` and
/// `_` only (`de`, `pt-BR`, `zh_TW`). Whether PhotoCraft has that language is the caller's
/// question (`i18n::lang_from_tag`).
pub fn lang_tag(value: &str) -> Option<&str> {
    let ok = !value.is_empty() && value.len() <= MAX_LANG && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then_some(value)
}

/// The file name the host's document is opened under: a plain `.pcraft` name, so File › Save
/// writes it back in PhotoCraft's own format and in place (no dialog, no other format).
pub fn document_name(name: &str) -> Result<&str, String> {
    let plain = !name.is_empty() && name.len() <= 255 && !name.contains(['/', '\\']) && !name.chars().any(char::is_control);
    let pcraft = std::path::Path::new(name).extension().is_some_and(|e| e.eq_ignore_ascii_case(photocraft_format::EXTENSION));
    if plain && pcraft { Ok(name) } else { Err(format!("the document to open must have a plain .{} file name, not {name:?}", photocraft_format::EXTENSION)) }
}

/// What the web shell's writer does with a write to `path` in embed mode.
#[derive(Debug, PartialEq, Eq)]
pub enum Write {
    /// The host's document: its bytes go to the host.
    Host,
    /// Anything else (Save As another name, Export, Quick Export): refused, nothing downloads.
    Refused,
}

/// Where a write to `path` goes, given the host's document's name (once one is open).
pub fn route(document: Option<&str>, path: &str) -> Write {
    if document == Some(path) { Write::Host } else { Write::Refused }
}

/// Bytes File › Save wrote for the host, not yet posted.
pub type Written = Rc<RefCell<Option<Vec<u8>>>>;

/// The host's document's file name, once `open` named it (read by the writer).
pub type DocumentName = Rc<RefCell<Option<String>>>;

/// A command from the host page.
#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
pub enum Command {
    Open { name: String, document: Opened, fonts: Vec<(String, Vec<u8>)>, unsaved: bool },
    Save,
    Saved { revision: u64 },
    Snapshot,
    SnapshotReset,
}

/// What `open` hands over: a `.pcraft`, or a recovery copy as the parts the host kept.
#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
pub enum Opened {
    Bundle(Vec<u8>),
    Parts { manifest: Vec<u8>, objects: Vec<(String, Vec<u8>)> },
}

/// How long the document's changes rest before a recovery snapshot is posted.
pub const SNAPSHOT_QUIET_MS: f64 = 2_000.0;
/// The longest rest a slow snapshot stretches that to.
pub const SNAPSHOT_MAX_QUIET_MS: f64 = 15_000.0;
/// Maximum age of pending committed edits between pointer gestures.
const SNAPSHOT_MAX_PENDING_MS: f64 = 5_000.0;
/// How often a snapshot held back by a pointer drag looks again.
const SNAPSHOT_BUSY_RETRY_MS: f64 = 250.0;

/// What the recovery snapshot schedule does in a frame (see [`SnapshotSchedule::frame`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SnapshotStep {
    /// Nothing to snapshot: the document is saved, or the last snapshot holds this revision.
    Idle,
    /// Write and post a snapshot of the current revision now.
    Snapshot,
    /// Look again after this many milliseconds.
    Wait(f64),
}

/// When embed mode posts a recovery `snapshot`: once a revision with unsaved changes has rested,
/// or at once when the host asks, but never twice for one revision and never during a drag. A
/// slow snapshot makes the next rest longer (three times what it took), so a large document is
/// not written again and again while it is being worked on.
#[derive(Debug, Clone, Default)]
pub struct SnapshotSchedule {
    seen: Option<u64>,
    changed_at: f64,
    pending_since: Option<f64>,
    posted: Option<u64>,
    requested: bool,
    last_ms: f64,
}

impl SnapshotSchedule {
    /// The host asked for a snapshot (`snapshot`).
    pub fn request(&mut self) {
        self.requested = true;
    }

    /// The rest a revision needs before it is snapshotted.
    pub fn quiet_ms(&self) -> f64 {
        (self.last_ms * 3.0).clamp(SNAPSHOT_QUIET_MS, SNAPSHOT_MAX_QUIET_MS)
    }

    /// One frame: `now_ms` on any monotonic clock, the document's unsaved state and revision, and
    /// whether a pointer is held down (a drag in progress).
    pub fn frame(&mut self, now_ms: f64, dirty: bool, revision: u64, pointer_down: bool) -> SnapshotStep {
        if !dirty {
            // Saved: the next unsaved frame rests afresh, also at this revision (a document that
            // opens unsaved changes nothing before its first copy is due).
            self.posted = None;
            self.requested = false;
            self.seen = None;
            self.pending_since = None;
            return SnapshotStep::Idle;
        }
        if self.seen != Some(revision) {
            self.seen = Some(revision);
            self.changed_at = now_ms;
        }
        if self.posted == Some(revision) {
            self.requested = false;
            return SnapshotStep::Idle;
        }
        let pending_since = *self.pending_since.get_or_insert(now_ms);
        if pointer_down {
            return SnapshotStep::Wait(SNAPSHOT_BUSY_RETRY_MS);
        }
        let rested = now_ms - self.changed_at;
        let remaining = (self.quiet_ms() - rested).min(SNAPSHOT_MAX_PENDING_MS - (now_ms - pending_since));
        if self.requested || remaining <= 0.0 {
            return SnapshotStep::Snapshot;
        }
        SnapshotStep::Wait(remaining)
    }

    /// A snapshot of `revision` was posted after `took_ms`.
    pub fn posted(&mut self, revision: u64, took_ms: f64) {
        self.posted = Some(revision);
        self.pending_since = None;
        self.requested = false;
        self.last_ms = took_ms.max(0.0);
    }

    /// A snapshot of `revision` failed: not again for this revision (the next change tries anew).
    pub fn failed(&mut self, revision: u64) {
        self.posted(revision, self.last_ms);
    }
}

/// Main-thread time one recovery copy step may take per frame (a frame is 16.7 ms; the GUI draws
/// in the rest).
pub const SNAPSHOT_SLICE_MS: f64 = 6.0;
/// The same during a pointer drag, when the GUI's own frames are busiest.
pub const SNAPSHOT_DRAG_SLICE_MS: f64 = 3.0;
const _: () = assert!(SNAPSHOT_DRAG_SLICE_MS < SNAPSHOT_SLICE_MS, "a drag gets the smaller step");

/// What a recovery copy step did ([`SnapshotRun::step`]).
#[derive(Debug)]
pub enum RunStep {
    /// More steps are needed (call again next frame).
    Continue,
    /// The copy is complete: every part the host lacked was posted.
    Done(Finished),
    /// The copy could not be written.
    Failed(String),
}

/// A complete recovery copy: the manifest and every part it names.
#[derive(Debug)]
pub struct Finished {
    pub revision: u64,
    pub manifest: Vec<u8>,
    pub paths: Vec<String>,
    /// How many parts this copy posted (the rest the host held already).
    pub parts: usize,
    /// Main-thread time of all its steps, the longest step, and how many there were.
    pub work_ms: f64,
    pub max_slice_ms: f64,
    pub slices: u32,
}

/// One recovery copy of one revision, written a slice per frame so the GUI never stalls: the
/// document is frozen (an `Arc` of it; editing goes on copy-on-write), its tiles and blobs are
/// hashed with a time budget that resumes where it stopped, and only the parts the host does not
/// hold yet are compressed, one at a time, and posted as they are made.
pub struct SnapshotRun {
    revision: u64,
    doc: std::sync::Arc<photocraft_doc::Document>,
    parts: Option<photocraft_format::Parts>,
    next: usize,
    sent: std::collections::HashSet<String>,
    /// The budget of the next step when the last one could not get further (an object too
    /// large for one step: doubled until it fits, so the copy always completes).
    stretched_ms: f64,
    /// A running estimate of one part's compression time.
    part_ms: f64,
    work_ms: f64,
    max_slice_ms: f64,
    slices: u32,
}

impl SnapshotRun {
    pub fn new(doc: std::sync::Arc<photocraft_doc::Document>, revision: u64) -> Self {
        Self { revision, doc, parts: None, next: 0, sent: Default::default(), stretched_ms: 0.0, part_ms: 0.0, work_ms: 0.0, max_slice_ms: 0.0, slices: 0 }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// One step of about `budget_ms` on the clock `now` (ms): hashing first, then the parts the
    /// host does not hold (`known`), each posted with `post` as soon as it is compressed.
    pub fn step(
        &mut self,
        cache: &photocraft_io::PcraftExportCache,
        known: &std::collections::HashSet<String>,
        budget_ms: f64,
        now: &(dyn Fn() -> f64 + Sync),
        post: &mut dyn FnMut(&str, Vec<u8>),
    ) -> RunStep {
        let start = now();
        let deadline = start + budget_ms.max(self.stretched_ms);
        if self.parts.is_none() {
            let over = || now() >= deadline;
            match cache.prepare_parts_until(&self.doc, &over) {
                (Ok(parts), _) => {
                    self.parts = Some(parts);
                    self.stretched_ms = 0.0;
                }
                (Err(photocraft_format::FormatError::Cancelled(_)), progressed) => {
                    self.stretched_ms = if progressed { 0.0 } else { (budget_ms.max(self.stretched_ms)) * 2.0 };
                    self.account(now() - start);
                    return RunStep::Continue;
                }
                (Err(e), _) => return RunStep::Failed(e.to_string()),
            }
        }
        let Some(parts) = self.parts.as_ref() else { return RunStep::Continue };
        let mut compressed = false;
        while let Some(path) = parts.path(self.next) {
            if known.contains(path) || self.sent.contains(path) {
                self.next += 1;
                continue;
            }
            // At least one part per step, then only while the next one is likely to fit.
            if compressed && now() + self.part_ms > deadline {
                break;
            }
            let t = now();
            let data = match photocraft_io::compress_part(parts, self.next) {
                Ok(data) => data,
                Err(e) => return RunStep::Failed(e.to_string()),
            };
            let took = now() - t;
            self.part_ms = if self.part_ms == 0.0 { took } else { self.part_ms * 0.7 + took * 0.3 };
            post(path, data);
            self.sent.insert(path.to_owned());
            self.next += 1;
            compressed = true;
        }
        let complete = parts.path(self.next).is_none();
        self.account(now() - start);
        if !complete {
            return RunStep::Continue;
        }
        let Some(parts) = self.parts.take() else { return RunStep::Continue };
        RunStep::Done(Finished {
            revision: self.revision,
            manifest: parts.manifest().to_vec(),
            paths: parts.paths().map(str::to_owned).collect(),
            parts: self.sent.len(),
            work_ms: self.work_ms,
            max_slice_ms: self.max_slice_ms,
            slices: self.slices,
        })
    }

    fn account(&mut self, slice_ms: f64) {
        self.work_ms += slice_ms;
        self.max_slice_ms = self.max_slice_ms.max(slice_ms);
        self.slices += 1;
    }
}

/// The writer of embed mode (`Services::write`): the host's document is handed over in
/// `written`, any other write fails.
pub fn writer(name: DocumentName, written: Written) -> impl FnMut(&str, &[u8]) -> Result<(), String> {
    move |path: &str, bytes: &[u8]| match route(name.borrow().as_deref(), path) {
        Write::Host => {
            *written.borrow_mut() = Some(bytes.to_vec());
            Ok(())
        }
        Write::Refused => Err(refused_write().to_string()),
    }
}

#[cfg(target_arch = "wasm32")]
fn refused_write() -> &'static str {
    i18n::t("This PhotoCraft saves only into the page that opened it: File › Save. Downloads are turned off.")
}

#[cfg(not(target_arch = "wasm32"))]
fn refused_write() -> &'static str {
    "This PhotoCraft saves only into the page that opened it: File › Save. Downloads are turned off."
}

/// Embed mode's state in the web shell.
#[cfg(target_arch = "wasm32")]
pub struct Embed {
    host: Host,
    commands: Rc<RefCell<Vec<Command>>>,
    name: DocumentName,
    written: Written,
    /// The host's document once it is open (an open can wait behind a Color Settings prompt).
    doc: Option<DocId>,
    /// The open's warnings, posted with `opened`.
    warnings: Vec<String>,
    /// The `dirty` last posted.
    dirty: Option<bool>,
    /// The document's saved revision before this frame's own File › Save, if it makes one.
    frame_saved: u64,
    /// The host's `open` asked for the document to open unsaved (a recovery copy).
    open_unsaved: bool,
    /// The parts the host holds for this document's recovery copy (the last complete copy's): a
    /// later copy posts only the others.
    known: std::collections::HashSet<String>,
    /// The recovery copy being written, a step per frame.
    run: Option<SnapshotRun>,
    /// When the next recovery snapshot is due.
    snapshots: SnapshotSchedule,
}

/// The parent window and the origin both sides share.
#[cfg(target_arch = "wasm32")]
struct Host {
    parent: web_sys::Window,
    origin: String,
}

#[cfg(target_arch = "wasm32")]
impl Embed {
    /// Embed mode, when the page asked for it (`?embed`) and is framed by a page on its own
    /// origin; `None` otherwise (the app then runs as the standalone web app). Starts listening
    /// for the host's commands, waking `ctx` when one arrives.
    pub fn start(search: &str, ctx: &egui::Context) -> Option<(Self, DocumentName, Written)> {
        if !query_has(search, "embed") {
            return None;
        }
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) {
            log::warn!("photocraft-web: ?embed outside a frame; running standalone");
            return None;
        }
        let origin = window.location().origin().ok()?;
        // A cross-origin parent throws on reading its location.
        if parent.location().origin().ok().as_deref() != Some(origin.as_str()) {
            log::warn!("photocraft-web: ?embed in a frame of another origin; running standalone");
            return None;
        }
        let commands: Rc<RefCell<Vec<Command>>> = Rc::default();
        listen(&window, parent.clone(), origin.clone(), commands.clone(), ctx.clone());
        let (name, written) = (DocumentName::default(), Written::default());
        let embed = Embed {
            host: Host { parent, origin },
            commands,
            name: name.clone(),
            written: written.clone(),
            doc: None,
            warnings: Vec::new(),
            dirty: None,
            frame_saved: 0,
            open_unsaved: false,
            known: Default::default(),
            run: None,
            snapshots: SnapshotSchedule::default(),
        };
        Some((embed, name, written))
    }

    /// Tells the host the app runs and which format it writes.
    pub fn ready(&self) {
        let msg = message("ready");
        set(&msg, "pcraft", &photocraft_format::FORMAT_VERSION.into());
        set(&msg, "rev", &photocraft_engine::build_info::git_sha().map_or(wasm_bindgen::JsValue::NULL, Into::into));
        set(&msg, "version", &photocraft_engine::build_info::VERSION.into());
        self.host.post(&msg, None);
    }

    /// Runs the host's commands; called at the start of every frame.
    pub fn before_frame(&mut self, app: &mut PhotocraftApp, ctx: &egui::Context) {
        let commands = std::mem::take(&mut *self.commands.borrow_mut());
        for command in commands {
            match command {
                Command::Open { name, document, fonts, unsaved } => {
                    self.open_unsaved = unsaved;
                    let document = match document {
                        Opened::Bundle(bytes) => Ok(bytes),
                        Opened::Parts { manifest, objects } => {
                            // The host holds these parts: a copy of this document posts only the
                            // others.
                            self.known = objects.iter().map(|(path, _)| path.clone()).collect();
                            photocraft_io::bundle_from_parts(&manifest, &objects).map_err(|e| e.to_string())
                        }
                    };
                    match document {
                        Ok(bytes) => self.open(app, &name, &bytes, fonts),
                        Err(e) => self.open_failed(e),
                    }
                }
                Command::Save => self.save(app, ctx),
                Command::Saved { revision } => self.saved(app, revision),
                Command::Snapshot => self.snapshots.request(),
                Command::SnapshotReset => {
                    // The host no longer holds what this app sent: the next copy sends it all.
                    self.known.clear();
                    self.run = None;
                    self.snapshots.request();
                }
            }
        }
        self.frame_saved = self.state(app).map_or(0, |st| st.saved_revision);
    }

    /// Reports what the frame changed: the document now open, a save, the unsaved state, and a
    /// recovery snapshot when one is due.
    pub fn after_frame(&mut self, app: &mut PhotocraftApp, ctx: &egui::Context) {
        if self.doc.is_none()
            && let Some(name) = self.name.borrow().clone()
            && let Some(st) = app.session.documents().iter().find(|st| st.path.as_deref() == Some(name.as_str()))
        {
            self.doc = Some(st.doc.id);
            if std::mem::take(&mut self.open_unsaved) {
                // A recovery copy: unsaved changes until a save, as content that was never saved
                // (closing asks).
                self.with_doc(app, |st| st.saved_revision = 0);
            }
            let msg = message("opened");
            set(&msg, "warnings", &strings(&std::mem::take(&mut self.warnings)));
            self.host.post(&msg, None);
        }
        // The user's own File › Save (a menu or Ctrl+S) during this frame.
        if self.written.borrow().is_some() {
            let revision = self.unconfirm(app, self.frame_saved);
            self.post_document(revision);
        }
        let dirty = self.state(app).map(|st| st.is_dirty());
        if let Some(dirty) = dirty
            && self.dirty != Some(dirty)
        {
            self.dirty = Some(dirty);
            let msg = message("dirty");
            set(&msg, "dirty", &dirty.into());
            self.host.post(&msg, None);
        }
        self.snapshot_tick(app, ctx);
    }

    /// Writes recovery copies: starts one when [`SnapshotSchedule`] says one is due, then a
    /// [`SnapshotRun`] step per frame (about [`SNAPSHOT_SLICE_MS`]) until it is complete, waking
    /// the next frame as long as one is under way.
    fn snapshot_tick(&mut self, app: &PhotocraftApp, ctx: &egui::Context) {
        let Some(st) = self.state(app) else { return };
        let now = ctx.input(|input| input.time * 1000.0);
        let pointer_down = ctx.input(|i| i.pointer.any_down());
        if !st.is_dirty() {
            // Saved: the host drops its copy with the save, so nothing it held counts any more.
            self.run = None;
            self.known.clear();
            self.snapshots.frame(now, false, st.revision, pointer_down);
            return;
        }
        if self.run.is_none() {
            match self.snapshots.frame(now, true, st.revision, pointer_down) {
                SnapshotStep::Idle => return,
                SnapshotStep::Wait(ms) => {
                    ctx.request_repaint_after(std::time::Duration::from_secs_f64(ms / 1000.0));
                    return;
                }
                SnapshotStep::Snapshot => {
                    self.run = Some(SnapshotRun::new(st.doc.clone(), st.revision));
                    let msg = message("snapshot-start");
                    set(&msg, "revision", &(st.revision as f64).into());
                    self.host.post(&msg, None);
                }
            }
        }
        let Some(run) = self.run.as_mut() else { return };
        let budget = if pointer_down { SNAPSHOT_DRAG_SLICE_MS } else { SNAPSHOT_SLICE_MS };
        let host = &self.host;
        let revision = run.revision();
        let mut post = |path: &str, data: Vec<u8>| {
            let array = js_sys::Uint8Array::from(data.as_slice());
            let buffer = array.buffer();
            let msg = message("snapshot-part");
            set(&msg, "revision", &(revision as f64).into());
            set(&msg, "path", &path.into());
            set(&msg, "data", &buffer);
            host.post(&msg, Some(&js_sys::Array::of1(&buffer)));
        };
        match run.step(&st.pcraft_cache, &self.known, budget, &js_sys::Date::now, &mut post) {
            RunStep::Continue => ctx.request_repaint(),
            RunStep::Done(done) => {
                self.run = None;
                self.snapshots.posted(done.revision, done.work_ms);
                let manifest = js_sys::Uint8Array::from(done.manifest.as_slice()).buffer();
                let msg = message("snapshot");
                set(&msg, "revision", &(done.revision as f64).into());
                set(&msg, "manifest", &manifest);
                set(&msg, "paths", &strings(&done.paths));
                set(&msg, "parts", &(done.parts as f64).into());
                set(&msg, "ms", &done.work_ms.round().into());
                set(&msg, "maxSliceMs", &done.max_slice_ms.round().into());
                set(&msg, "slices", &done.slices.into());
                self.host.post(&msg, Some(&js_sys::Array::of1(&manifest)));
                self.known = done.paths.into_iter().collect();
                // A newer revision may be due already.
                ctx.request_repaint();
            }
            RunStep::Failed(error) => {
                self.run = None;
                self.snapshots.failed(revision);
                log::error!("photocraft-web: recovery copy: {error}");
                let msg = message("snapshot-failed");
                set(&msg, "error", &error.into());
                self.host.post(&msg, None);
            }
        }
    }

    fn state<'a>(&self, app: &'a PhotocraftApp) -> Option<&'a photocraft_engine::DocState> {
        let doc = self.doc?;
        app.session.documents().iter().find(|st| st.doc.id == doc)
    }

    fn open(&mut self, app: &mut PhotocraftApp, name: &str, document: &[u8], fonts: Vec<(String, Vec<u8>)>) {
        let result = (|| {
            if self.name.borrow().is_some() {
                return Err("this PhotoCraft already holds the host's document".to_string());
            }
            let name = document_name(name)?;
            // The document's fonts first: text then lays out in them, and the open names none
            // missing.
            photocraft_ui_egui::served_fonts::install(app, fonts);
            let warnings = app.open_bytes_from(name, document, Some(name))?;
            *self.name.borrow_mut() = Some(name.to_string());
            Ok(warnings)
        })();
        match result {
            Ok(warnings) => self.warnings = warnings,
            Err(e) => {
                log::error!("photocraft-web: open: {e}");
                let msg = message("open-failed");
                set(&msg, "error", &e.into());
                self.host.post(&msg, None);
            }
        }
    }

    /// File › Save on the host's document, whichever tab is active, then back to that tab.
    fn save(&mut self, app: &mut PhotocraftApp, ctx: &egui::Context) {
        let result = (|| {
            let doc = self.doc.ok_or("no document is open")?;
            let index = app.session.documents().iter().position(|st| st.doc.id == doc).ok_or("the document was closed")?;
            let active = app.session.active_index();
            app.session.set_active(index);
            let before = app.session.documents().get(index).map_or(0, |st| st.saved_revision);
            let saved = photocraft_ui_egui::menus::invoke(app, ctx, "file.save", serde_json::json!({}));
            if let Some(active) = active {
                app.session.set_active(active);
            }
            saved?;
            Ok::<u64, String>(before)
        })();
        match result {
            Ok(before) if self.written.borrow().is_some() => {
                let revision = self.unconfirm(app, before);
                self.post_document(revision);
            }
            Ok(_) => self.save_failed("the document was not written (a prompt may be waiting in PhotoCraft)".into()),
            Err(e) => self.save_failed(e),
        }
    }

    fn open_failed(&self, error: String) {
        log::error!("photocraft-web: open: {error}");
        let msg = message("open-failed");
        set(&msg, "error", &error.into());
        self.host.post(&msg, None);
    }

    fn save_failed(&self, error: String) {
        log::error!("photocraft-web: save: {error}");
        let msg = message("save-failed");
        set(&msg, "error", &error.into());
        self.host.post(&msg, None);
    }

    /// The host stored the document of `revision`: it is saved at that revision (still unsaved
    /// when it was edited since).
    fn saved(&mut self, app: &mut PhotocraftApp, revision: u64) {
        self.with_doc(app, |st| {
            if revision <= st.revision {
                st.saved_revision = revision;
            }
        });
    }

    /// The revision a save just wrote, which File › Save recorded as the saved one; the document
    /// goes back to `before` (unsaved) until the host has stored it.
    fn unconfirm(&self, app: &mut PhotocraftApp, before: u64) -> u64 {
        self.with_doc(app, |st| std::mem::replace(&mut st.saved_revision, before)).unwrap_or(before)
    }

    /// Runs `f` on the host's document, whichever tab is active.
    fn with_doc<R>(&self, app: &mut PhotocraftApp, f: impl FnOnce(&mut photocraft_engine::DocState) -> R) -> Option<R> {
        let doc = self.doc?;
        let index = app.session.documents().iter().position(|st| st.doc.id == doc)?;
        let active = app.session.active_index();
        app.session.set_active(index);
        let r = app.session.active_mut().map(f);
        if let Some(active) = active {
            app.session.set_active(active);
        }
        r
    }

    fn post_document(&self, revision: u64) {
        let Some(bytes) = self.written.borrow_mut().take() else { return };
        let data = js_sys::Uint8Array::from(bytes.as_slice());
        let buffer = data.buffer();
        let msg = message("document");
        set(&msg, "name", &self.name.borrow().clone().unwrap_or_default().into());
        set(&msg, "revision", &(revision as f64).into());
        set(&msg, "document", &buffer);
        self.host.post(&msg, Some(&js_sys::Array::of1(&buffer)));
    }
}

/// Tells an embedding host that the app could not start (`start-failed`): without it the host
/// would wait for `ready` forever. Nothing happens outside embed mode.
#[cfg(target_arch = "wasm32")]
pub fn start_failed(search: &str, error: &str) {
    if !query_has(search, "embed") {
        return;
    }
    let Some(window) = web_sys::window() else { return };
    let Some(parent) = window.parent().ok().flatten() else { return };
    let Ok(origin) = window.location().origin() else { return };
    if js_sys::Object::is(&parent, &window) || parent.location().origin().ok().as_deref() != Some(origin.as_str()) {
        return;
    }
    let msg = message("start-failed");
    set(&msg, "error", &error.into());
    Host { parent, origin }.post(&msg, None);
}

#[cfg(target_arch = "wasm32")]
impl Host {
    fn post(&self, msg: &js_sys::Object, transfer: Option<&js_sys::Array>) {
        let posted = match transfer {
            Some(t) => self.parent.post_message_with_transfer(msg, &self.origin, t),
            None => self.parent.post_message(msg, &self.origin),
        };
        if let Err(e) = posted {
            log::error!("photocraft-web: posting to the host failed: {e:?}");
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn message(kind: &str) -> js_sys::Object {
    let msg = js_sys::Object::new();
    set(&msg, PROTOCOL_KEY, &PROTOCOL_VERSION.into());
    set(&msg, "type", &kind.into());
    msg
}

#[cfg(target_arch = "wasm32")]
fn set(target: &js_sys::Object, key: &str, value: &wasm_bindgen::JsValue) {
    let _ = js_sys::Reflect::set(target, &key.into(), value);
}

#[cfg(target_arch = "wasm32")]
fn strings(items: &[String]) -> js_sys::Array {
    items.iter().map(|s| wasm_bindgen::JsValue::from_str(s)).collect()
}

/// Queues the host's commands as they arrive, from the parent window on this origin only.
#[cfg(target_arch = "wasm32")]
fn listen(window: &web_sys::Window, parent: web_sys::Window, origin: String, commands: Rc<RefCell<Vec<Command>>>, ctx: egui::Context) {
    use wasm_bindgen::JsCast as _;
    use wasm_bindgen::closure::Closure;
    let cb = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
        let from_parent = e.source().is_some_and(|s| js_sys::Object::is(&s, &parent));
        if e.origin() != origin || !from_parent {
            return;
        }
        match command(&e.data()) {
            Ok(Some(c)) => {
                commands.borrow_mut().push(c);
                ctx.request_repaint();
            }
            Ok(None) => {}
            Err(err) => log::error!("photocraft-web: the host's message: {err}"),
        }
    });
    if window.add_event_listener_with_callback("message", cb.as_ref().unchecked_ref()).is_ok() {
        cb.forget();
    }
}

/// A host message read: `None` for messages of other protocols (the page may post its own).
#[cfg(target_arch = "wasm32")]
fn command(data: &wasm_bindgen::JsValue) -> Result<Option<Command>, String> {
    use js_sys::Reflect;
    use wasm_bindgen::JsCast as _;
    let field = |key: &str| Reflect::get(data, &key.into()).unwrap_or(wasm_bindgen::JsValue::UNDEFINED);
    if !data.is_object() || field(PROTOCOL_KEY).as_f64() != Some(f64::from(PROTOCOL_VERSION)) {
        return Ok(None);
    }
    let bytes = |v: wasm_bindgen::JsValue, what: &str| -> Result<Vec<u8>, String> {
        let buffer = v.dyn_into::<js_sys::ArrayBuffer>().map_err(|_| format!("{what} is not an ArrayBuffer"))?;
        Ok(js_sys::Uint8Array::new(&buffer).to_vec())
    };
    match field("type").as_string().as_deref() {
        Some("open") => {
            let name = field("name").as_string().ok_or("open needs a name")?;
            let parts = field("parts");
            let document = if parts.is_object() {
                let manifest = bytes(Reflect::get(&parts, &"manifest".into()).unwrap_or(wasm_bindgen::JsValue::UNDEFINED), "open's parts manifest")?;
                let list = Reflect::get(&parts, &"objects".into()).ok().and_then(|l| l.dyn_into::<js_sys::Array>().ok()).ok_or("open's parts need objects")?;
                let mut objects = Vec::with_capacity(list.length() as usize);
                for part in list.iter() {
                    let path = Reflect::get(&part, &"path".into()).ok().and_then(|p| p.as_string()).ok_or("a part needs a path")?;
                    let data = bytes(Reflect::get(&part, &"data".into()).unwrap_or(wasm_bindgen::JsValue::UNDEFINED), "a part's data")?;
                    objects.push((path, data));
                }
                Opened::Parts { manifest, objects }
            } else {
                Opened::Bundle(bytes(field("document"), "open's document")?)
            };
            let mut fonts = Vec::new();
            if let Ok(list) = field("fonts").dyn_into::<js_sys::Array>() {
                for font in list.iter() {
                    let family = Reflect::get(&font, &"family".into()).ok().and_then(|f| f.as_string()).ok_or("a font needs a family")?;
                    let data = bytes(Reflect::get(&font, &"data".into()).unwrap_or(wasm_bindgen::JsValue::UNDEFINED), "a font's data")?;
                    fonts.push((family, data));
                }
            }
            let unsaved = field("unsaved").as_bool().unwrap_or(false);
            Ok(Some(Command::Open { name, document, fonts, unsaved }))
        }
        Some("save") => Ok(Some(Command::Save)),
        Some("snapshot") => Ok(Some(Command::Snapshot)),
        Some("snapshot-reset") => Ok(Some(Command::SnapshotReset)),
        Some("saved") => {
            let revision = field("revision").as_f64().filter(|r| r.is_finite() && *r >= 0.0 && r.fract() == 0.0).ok_or("saved needs a revision")?;
            Ok(Some(Command::Saved { revision: revision as u64 }))
        }
        other => Err(format!("unknown message type {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_flags_and_values_are_read_by_name() {
        let q = "?embed&cpu&lang=de";
        assert!(query_has(q, "embed") && query_has(q, "cpu") && query_has(q, "lang"));
        assert!(!query_has(q, "webgl"));
        assert!(!query_has("?embedded", "embed"), "a longer flag is another flag");
        assert_eq!(query_value(q, "lang"), Some("de"));
        assert_eq!(query_value("?lang=pt-BR&lang=en", "lang"), Some("pt-BR"), "the first one");
        assert_eq!(query_value("", "lang"), None);
        assert!(query_has("?embed=1", "embed"));
    }

    #[test]
    fn only_plain_language_tags_are_taken() {
        for ok in ["de", "en", "pt-BR", "zh_TW", "zh-Hant-TW"] {
            assert_eq!(lang_tag(ok), Some(ok));
        }
        for bad in ["", "de%20", "de;", "<de>", &"x".repeat(MAX_LANG + 1)] {
            assert_eq!(lang_tag(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_hosts_document_is_a_plain_pcraft_name() {
        assert_eq!(document_name("Hochzeit Sommer.pcraft"), Ok("Hochzeit Sommer.pcraft"));
        assert_eq!(document_name("CAPS.PCRAFT"), Ok("CAPS.PCRAFT"));
        for bad in ["", "design.psd", "design", "../design.pcraft", "a/b.pcraft", "a\\b.pcraft", "a\nb.pcraft"] {
            assert!(document_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn only_the_hosts_document_is_written_and_everything_else_is_refused() {
        assert_eq!(route(Some("d.pcraft"), "d.pcraft"), Write::Host);
        assert_eq!(route(Some("d.pcraft"), "d.psd"), Write::Refused, "no PSD leaves through the browser");
        assert_eq!(route(Some("d.pcraft"), "d.png"), Write::Refused);
        assert_eq!(route(None, "d.pcraft"), Write::Refused, "nothing before the host named its document");

        let name = DocumentName::default();
        let written = Written::default();
        let mut write = writer(name.clone(), written.clone());
        assert!(write("d.pcraft", b"x").is_err());
        *name.borrow_mut() = Some("d.pcraft".into());
        assert!(write("d.pcraft", b"pcraft bytes").is_ok());
        assert_eq!(written.borrow().as_deref(), Some(&b"pcraft bytes"[..]));
        let refused = write("d.psd", b"psd").expect_err("export refused");
        assert!(refused.contains("Downloads are turned off"), "{refused}");
    }

    #[test]
    fn a_recovery_snapshot_waits_for_the_changes_to_rest_and_never_repeats_a_revision() {
        let mut s = SnapshotSchedule::default();
        assert_eq!(s.frame(0.0, false, 1, false), SnapshotStep::Idle, "a saved document needs no copy");
        // An edit: wait for the rest, then once.
        assert_eq!(s.frame(1_000.0, true, 2, false), SnapshotStep::Wait(SNAPSHOT_QUIET_MS));
        assert_eq!(s.frame(2_500.0, true, 2, false), SnapshotStep::Wait(500.0));
        assert_eq!(s.frame(3_000.0, true, 2, false), SnapshotStep::Snapshot);
        s.posted(2, 40.0);
        assert_eq!(s.frame(9_000.0, true, 2, false), SnapshotStep::Idle, "this revision is held already");
        // Another edit starts the rest again.
        assert_eq!(s.frame(10_000.0, true, 3, false), SnapshotStep::Wait(SNAPSHOT_QUIET_MS));
        assert_eq!(s.frame(11_000.0, true, 4, false), SnapshotStep::Wait(SNAPSHOT_QUIET_MS), "every change restarts it");
        assert_eq!(s.frame(13_000.0, true, 4, false), SnapshotStep::Snapshot);
    }

    #[test]
    fn continued_edits_cannot_starve_recovery() {
        let mut s = SnapshotSchedule::default();
        for revision in 0..5 {
            assert!(matches!(s.frame(revision as f64 * 1000.0, true, revision, false), SnapshotStep::Wait(_)));
        }
        assert_eq!(s.frame(5000.0, true, 5, false), SnapshotStep::Snapshot);
    }

    #[test]
    fn a_drag_holds_a_snapshot_back_and_the_host_can_ask_for_one_at_once() {
        let mut s = SnapshotSchedule::default();
        s.frame(0.0, true, 2, false);
        assert_eq!(s.frame(5_000.0, true, 2, true), SnapshotStep::Wait(SNAPSHOT_BUSY_RETRY_MS), "never during a drag");
        assert_eq!(s.frame(5_250.0, true, 2, false), SnapshotStep::Snapshot);
        s.posted(2, 10.0);
        // The page goes hidden right after an edit: the host asks, no rest needed.
        s.frame(6_000.0, true, 3, false);
        s.request();
        assert_eq!(s.frame(6_010.0, true, 3, false), SnapshotStep::Snapshot);
        s.posted(3, 10.0);
        // Asking when the copy is current, or the document saved, does nothing.
        s.request();
        assert_eq!(s.frame(6_020.0, true, 3, false), SnapshotStep::Idle);
        s.request();
        assert_eq!(s.frame(6_030.0, false, 3, false), SnapshotStep::Idle);
        assert_eq!(s.frame(6_040.0, true, 4, false), SnapshotStep::Wait(SNAPSHOT_QUIET_MS), "a request does not outlive a save");
    }

    #[test]
    fn a_slow_snapshot_stretches_the_rest_and_a_failed_one_waits_for_the_next_change() {
        let mut s = SnapshotSchedule::default();
        s.frame(0.0, true, 2, false);
        assert_eq!(s.frame(2_000.0, true, 2, false), SnapshotStep::Snapshot);
        s.posted(2, 1_000.0);
        assert_eq!(s.quiet_ms(), 3_000.0, "three times what the last one took");
        s.posted(2, 60_000.0);
        assert_eq!(s.quiet_ms(), SNAPSHOT_MAX_QUIET_MS, "capped");
        let mut s = SnapshotSchedule::default();
        s.frame(0.0, true, 5, false);
        assert_eq!(s.frame(2_000.0, true, 5, false), SnapshotStep::Snapshot);
        s.failed(5);
        assert_eq!(s.frame(9_000.0, true, 5, false), SnapshotStep::Idle, "not again for the same revision");
        assert_eq!(s.frame(9_100.0, true, 6, false), SnapshotStep::Wait(SNAPSHOT_QUIET_MS));
        // Saved, then unsaved again at the same revision (a copy reopened as unsaved changes): a
        // fresh rest, then a copy.
        s.frame(9_200.0, false, 6, false);
        assert_eq!(s.frame(9_300.0, true, 6, false), SnapshotStep::Wait(SNAPSHOT_QUIET_MS));
        assert_eq!(s.frame(11_300.0, true, 6, false), SnapshotStep::Snapshot);
    }

    /// A 700×300 raster document whose 3×2 tiles all differ.
    fn tiled_document() -> photocraft_doc::Document {
        use photocraft_doc::{ColorMode, Document, Layer, LayerContent, Rect, SampleType, Size, Surface};
        let mut doc = Document::new("copy", Size::new(700, 300), ColorMode::Rgb, SampleType::U8);
        let mut surface = Surface::new(doc.pixel_format());
        for ty in 0..2i32 {
            for tx in 0..3i32 {
                let shade = (ty * 3 + tx) as f32 / 8.0;
                let rect = Rect::from_xywh(tx * 256, ty * 256, (700 - tx * 256).min(256) as u32, (300 - ty * 256).min(256) as u32);
                let n = (rect.width() * rect.height()) as usize;
                surface.try_write_region(rect, &[shade, 0.4, 1.0 - shade, 1.0].repeat(n)).unwrap();
            }
        }
        doc.layers.push(Layer::new("pixels", LayerContent::Raster(surface)));
        doc
    }

    /// Runs `run` to completion with a clock that moves 1 ms each time it is read; the parts it
    /// posted, how many steps it took, and what it finished with.
    fn run_to_end(
        run: &mut SnapshotRun,
        cache: &photocraft_io::PcraftExportCache,
        known: &std::collections::HashSet<String>,
        budget: f64,
    ) -> (Vec<(String, Vec<u8>)>, u32, Finished) {
        let ticks = std::sync::atomic::AtomicU64::new(0);
        let now = || ticks.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as f64;
        let mut posted = Vec::new();
        let mut steps = 0;
        loop {
            steps += 1;
            assert!(steps < 10_000, "a copy must complete");
            match run.step(cache, known, budget, &now, &mut |path, data| posted.push((path.to_owned(), data))) {
                RunStep::Continue => {}
                RunStep::Done(done) => return (posted, steps, done),
                RunStep::Failed(e) => panic!("{e}"),
            }
        }
    }

    #[test]
    fn a_recovery_copy_is_written_in_small_steps_and_sends_only_what_the_host_lacks() {
        let mut doc = tiled_document();
        let cache = photocraft_io::PcraftExportCache::default();
        let mut run = SnapshotRun::new(std::sync::Arc::new(doc.clone()), 3);
        let (posted, steps, done) = run_to_end(&mut run, &cache, &Default::default(), SNAPSHOT_SLICE_MS);
        assert!(steps > 2, "hashing and six tiles do not fit one {SNAPSHOT_SLICE_MS} ms step: {steps} steps");
        assert_eq!(done.revision, 3);
        assert_eq!(done.parts, posted.len());
        assert_eq!(posted.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(), done.paths, "every part once, in bundle order");
        assert_eq!(done.slices, steps);
        assert!(done.work_ms >= done.max_slice_ms && done.max_slice_ms > 0.0, "{} ms over {} steps, longest {}", done.work_ms, steps, done.max_slice_ms);
        let bundle = photocraft_io::bundle_from_parts(&done.manifest, &posted).unwrap();
        assert_eq!(photocraft_format::load_from_bytes(&bundle).unwrap(), doc);

        // One pixel later: the host holds the first copy's parts, so only the edited tile is sent.
        doc.layers[0].surface_mut().unwrap().write_pixel(5, 5, &[1.0, 0.0, 0.0, 1.0]);
        let known: std::collections::HashSet<String> = done.paths.iter().cloned().collect();
        let mut again = SnapshotRun::new(std::sync::Arc::new(doc.clone()), 4);
        assert_eq!(again.revision(), 4);
        let (more, _, next) = run_to_end(&mut again, &cache, &known, SNAPSHOT_SLICE_MS);
        assert_eq!(more.len(), 1, "only the edited tile: {:?}", more.iter().map(|(p, _)| p).collect::<Vec<_>>());
        assert_eq!(next.parts, 1);
        let mut objects: Vec<_> = posted.into_iter().filter(|(p, _)| next.paths.contains(p)).collect();
        objects.extend(more);
        let bundle = photocraft_io::bundle_from_parts(&next.manifest, &objects).unwrap();
        assert_eq!(photocraft_format::load_from_bytes(&bundle).unwrap(), doc);
    }

    #[test]
    fn an_object_larger_than_one_step_still_gets_its_copy_written() {
        // A step budget far below what hashing one tile takes on this clock: the budget doubles
        // while a step gets no further, so the copy completes anyway.
        let doc = std::sync::Arc::new(tiled_document());
        let cache = photocraft_io::PcraftExportCache::default();
        let mut run = SnapshotRun::new(doc, 1);
        let (posted, _, done) = run_to_end(&mut run, &cache, &Default::default(), 0.5);
        assert_eq!(posted.len(), done.paths.len());
    }
}
