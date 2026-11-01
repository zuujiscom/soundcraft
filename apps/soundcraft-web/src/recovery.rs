//! Crash recovery in the browser: while the session has unsaved changes a copy is kept in
//! IndexedDB (craft-libs' `craft-web-recovery`: one entry per tab, held by a Web Lock so a second
//! tab leaves a running tab's copy alone, a write counted only once IndexedDB has committed it).
//! The desktop keeps its copies with a thread in the autosave folder (`soundcraft_engine::autosave`),
//! which the browser has neither of.
//!
//! The copy is the session bundle File > Save would download (`.scraftz`: the session and its
//! audio), because the browser has no files to point at: a session without its imported audio
//! would come back silent. Encoding runs on the page, so a copy is made at most every
//! [`EVERY_MS`], and a session whose bundle takes long to encode waits ten times that long. A
//! full browser store is reported on the status line and tried again at the next interval.
//!
//! Startup reads every copy before the app is built. The newest is offered ("Recover Unsaved
//! Work"); recovering it opens it as an unsaved session that keeps living under its entry, and
//! discarding it deletes it. A copy goes when its session is saved or replaced by a clean one.

use craft_web_recovery::{Opened, Recovered, Snapshot, Store};
use soundcraft_engine::{Engine, io};
use soundcraft_model::Session;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

/// The least time between two copies (milliseconds).
const EVERY_MS: f64 = 10_000.0;

/// What [`open`] found, for the app to be built with.
pub struct Startup {
    pub store: Store,
    pub recovered: Vec<Recovered>,
    pub warnings: Vec<String>,
}

pub async fn open() -> Startup {
    let Opened { store, recovered, warnings } = craft_web_recovery::open("soundcraft").await;
    Startup { store, recovered, warnings: warnings.iter().map(ToString::to_string).collect() }
}

/// Open `copy` as an unsaved session in `e` (like the desktop's Recover: the document differs
/// from the saved state, so it is modified until saved). Returns the media that was missing.
pub fn restore(e: &mut Engine, copy: &Recovered) -> Result<Vec<String>, String> {
    let mut scratch = Engine::default();
    let file = format!("{}.{}", copy.name, io::BUNDLE_EXTENSION);
    let missing = io::open_session_bytes(&mut scratch, &file, &copy.bytes).map_err(|err| err.to_string())?;
    let session: Session = scratch.session().clone();
    e.replace_session(Session::default());
    *e.session_mut() = session;
    e.path = scratch.path.take();
    Ok(missing)
}

/// The newest session handed over, for writing from the frame loop or when the tab is hidden.
struct Latest {
    session: Arc<Session>,
    revision: u64,
}

struct Inner {
    store: Store,
    /// The entry the open session lives under.
    key: Option<String>,
    latest: Option<Latest>,
    /// The revision last handed to the store.
    queued: Option<u64>,
    /// Earliest `now_ms` for the next copy.
    next_ms: f64,
    /// The last failure reported, so a full store is announced once, not every interval.
    reported: Option<String>,
    /// Counter for new entry keys.
    serial: u64,
    failure: Option<String>,
}

/// The copy of the open session, kept up to date once per frame and when the tab is hidden.
pub struct WebRecovery {
    inner: Rc<RefCell<Inner>>,
    /// Whether the session has unsaved changes, for the "Leave site?" question.
    dirty: Rc<Cell<bool>>,
    _guard: Option<craft_web_recovery::UnloadGuard>,
    _hidden: Option<craft_web_recovery::HiddenHook>,
}

impl WebRecovery {
    /// `adopted`: the entry of a session just recovered, which goes on living under it.
    pub fn new(store: Store, adopted: Option<String>) -> Self {
        let inner =
            Rc::new(RefCell::new(Inner { store, key: adopted, latest: None, queued: None, next_ms: 0.0, reported: None, serial: 0, failure: None }));
        let dirty = Rc::new(Cell::new(false));
        let flag = dirty.clone();
        let guard = craft_web_recovery::UnloadGuard::new(move || flag.get()).map_err(|e| log::warn!("unload guard: {e}")).ok();
        let hook = inner.clone();
        // Hidden tabs get no frames and may be discarded: write what is pending right now.
        let hidden = craft_web_recovery::HiddenHook::new(move || {
            if let Ok(mut i) = hook.try_borrow_mut() {
                i.write(true);
            }
        })
        .map_err(|e| log::warn!("hidden hook: {e}"))
        .ok();
        Self { inner, dirty, _guard: guard, _hidden: hidden }
    }

    /// The entry of a copy that was recovered into the open session.
    pub fn adopt(&mut self, key: String) {
        let mut i = self.inner.borrow_mut();
        i.forget();
        i.key = Some(key);
    }

    /// A failure to tell the user about, once.
    pub fn take_failure(&mut self) -> Option<String> {
        self.inner.borrow_mut().failure.take()
    }

    pub fn tick(&mut self, e: &Engine, ctx: &egui::Context) {
        let mut i = self.inner.borrow_mut();
        i.settle();
        let dirty = e.is_dirty();
        self.dirty.set(dirty);
        if !dirty {
            // Saved, closed or replaced by a clean session: nothing is left to recover.
            i.latest = None;
            i.forget();
            return;
        }
        i.latest = Some(Latest { session: e.session_arc(), revision: e.revision });
        if i.queued == Some(e.revision) {
            return;
        }
        let wait = i.next_ms - craft_web_recovery::now_ms();
        if wait > 0.0 {
            ctx.request_repaint_after(std::time::Duration::from_millis(wait as u64 + 1));
            return;
        }
        i.write(false);
    }
}

impl Inner {
    /// Collect finished writes: a failed one is tried again at the next interval.
    fn settle(&mut self) {
        for w in self.store.take_written() {
            if self.key.as_deref() != Some(&w.key) {
                continue;
            }
            match w.result {
                Ok(()) => self.reported = None,
                Err(e) => {
                    self.queued = None;
                    self.fail(e);
                }
            }
        }
    }

    fn fail(&mut self, e: String) {
        log::warn!("recovery copy not written: {e}");
        if self.reported.as_deref() != Some(&e) {
            self.failure = Some(format!("Crash-recovery copy failed (browser storage full or unavailable?): {e}"));
            self.reported = Some(e);
        }
    }

    fn write(&mut self, force: bool) {
        let Some(Latest { session, revision }) = self.latest.as_ref().map(|l| Latest { session: Arc::clone(&l.session), revision: l.revision })
        else {
            return;
        };
        if self.queued == Some(revision) || (!force && self.next_ms > craft_web_recovery::now_ms()) {
            return;
        }
        let started = craft_web_recovery::now_ms();
        let bytes = match io::session_bundle(&session) {
            Ok((bytes, _)) => bytes,
            Err(err) => {
                self.next_ms = started + EVERY_MS;
                return self.fail(err.to_string());
            }
        };
        let key = match &self.key {
            Some(k) => k.clone(),
            None => {
                self.serial += 1;
                let k = self.store.key(self.serial);
                self.key = Some(k.clone());
                k
            }
        };
        match self.store.put(&key, revision, Snapshot { name: &session.name, path: None, meta: None, bytes: &bytes }) {
            Ok(()) => self.queued = Some(revision),
            Err(e) => self.fail(e),
        }
        let took = craft_web_recovery::now_ms() - started;
        self.next_ms = started + EVERY_MS.max(took * 10.0);
    }

    /// The open session has nothing unsaved (or is gone): its copy is deleted.
    fn forget(&mut self) {
        if let Some(key) = self.key.take() {
            self.store.remove(&key);
        }
        self.queued = None;
        self.reported = None;
    }

    /// Delete a copy the user declined.
    fn discard(&self, key: &str) {
        self.store.remove(key);
    }
}

impl WebRecovery {
    pub fn discard(&self, key: &str) {
        self.inner.borrow().discard(key);
    }
}

/// The offer of a copy a closed or crashed tab left behind.
pub enum Choice {
    Recover,
    Discard,
}

pub fn offer(ctx: &egui::Context, copy: &Recovered, recording: bool) -> Option<Choice> {
    let mut choice = None;
    egui::Window::new(egui::RichText::new("Recover Unsaved Work").strong())
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, -80.0))
        .show(ctx, |ui| {
            let secs = ((craft_web_recovery::now_ms() - copy.saved_at) / 1000.0).max(0.0) as u64;
            let age = match secs {
                0..60 => "less than a minute ago".to_string(),
                60..3_600 => format!("{} min ago", secs / 60),
                3_600..86_400 => format!("{} h ago", secs / 3_600),
                _ => format!("{} d ago", secs / 86_400),
            };
            ui.label(format!("“{}” has unsaved changes from an earlier visit, kept in this browser {age}.", copy.name));
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Discard").clicked() {
                    choice = Some(Choice::Discard);
                }
                if ui.add_enabled(!recording, egui::Button::new(egui::RichText::new("Recover").strong())).clicked() {
                    choice = Some(Choice::Recover);
                }
            });
        });
    choice
}
