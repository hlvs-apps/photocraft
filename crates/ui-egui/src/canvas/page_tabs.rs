//! Page tabs (Window › Pages): a document whose pages are artboards shows one tab per artboard,
//! in page order, instead of one tab for the whole canvas. Every page tab is a view of the same
//! document ([`PageTabs`]) with its own zoom and pan, fitted to its artboard, that draws only
//! that artboard (`canvas_view`). Saving, undo and the unsaved state stay the document's, so a
//! save from any page tab is the save of the one document; closing a page tab closes only that
//! view, never the page. The engine keeps a page tab's edits on its page
//! ([`photocraft_engine::DocState::page`]).
//!
//! The order of the pages is the order of the artboards in the document (bottom to top), the
//! order a save maps them to pages in; moving a page tab moves its artboard.

use photocraft_doc::LayerId;
use serde_json::{Value, json};

use crate::PhotocraftApp;
use crate::state::{PageTabs, View};

/// Window › Pages: one row per page, `window.page.<artboard id>`, checked while its tab is open.
pub const PAGE_ITEM: &str = "window.page.";

/// Window › Pages › Artboards as Tabs: the active document's artboards as page tabs, or the
/// whole canvas in one tab.
pub const AS_TABS: &str = "window.pages.asTabs";

/// The shell commands of Window › Pages.
const COMMANDS: [&str; 8] = [
    AS_TABS,
    "window.pages.next",
    "window.pages.previous",
    "window.pages.moveLeft",
    "window.pages.moveRight",
    "window.pages.close",
    "window.pages.closeOthers",
    "window.pages.show",
];

/// A tab on the document tab strip: a document, or one page of a document shown as page tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tab {
    /// The document's index.
    pub doc: usize,
    /// The artboard (layer id) a page tab shows; `None` for a document's own tab.
    pub page: Option<u64>,
}

impl Tab {
    pub fn document(doc: usize) -> Self {
        Self { doc, page: None }
    }
}

fn entry_index(app: &PhotocraftApp, doc: usize) -> Option<usize> {
    let id = app.session.documents().get(doc)?.doc.id.0;
    app.ui.page_tabs.iter().position(|p| p.document == id)
}

/// Document `doc`'s page tab state, while Artboards as Tabs is on for it.
pub fn entry(app: &PhotocraftApp, doc: usize) -> Option<&PageTabs> {
    entry_index(app, doc).and_then(|k| app.ui.page_tabs.get(k))
}

fn entry_mut(app: &mut PhotocraftApp, doc: usize) -> Option<&mut PageTabs> {
    let k = entry_index(app, doc)?;
    app.ui.page_tabs.get_mut(k)
}

/// The artboards of document `doc` (its pages), in page order.
fn boards(app: &PhotocraftApp, doc: usize) -> Vec<u64> {
    app.session.documents().get(doc).map(|st| st.doc.artboards().iter().map(|b| b.0.0).collect()).unwrap_or_default()
}

/// Is document `doc` shown as page tabs: Artboards as Tabs is on for it and it has artboards?
/// (Without artboards it shows its whole canvas in one tab until an artboard comes back.)
pub fn paged(app: &PhotocraftApp, doc: usize) -> bool {
    entry(app, doc).is_some() && app.session.documents().get(doc).is_some_and(|st| st.doc.has_artboards())
}

/// Document `doc`'s pages in page order: artboard id, name, and whether its tab is open. Empty
/// unless the document is shown as page tabs.
pub fn pages(app: &PhotocraftApp, doc: usize) -> Vec<(u64, String, bool)> {
    let (Some(e), Some(st)) = (entry(app, doc), app.session.documents().get(doc)) else { return Vec::new() };
    st.doc.artboards().iter().map(|(id, name, _)| (id.0, (*name).to_string(), !e.closed.contains(&id.0))).collect()
}

/// The pages of document `doc` whose tabs are open, in page order.
pub fn open_pages(app: &PhotocraftApp, doc: usize) -> Vec<u64> {
    pages(app, doc).into_iter().filter(|p| p.2).map(|p| p.0).collect()
}

/// A page's name: its artboard's.
pub fn page_name(app: &PhotocraftApp, doc: usize, page: u64) -> Option<String> {
    app.session.documents().get(doc)?.doc.layer(LayerId(page)).filter(|l| l.artboard().is_some()).map(|l| l.name.clone())
}

/// Every tab of the strip, left to right: the documents in order, a document shown as page tabs
/// by its open pages in page order.
pub fn strip(app: &PhotocraftApp) -> Vec<Tab> {
    let mut tabs = Vec::with_capacity(app.session.documents().len());
    for doc in 0..app.session.documents().len() {
        if paged(app, doc) {
            tabs.extend(open_pages(app, doc).into_iter().map(|p| Tab { doc, page: Some(p) }));
        } else {
            tabs.push(Tab::document(doc));
        }
    }
    tabs
}

/// The active tab: the active document, on its active page when it shows page tabs.
pub fn active(app: &PhotocraftApp) -> Option<Tab> {
    let doc = app.session.active_index()?;
    let page = if paged(app, doc) { entry(app, doc).and_then(|e| e.active) } else { None };
    Some(Tab { doc, page })
}

/// The camera of tab `tab`: the document's view for its own tab and its active page, the
/// page's own for another page (a fresh one, fitted to the page, before it was ever shown).
pub fn view_of(app: &PhotocraftApp, tab: Tab) -> View {
    let current = app.ui.views.get(tab.doc).cloned().unwrap_or_default();
    match tab.page {
        Some(p) if current.page != Some(p) => entry(app, tab.doc)
            .and_then(|e| e.views.iter().find(|(b, _)| *b == p).map(|(_, v)| v.clone()))
            .unwrap_or_else(|| View { fit_pending: true, page: Some(p), ..View::default() }),
        _ => current,
    }
}

/// Keep `view` as tab `tab`'s camera (see [`view_of`]).
pub fn store_view(app: &mut PhotocraftApp, tab: Tab, view: View) {
    let current = app.ui.views.get(tab.doc).map(|v| v.page);
    match tab.page {
        Some(p) if current != Some(Some(p)) => {
            if let Some(e) = entry_mut(app, tab.doc) {
                e.views.retain(|(b, _)| *b != p);
                e.views.push((p, View { page: Some(p), ..view }));
            }
        }
        _ => {
            if let Some(v) = app.ui.views.get_mut(tab.doc) {
                *v = view;
            }
        }
    }
}

/// Show tab `tab`: its document becomes the active one and, for a page tab, that page the
/// document's active page (its tab opens if it was closed).
pub fn activate(app: &mut PhotocraftApp, tab: Tab) {
    app.session.set_active(tab.doc);
    app.jobs.focus = None;
    if let Some(p) = tab.page
        && let Some(e) = entry_mut(app, tab.doc)
    {
        e.closed.retain(|c| *c != p);
        e.active = Some(p);
    }
    sync_doc(app, tab.doc);
}

/// Artboards as Tabs on or off for document `doc`. On, the page of the active layer's artboard
/// (else the first page) is shown, fitted; off, the whole canvas again. Turning it on for a
/// document without artboards fails.
pub fn set_paged(app: &mut PhotocraftApp, doc: usize, on: bool) -> Result<bool, String> {
    page_tabs_on(app, doc, on, false)
}

/// [`set_paged`]; `first_page`: show the first page rather than the active layer's (a document
/// that has just opened starts on its first page).
fn page_tabs_on(app: &mut PhotocraftApp, doc: usize, on: bool, first_page: bool) -> Result<bool, String> {
    let st = app.session.documents().get(doc).ok_or("no document")?;
    let id = st.doc.id.0;
    let shown = entry_index(app, doc).is_some();
    if on && !shown {
        let boards: Vec<u64> = st.doc.artboards().iter().map(|b| b.0.0).collect();
        let of_layer = st.active_layer.and_then(|l| st.doc.artboard_of(l)).map(|b| b.0).filter(|_| !first_page);
        let first = of_layer.or_else(|| boards.first().copied());
        let Some(first) = first else { return Err(tl!("The document has no artboards to show as page tabs.").into()) };
        app.ui.page_tabs.push(PageTabs { document: id, active: Some(first), seen: boards, ..PageTabs::default() });
    } else if !on && shown {
        app.ui.page_tabs.retain(|p| p.document != id);
    }
    sync_doc(app, doc);
    Ok(on)
}

/// Close page `page`'s tab of document `doc`: the page stays (Window › Pages opens it again),
/// and so does the document. The last open page tab stays open: closing it would leave the
/// document without a view, so it fails (File › Close closes the document).
pub fn close_page(app: &mut PhotocraftApp, doc: usize, page: u64) -> Result<(), String> {
    let open = open_pages(app, doc);
    let at = open.iter().position(|p| *p == page).ok_or("no such page tab is open")?;
    if open.len() == 1 {
        return Err(tl!("The last page tab stays open. File › Close closes the document.").into());
    }
    let next = open.get(at + 1).or_else(|| at.checked_sub(1).and_then(|i| open.get(i))).copied();
    if let Some(e) = entry_mut(app, doc) {
        e.closed.push(page);
        if e.active == Some(page) {
            e.active = next;
        }
    }
    sync_doc(app, doc);
    Ok(())
}

/// Move page `page` of document `doc` so it comes just before page `before`, or after every page
/// (`None`): its artboard moves among the document's artboards (`layer.moveTo`), one undoable
/// step that changes the order a save maps the pages in. Returns whether anything moved.
pub fn move_page(app: &mut PhotocraftApp, doc: usize, page: u64, before: Option<u64>) -> Result<bool, String> {
    let all = boards(app, doc);
    let from = all.iter().position(|b| *b == page).ok_or("no such page")?;
    let (target, position) = match before {
        Some(b) if b == page => return Ok(false),
        Some(b) => {
            let to = all.iter().position(|x| *x == b).ok_or("no such page")?;
            if to == from + 1 {
                return Ok(false);
            }
            (b, "below")
        }
        None => {
            let Some(&last) = all.last().filter(|l| **l != page) else { return Ok(false) };
            (last, "above")
        }
    };
    let active = app.session.active_index();
    app.session.set_active(doc);
    let r = app.run("layer.moveTo", json!({"layer": page, "target": target, "position": position}));
    if let Some(a) = active {
        app.session.set_active(a);
    }
    r.map(|_| true)
}

/// Move the active page one place left (`-1`) or right (`1`) among every page.
fn step_page(app: &mut PhotocraftApp, delta: isize) -> Result<Value, String> {
    let tab = active(app).filter(|t| t.page.is_some()).ok_or("the document is not shown as page tabs")?;
    let page = tab.page.ok_or("no page")?;
    let all = boards(app, tab.doc);
    let at = all.iter().position(|b| *b == page).ok_or("no such page")?;
    let before = match delta {
        d if d < 0 => match at.checked_sub(1).and_then(|i| all.get(i)) {
            Some(&b) => Some(b),
            None => return Err(tl!("The page is already the first.").into()),
        },
        _ => match (all.get(at + 1), all.get(at + 2)) {
            (None, _) => return Err(tl!("The page is already the last.").into()),
            (Some(_), next) => next.copied(),
        },
    };
    move_page(app, tab.doc, page, before)?;
    Ok(json!({"page": page, "pages": boards(app, tab.doc)}))
}

/// Can tab `tab` be closed from the strip? A document tab can (File › Close); a page tab while
/// another page tab of its document stays open.
pub fn closable(app: &PhotocraftApp, tab: Tab) -> bool {
    tab.page.is_none() || open_pages(app, tab.doc).len() > 1
}

/// The page after `page` in document `doc` (every page, open or not), `None` after the last.
pub fn page_after(app: &PhotocraftApp, doc: usize, page: u64) -> Option<u64> {
    let all = boards(app, doc);
    let at = all.iter().position(|b| *b == page)?;
    all.get(at + 1).copied()
}

/// A page in its own window (a page tab torn off the strip): a view of the document showing
/// only that page, like Window › Arrange › New Window for Document. Returns the window's id.
pub fn float(app: &mut PhotocraftApp, tab: Tab, position: Option<[f32; 2]>) -> u64 {
    let mut view = view_of(app, tab);
    view.zoom_animating = false;
    view.zoom_anchor = None;
    view.zoom_transition = None;
    view.display_camera = None;
    let id = app.ui.alloc_id();
    app.ui.windows.push(crate::state::DocWindow { id, document: tab.doc, view, open: true, position });
    id
}

/// The rectangle a page view ([`View::page`]) shows of `doc`: its artboard's, on the canvas.
/// `None` for a view of the whole canvas, or when that artboard is gone or off the canvas.
pub fn page_rect(view: &View, doc: &photocraft_doc::Document) -> Option<photocraft_geom::Rect> {
    let page = LayerId(view.page?);
    let board = doc.layers.iter().find(|l| l.id == page)?.artboard()?.rect.intersect(&doc.bounds());
    (!board.is_empty()).then_some(board)
}

/// Keep every document's page tabs in step with the document (once a frame and after every
/// command, from `sync_views`): pages that are gone drop out, a new artboard opens and activates
/// its tab, the active page's camera is the document's view, and the engine knows the page.
/// `opened`: documents that opened since the last call; with `app.artboards_as_tabs` (the web
/// embed) a document with artboards opens as page tabs.
pub fn sync(app: &mut PhotocraftApp, opened: &[photocraft_doc::DocId]) {
    let ids: Vec<u64> = app.session.documents().iter().map(|st| st.doc.id.0).collect();
    app.ui.page_tabs.retain(|p| ids.contains(&p.document));
    if app.artboards_as_tabs {
        for d in opened {
            if let Some(doc) = app.session.documents().iter().position(|st| st.doc.id == *d)
                && app.session.documents().get(doc).is_some_and(|st| st.doc.has_artboards())
            {
                let _ = page_tabs_on(app, doc, true, true);
            }
        }
    }
    for doc in 0..ids.len() {
        sync_doc(app, doc);
    }
}

/// [`sync`] for one document.
fn sync_doc(app: &mut PhotocraftApp, doc: usize) {
    let all = boards(app, doc);
    let Some(k) = entry_index(app, doc).filter(|_| !all.is_empty()) else {
        // The whole canvas: a page view left behind (Artboards as Tabs turned off, the last
        // artboard deleted) shows the canvas again, fitted.
        if let Some(v) = app.ui.views.get_mut(doc)
            && v.page.is_some()
        {
            *v = View::default();
        }
        if let Some(e) = entry_mut(app, doc) {
            e.active = None;
            e.seen.clear();
        }
        if app.session.documents().get(doc).is_some_and(|st| st.page.is_some()) {
            app.session.set_page(doc, None);
        }
        return;
    };
    let ui = &mut app.ui;
    let Some(e) = ui.page_tabs.get_mut(k) else { return };
    let previous = std::mem::take(&mut e.seen);
    e.closed.retain(|b| all.contains(b));
    e.views.retain(|(b, _)| all.contains(b));
    e.layers.retain(|(b, _)| all.contains(b));
    // A new artboard (not one the document had when last seen) is a new page: open and show it.
    if !previous.is_empty()
        && let Some(&new) = all.iter().rev().find(|b| !previous.contains(b))
    {
        e.closed.retain(|c| *c != new);
        e.active = Some(new);
    }
    e.seen = all.clone();
    let mut open: Vec<u64> = all.iter().copied().filter(|b| !e.closed.contains(b)).collect();
    if open.is_empty() {
        // At least one page tab stays open: the document must never be left without a view.
        let keep = e.active.filter(|a| all.contains(a)).unwrap_or(all[0]);
        e.closed.retain(|c| *c != keep);
        open.push(keep);
    }
    if e.active.is_none_or(|a| !open.contains(&a)) {
        // The active page is gone (deleted, or its artboard ungrouped): the page that followed
        // it, else the last one.
        let at = e.active.and_then(|a| previous.iter().position(|b| *b == a)).unwrap_or(0);
        let next = open.iter().copied().find(|b| all.iter().position(|x| x == b).is_some_and(|i| i >= at)).or_else(|| open.last().copied());
        e.active = next;
    }
    let Some(active) = e.active else { return };
    let Some(view) = ui.views.get_mut(doc) else { return };
    let switched = view.page != Some(active);
    let left = view.page;
    if switched {
        let mut shown = e.views.iter().position(|(b, _)| *b == active).map_or_else(|| View { fit_pending: true, ..View::default() }, |i| e.views.remove(i).1);
        shown.page = Some(active);
        let before = std::mem::replace(view, shown);
        if let Some(old) = left.filter(|o| all.contains(o)) {
            e.views.retain(|(b, _)| *b != old);
            e.views.push((old, before));
        }
    }
    let page = LayerId(active);
    if app.session.documents().get(doc).is_some_and(|st| st.page != Some(page)) {
        app.session.set_page(doc, Some(page));
    }
    if app.session.active_index() == Some(doc) {
        keep_active_layer_on_page(app, doc, page, left.filter(|_| switched));
    }
}

/// The active document's active layer belongs to the page shown: one outside it (another page's,
/// or a loose layer's) gives way to the layer the page had active last, else its top layer, else
/// its artboard. `left`: the page just left, whose active layer is remembered. No layer active
/// stays so.
fn keep_active_layer_on_page(app: &mut PhotocraftApp, doc: usize, page: LayerId, left: Option<u64>) {
    let Some(st) = app.session.documents().get(doc) else { return };
    let Some(current) = st.active_layer else { return };
    let on_page = |id: LayerId| st.doc.artboard_of(id) == Some(page);
    if on_page(current) {
        return;
    }
    let remembered = entry(app, doc).and_then(|e| e.layers.iter().find(|(b, _)| *b == page.0).map(|(_, l)| LayerId(*l))).filter(|l| on_page(*l));
    let top = st.doc.layer(page).and_then(|b| b.children()).and_then(|c| c.last()).map(|l| l.id);
    let want = remembered.or(top).unwrap_or(page);
    let left_layer = left.filter(|o| st.doc.artboard_of(current) == Some(LayerId(*o))).map(|o| (o, current.0));
    if let (Some((o, l)), Some(e)) = (left_layer, entry_mut(app, doc)) {
        e.layers.retain(|(b, _)| *b != o);
        e.layers.push((o, l));
    }
    let _ = photocraft_engine::layer_multi_cmds::set_selection(&mut app.session, vec![want], Some(want), Some(want));
}

/// Does the Layers panel list layer `id` of the active document? A page tab lists only its
/// page: the artboard and everything in it.
pub fn panel_shows(app: &PhotocraftApp, doc: &photocraft_doc::Document, id: LayerId) -> bool {
    let Some(page) = app.session.active_index().and_then(|i| active(app).filter(|t| t.doc == i)).and_then(|t| t.page) else { return true };
    doc.artboard_of(id) == Some(LayerId(page))
}

/// Is `id` one of the Window › Pages commands (or a page row)?
pub fn handles(id: &str) -> bool {
    COMMANDS.contains(&id) || id.starts_with(PAGE_ITEM)
}

/// Window › Pages' check marks: Artboards as Tabs, and each page row while its tab is open.
pub fn checked(app: &PhotocraftApp, id: &str) -> Option<bool> {
    if id == AS_TABS {
        return Some(app.session.active_index().is_some_and(|d| entry(app, d).is_some()));
    }
    let page = id.strip_prefix(PAGE_ITEM)?.parse::<u64>().ok()?;
    let doc = app.session.active_index()?;
    Some(pages(app, doc).iter().any(|p| p.0 == page && p.2))
}

/// Whether a Window › Pages command can run now.
pub fn enabled(app: &PhotocraftApp, id: &str) -> Option<bool> {
    if !handles(id) {
        return None;
    }
    let Some(doc) = app.session.active_index() else { return Some(false) };
    let has_boards = app.session.documents().get(doc).is_some_and(|st| st.doc.has_artboards());
    Some(match id {
        AS_TABS => has_boards || entry(app, doc).is_some(),
        "window.pages.next" | "window.pages.previous" | "window.pages.closeOthers" | "window.pages.close" => paged(app, doc) && open_pages(app, doc).len() > 1,
        "window.pages.moveLeft" | "window.pages.moveRight" => paged(app, doc) && boards(app, doc).len() > 1,
        _ => paged(app, doc),
    })
}

/// The document a command names (`"document"`: an index) or the active one.
fn doc_param(app: &PhotocraftApp, p: &Value) -> Result<usize, String> {
    match p.get("document") {
        None | Some(Value::Null) => app.session.active_index().ok_or_else(|| "no document".to_string()),
        Some(v) => v
            .as_u64()
            .and_then(|i| usize::try_from(i).ok())
            .filter(|i| *i < app.session.documents().len())
            .ok_or_else(|| "document must be the index of an open document".to_string()),
    }
}

/// The page a command names (`"page"`: an artboard's layer id) or the active one.
fn page_param(app: &PhotocraftApp, doc: usize, p: &Value) -> Result<u64, String> {
    match p.get("page") {
        None | Some(Value::Null) => {
            entry(app, doc).and_then(|e| e.active).filter(|_| paged(app, doc)).ok_or_else(|| "the document is not shown as page tabs".to_string())
        }
        Some(v) => {
            v.as_u64().filter(|id| boards(app, doc).contains(id)).ok_or_else(|| "page must be the layer id of one of the document's artboards".to_string())
        }
    }
}

/// Run a Window › Pages command. Params: `document` (index, default the active one), `page`
/// (artboard layer id, default the active page), `on` (Artboards as Tabs; default: toggle).
pub fn invoke(app: &mut PhotocraftApp, id: &str, p: &Value) -> Option<Result<Value, String>> {
    if !handles(id) {
        return None;
    }
    Some((|| {
        let doc = doc_param(app, p)?;
        if let Some(page) = id.strip_prefix(PAGE_ITEM) {
            let page = page.parse::<u64>().ok().filter(|b| boards(app, doc).contains(b)).ok_or("no such page")?;
            return show(app, doc, page);
        }
        match id {
            AS_TABS => {
                let on = p.get("on").and_then(Value::as_bool).unwrap_or_else(|| entry(app, doc).is_none());
                let on = set_paged(app, doc, on)?;
                Ok(json!({"pageTabs": on, "pages": pages_json(app, doc)}))
            }
            "window.pages.show" => {
                let page = page_param(app, doc, p)?;
                show(app, doc, page)
            }
            "window.pages.next" | "window.pages.previous" => {
                let open = open_pages(app, doc);
                let page = page_param(app, doc, p)?;
                let at = open.iter().position(|b| *b == page).ok_or("no such page tab is open")?;
                let n = open.len();
                let to = if id == "window.pages.next" { (at + 1) % n } else { (at + n - 1) % n };
                let next = *open.get(to).ok_or("no page")?;
                show(app, doc, next)
            }
            "window.pages.moveLeft" | "window.pages.moveRight" => {
                app.session.set_active(doc);
                let page = page_param(app, doc, p)?;
                activate(app, Tab { doc, page: Some(page) });
                step_page(app, if id == "window.pages.moveLeft" { -1 } else { 1 })
            }
            "window.pages.close" => {
                let page = page_param(app, doc, p)?;
                close_page(app, doc, page)?;
                Ok(json!({"closed": page, "pages": pages_json(app, doc)}))
            }
            "window.pages.closeOthers" => {
                let page = page_param(app, doc, p)?;
                activate(app, Tab { doc, page: Some(page) });
                for other in open_pages(app, doc).into_iter().filter(|o| *o != page) {
                    close_page(app, doc, other)?;
                }
                Ok(json!({"pages": pages_json(app, doc)}))
            }
            _ => Err(format!("unknown page command {id}")),
        }
    })())
}

fn show(app: &mut PhotocraftApp, doc: usize, page: u64) -> Result<Value, String> {
    if !paged(app, doc) {
        set_paged(app, doc, true)?;
    }
    activate(app, Tab { doc, page: Some(page) });
    Ok(json!({"page": page, "pages": pages_json(app, doc)}))
}

/// The pages for automation: `[{page, name, open, active}]`.
pub fn pages_json(app: &PhotocraftApp, doc: usize) -> Value {
    let active = entry(app, doc).and_then(|e| e.active);
    Value::Array(
        pages(app, doc).into_iter().map(|(page, name, open)| json!({"page": page, "name": name, "open": open, "active": active == Some(page)})).collect(),
    )
}

/// The Window › Pages rows of the active document's pages, after the commands.
pub fn menu_rows(app: &PhotocraftApp) -> Vec<crate::menus::MenuItem> {
    let Some(doc) = app.session.active_index().filter(|d| paged(app, *d)) else { return Vec::new() };
    let path = vec!["Window".to_string(), "Pages".to_string()];
    let separator =
        crate::menus::MenuItem { id: "---".into(), label: "---".into(), path: path.clone(), shortcut: None, enabled: false, checked: None, color: None };
    std::iter::once(separator)
        .chain(pages(app, doc).into_iter().map(|(page, name, open)| crate::menus::MenuItem {
            id: format!("{PAGE_ITEM}{page}"),
            label: name,
            path: path.clone(),
            shortcut: None,
            enabled: true,
            checked: Some(open),
            color: None,
        }))
        .collect()
}

#[cfg(test)]
#[path = "page_tabs_tests.rs"]
mod tests;
