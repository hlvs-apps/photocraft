//! Page tabs: one tab per artboard, each a view of the one document (whole app, real tree).

use std::cell::RefCell;
use std::rc::Rc;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use photocraft_doc::LayerId;
use serde_json::{Value, json};

use super::*;
use crate::{PhotocraftApp, Services, menus};

/// A design as the server opens it in the GUI: three 100 × 50 pages side by side, 100 px apart,
/// each an artboard with one filled layer ("Magenta", "Green", "Yellow"), as `.pcraft` bytes.
fn design() -> Vec<u8> {
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", json!({"width": 100, "height": 50, "artboard": true, "background": "white", "name": "Design"})).unwrap();
    let first = s.active().unwrap().doc.artboards()[0].0;
    s.execute("layer.renameLayer", json!({"layer": first.0, "name": "Page 1"})).unwrap();
    for (x, name) in [(200, "Page 2"), (400, "Page 3")] {
        s.execute("layer.new.artboard", json!({"rect": [x, 0, 100, 50], "name": name, "background": "white"})).unwrap();
        s.execute("layer.new.layer", json!({})).unwrap();
    }
    let boards: Vec<LayerId> = s.active().unwrap().doc.artboards().iter().map(|b| b.0).collect();
    for (board, (name, color)) in boards.iter().zip([("Magenta", "#ff00ff"), ("Green", "#00ff00"), ("Yellow", "#ffff00")]) {
        let layer = s.active().unwrap().doc.layer(*board).unwrap().children().unwrap()[0].id;
        s.execute("layer.select", json!({"layer": layer.0})).unwrap();
        s.execute("layer.renameLayer", json!({"layer": layer.0, "name": name})).unwrap();
        let r = s.active().unwrap().doc.layer(*board).unwrap().artboard().unwrap().rect;
        s.execute("select.rect", json!({"x": r.x0 + 10, "y": 10, "width": 80, "height": 30})).unwrap();
        s.execute("edit.fill", json!({"color": color})).unwrap();
        s.execute("select.deselect", json!({})).unwrap();
    }
    photocraft_format::save_to_bytes(&s.active().unwrap().doc, &Default::default()).unwrap()
}

/// What File › Save wrote last.
type Written = Rc<RefCell<Option<Vec<u8>>>>;

/// An app that saves `.pcraft` into `written`, as the web embed's writer does.
fn app(as_tabs: bool) -> (PhotocraftApp, Written) {
    let written: Written = Rc::default();
    let w = written.clone();
    let mut app = PhotocraftApp::new(
        photocraft_engine::Session::new(),
        Services {
            import: Some(Box::new(|name, bytes, opts| {
                photocraft_io::import_with_options(name, bytes, opts).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
            })),
            export: Some(Box::new(|doc, path, settings| {
                photocraft_io::export(doc, path, &settings.io_options()).map(|r| (r.bytes, r.warnings)).map_err(|e| e.to_string())
            })),
            write: Some(Box::new(move |_, data| {
                *w.borrow_mut() = Some(data.to_vec());
                Ok(())
            })),
            ..Default::default()
        },
    );
    app.background_jobs = false;
    app.artboards_as_tabs = as_tabs;
    (app, written)
}

/// [`app`] with the [`design`] open, as the embed opens the host's document.
fn opened(as_tabs: bool) -> (PhotocraftApp, Written) {
    opened_from(as_tabs, &design())
}

/// [`opened`] with these design bytes.
fn opened_from(as_tabs: bool, bytes: &[u8]) -> (PhotocraftApp, Written) {
    let (mut app, written) = app(as_tabs);
    app.open_bytes_from("Design.pcraft", bytes, Some("Design.pcraft")).unwrap();
    app.sync_views();
    assert_eq!(app.session.documents().len(), 1, "the design opened");
    (app, written)
}

fn boards(app: &PhotocraftApp) -> Vec<u64> {
    app.session.active().unwrap().doc.artboards().iter().map(|b| b.0.0).collect()
}

fn layer(app: &PhotocraftApp, name: &str) -> LayerId {
    app.session.active().unwrap().doc.walk().into_iter().map(|(_, _, l)| l).find(|l| l.name == name).unwrap().id
}

fn pages_of(tabs: &[Tab]) -> Vec<Option<u64>> {
    tabs.iter().map(|t| t.page).collect()
}

fn invoke(app: &mut PhotocraftApp, id: &str, p: Value) -> Result<Value, String> {
    menus::invoke(app, &egui::Context::default(), id, p)
}

fn save(app: &mut PhotocraftApp, written: &Written) -> Vec<u8> {
    *written.borrow_mut() = None;
    invoke(app, "file.save", json!({})).unwrap();
    written.borrow_mut().take().expect("File › Save wrote the document")
}

#[test]
fn a_design_opens_as_one_page_tab_per_artboard_in_page_order() {
    let (app, _) = opened(true);
    let b = boards(&app);
    assert_eq!(pages_of(&strip(&app)), b.iter().map(|x| Some(*x)).collect::<Vec<_>>());
    assert_eq!(active(&app), Some(Tab { doc: 0, page: Some(b[0]) }), "the first page shows first");
    // The active page's camera is the document's view; the engine knows the page.
    assert_eq!(app.ui.views[0].page, Some(b[0]));
    assert_eq!(app.session.active().unwrap().page, Some(LayerId(b[0])));
    let names: Vec<String> = strip(&app).iter().map(|t| super::super::tabs::tab_name(&app, *t)).collect();
    assert_eq!(names, ["Page 1", "Page 2", "Page 3"]);
    assert!(super::super::tabs::tab_title(&app, strip(&app)[1]).starts_with("Page 2 @ "), "titled with the page");
}

#[test]
fn without_the_switch_a_design_still_opens_as_one_canvas_tab() {
    let (mut app, _) = opened(false);
    assert_eq!(strip(&app), vec![Tab::document(0)]);
    assert_eq!(app.ui.views[0].page, None);
    assert_eq!(app.session.active().unwrap().page, None);
    // Window › Pages › Artboards as Tabs turns it on for the document, and off again.
    assert_eq!(checked(&app, AS_TABS), Some(false));
    invoke(&mut app, AS_TABS, json!({})).unwrap();
    assert_eq!(strip(&app).len(), 3);
    assert_eq!(checked(&app, AS_TABS), Some(true));
    invoke(&mut app, AS_TABS, json!({})).unwrap();
    assert_eq!(strip(&app), vec![Tab::document(0)]);
    assert_eq!((app.ui.views[0].page, app.session.active().unwrap().page), (None, None));
}

#[test]
fn a_single_canvas_document_is_unchanged() {
    let (mut app, _) = app(true);
    app.run("file.new", json!({"width": 64, "height": 32, "name": "Flat"})).unwrap();
    assert_eq!(strip(&app), vec![Tab::document(0)]);
    assert_eq!(active(&app), Some(Tab::document(0)));
    assert_eq!(app.ui.views[0].page, None);
    assert_eq!(super::super::tabs::tab_title(&app, Tab::document(0)), super::super::tabs::pro_tab_title(&app, 0));
    assert!(invoke(&mut app, AS_TABS, json!({"on": true})).is_err(), "no artboards, no page tabs");
    assert!(!menus::is_enabled(&app, AS_TABS));
    assert_eq!(enabled(&app, AS_TABS), Some(false));
}

#[test]
fn saving_from_any_page_tab_writes_the_document_as_saving_the_canvas_does() {
    // The same edit on page 2, saved from the whole-canvas tab of today...
    let bytes = design();
    let (mut canvas, canvas_written) = opened_from(false, &bytes);
    let green = layer(&canvas, "Green");
    canvas.run("layer.select", json!({"layer": green.0})).unwrap();
    canvas.run("edit.fill", json!({"color": "#0000ff"})).unwrap();
    let expected = save(&mut canvas, &canvas_written);
    // ...and from each page tab: the same bytes.
    let (mut paged, written) = opened_from(true, &bytes);
    let b = boards(&paged);
    assert_eq!(layer(&paged, "Green"), green, "the same document, with the same layers");
    activate(&mut paged, Tab { doc: 0, page: Some(b[1]) });
    assert_eq!(paged.session.active().unwrap().active_layer, Some(green), "page 2's layer is active on page 2");
    paged.run("edit.fill", json!({"color": "#0000ff"})).unwrap();
    // Unsaved on every one of its tabs.
    for tab in strip(&paged) {
        assert!(super::super::tabs::tab_title(&paged, tab).contains(")*"), "{tab:?}");
    }
    for page in [b[1], b[0], b[2]] {
        activate(&mut paged, Tab { doc: 0, page: Some(page) });
        assert_eq!(save(&mut paged, &written), expected, "saved from page {page}");
    }
    assert!(!paged.session.active().unwrap().is_dirty());
    // Undo is the document's: one step back from any page tab.
    paged.run("edit.undo", json!({})).unwrap();
    assert!(paged.session.active().unwrap().is_dirty());
}

#[test]
fn new_layers_paste_and_place_on_a_page_tab_land_on_its_page() {
    let (mut app, _) = opened(true);
    let b = boards(&app);
    // Copy page 1's square, then paste it on page 2: centred on page 2, inside its artboard.
    let magenta = layer(&app, "Magenta");
    app.run("layer.select", json!({"layer": magenta.0})).unwrap();
    app.run("select.rect", json!({"x": 10, "y": 10, "width": 80, "height": 30})).unwrap();
    app.run("edit.copy", json!({})).unwrap();
    app.run("select.deselect", json!({})).unwrap();
    activate(&mut app, Tab { doc: 0, page: Some(b[1]) });
    let pasted = LayerId(invoke(&mut app, "edit.paste", json!({})).unwrap()["layer"].as_u64().unwrap());
    let st = app.session.active().unwrap();
    assert_eq!(st.doc.artboard_of(pasted), Some(LayerId(b[1])));
    let r = photocraft_engine::layer_multi_cmds::layer_bounds(st.doc.layer(pasted).unwrap()).unwrap();
    assert_eq!(r, photocraft_geom::Rect::new(210, 10, 290, 40), "centred on page 2, not on the canvas");
    // Paste in Place keeps the position, and the layer still joins page 2.
    let in_place = LayerId(invoke(&mut app, "edit.pasteSpecial.pasteInPlace", json!({})).unwrap()["layer"].as_u64().unwrap());
    assert_eq!(app.session.active().unwrap().doc.artboard_of(in_place), Some(LayerId(b[1])));
    // A new layer with nothing selected lands on the page too.
    app.run("select.deselectLayers", json!({})).unwrap();
    let new = LayerId(app.run("layer.new.layer", json!({"name": "New"})).unwrap()["layer"].as_u64().unwrap());
    assert_eq!(app.session.active().unwrap().doc.artboard_of(new), Some(LayerId(b[1])));
    // Place: the file centred on page 2.
    let mut src =
        photocraft_doc::Document::new("dot", photocraft_doc::Size { width: 20, height: 10 }, photocraft_doc::ColorMode::Rgb, photocraft_doc::SampleType::U8);
    let mut px = photocraft_doc::Layer::raster("px", src.pixel_format());
    px.surface_mut().unwrap().fill_rect(photocraft_geom::Rect::new(0, 0, 20, 10), &[0.0, 0.0, 1.0, 1.0]);
    src.layers.push(px);
    let bytes = photocraft_format::save_to_bytes(&src, &Default::default()).unwrap();
    let placed = app.place_bytes("dot.pcraft", bytes, None).unwrap();
    assert_eq!(app.session.active().unwrap().doc.artboard_of(LayerId(placed["layer"].as_u64().unwrap())), Some(LayerId(b[1])));
    assert_eq!(placed["bounds"], json!([240.0, 20.0, 260.0, 30.0]));
    // No loose layer was made.
    let top: Vec<u64> = app.session.active().unwrap().doc.layers.iter().map(|l| l.id.0).collect();
    assert_eq!(top, b);
}

#[test]
fn the_active_layer_follows_the_page_shown() {
    let (mut app, _) = opened(true);
    let b = boards(&app);
    let yellow = layer(&app, "Yellow");
    activate(&mut app, Tab { doc: 0, page: Some(b[2]) });
    assert_eq!(app.session.active().unwrap().active_layer, Some(yellow));
    // The page tab remembers its layer: back on page 3 after page 1, Yellow is active again.
    let artboard3 = LayerId(b[2]);
    app.run("layer.select", json!({"layer": artboard3.0})).unwrap();
    activate(&mut app, Tab { doc: 0, page: Some(b[0]) });
    assert_eq!(app.session.active().unwrap().active_layer, Some(layer(&app, "Magenta")));
    activate(&mut app, Tab { doc: 0, page: Some(b[2]) });
    assert_eq!(app.session.active().unwrap().active_layer, Some(artboard3));
    // Deselecting every layer stays so on a page.
    app.run("select.deselectLayers", json!({})).unwrap();
    app.sync_views();
    assert_eq!(app.session.active().unwrap().active_layer, None);
}

#[test]
fn closing_a_page_tab_keeps_the_page_and_window_pages_opens_it_again() {
    let (mut app, _) = opened(true);
    let b = boards(&app);
    let revision = app.session.active().unwrap().revision;
    activate(&mut app, Tab { doc: 0, page: Some(b[1]) });
    invoke(&mut app, "window.pages.close", json!({})).unwrap();
    assert_eq!(pages_of(&strip(&app)), vec![Some(b[0]), Some(b[2])]);
    assert_eq!(active(&app), Some(Tab { doc: 0, page: Some(b[2]) }), "the next page shows");
    assert_eq!(boards(&app), b, "the page itself stays");
    assert!(!app.session.active().unwrap().is_dirty());
    assert_eq!(app.session.documents().len(), 1);
    // Window › Pages lists every page, the closed one unchecked; its row opens and shows it.
    let rows = page_rows(&app);
    assert_eq!(rows, vec![("Page 1".to_string(), true), ("Page 2".to_string(), false), ("Page 3".to_string(), true)]);
    invoke(&mut app, &format!("{PAGE_ITEM}{}", b[1]), json!({})).unwrap();
    assert_eq!(pages_of(&strip(&app)), b.iter().map(|x| Some(*x)).collect::<Vec<_>>());
    assert_eq!(active(&app), Some(Tab { doc: 0, page: Some(b[1]) }));
    // Close Other Page Tabs leaves one, and that last page tab cannot close: no ×, and asking fails.
    invoke(&mut app, "window.pages.closeOthers", json!({})).unwrap();
    assert_eq!(pages_of(&strip(&app)), vec![Some(b[1])]);
    assert!(!closable(&app, Tab { doc: 0, page: Some(b[1]) }));
    assert!(invoke(&mut app, "window.pages.close", json!({})).is_err());
    assert_eq!(app.session.documents().len(), 1, "the document stays open");
    assert!(app.session.active().unwrap().revision >= revision);
    assert!(!app.session.active().unwrap().is_dirty(), "closing and opening page tabs changes nothing in the document");
}

fn page_rows(app: &PhotocraftApp) -> Vec<(String, bool)> {
    menus::menu_items(app).into_iter().filter(|i| i.id.starts_with(PAGE_ITEM)).map(|i| (i.label, i.checked == Some(true))).collect()
}

#[test]
fn artboards_and_page_tabs_stay_in_step() {
    let (mut app, _) = opened(true);
    let b = boards(&app);
    // A new artboard is a new page: its tab opens at the end and shows.
    let new = app.run("layer.new.artboard", json!({"rect": [600, 0, 100, 50], "name": "Page 4"})).unwrap()["layer"].as_u64().unwrap();
    assert_eq!(strip(&app).last().and_then(|t| t.page), Some(new));
    assert_eq!(active(&app).and_then(|t| t.page), Some(new));
    assert_eq!(app.ui.views[0].page, Some(new));
    // So is a duplicated one.
    activate(&mut app, Tab { doc: 0, page: Some(b[0]) });
    app.run("layer.select", json!({"layer": b[0]})).unwrap();
    app.run("layer.duplicate", json!({})).unwrap();
    let copy = *boards(&app).iter().find(|x| !b.contains(x) && **x != new).unwrap();
    assert_eq!(active(&app).and_then(|t| t.page), Some(copy));
    assert_eq!(strip(&app).len(), 5);
    // Renaming the artboard renames its tab.
    app.run("layer.renameLayer", json!({"layer": b[2], "name": "Back"})).unwrap();
    assert_eq!(super::super::tabs::tab_name(&app, Tab { doc: 0, page: Some(b[2]) }), "Back");
    // Deleting it closes its tab; the page shown moves on.
    activate(&mut app, Tab { doc: 0, page: Some(b[2]) });
    app.run("layer.delete", json!({"layer": b[2]})).unwrap();
    assert!(!strip(&app).iter().any(|t| t.page == Some(b[2])));
    assert!(active(&app).and_then(|t| t.page).is_some_and(|p| p != b[2]));
    // Undo brings the artboard, and so its page tab, back.
    app.run("edit.undo", json!({})).unwrap();
    assert!(strip(&app).iter().any(|t| t.page == Some(b[2])));
}

#[test]
fn moving_page_tabs_reorders_the_artboards_save_maps_to_pages() {
    let (mut app, written) = opened(true);
    let b = boards(&app);
    activate(&mut app, Tab { doc: 0, page: Some(b[0]) });
    invoke(&mut app, "window.pages.moveRight", json!({})).unwrap();
    assert_eq!(boards(&app), vec![b[1], b[0], b[2]]);
    assert_eq!(pages_of(&strip(&app)), vec![Some(b[1]), Some(b[0]), Some(b[2])], "tabs follow the artboards");
    assert!(app.session.active().unwrap().is_dirty(), "the page order is the document's");
    invoke(&mut app, "window.pages.moveLeft", json!({"page": b[2]})).unwrap();
    assert_eq!(boards(&app), vec![b[1], b[2], b[0]]);
    assert!(invoke(&mut app, "window.pages.moveLeft", json!({"page": b[1]})).is_err(), "already the first");
    // A tab dropped before another page tab of its document (as a tab drag does).
    assert!(move_page(&mut app, 0, b[0], Some(b[1])).unwrap());
    assert_eq!(boards(&app), b);
    assert!(move_page(&mut app, 0, b[0], None).unwrap());
    assert_eq!(boards(&app), vec![b[1], b[2], b[0]]);
    // What is saved is the new order of the artboards, nothing else.
    let bytes = save(&mut app, &written);
    let saved = photocraft_format::load_from_bytes(&bytes).unwrap();
    assert_eq!(saved.artboards().iter().map(|a| a.0.0).collect::<Vec<_>>(), vec![b[1], b[2], b[0]]);
    // Undo restores the order, one step per move.
    app.run("edit.undo", json!({})).unwrap();
    assert_eq!(boards(&app), b);
}

#[test]
fn ctrl_tab_and_next_page_step_through_the_page_tabs() {
    let (mut app, _) = opened(true);
    let b = boards(&app);
    app.run("file.new", json!({"width": 10, "height": 10, "name": "Other"})).unwrap();
    activate(&mut app, Tab { doc: 0, page: Some(b[0]) });
    let mut seen = Vec::new();
    for _ in 0..4 {
        invoke(&mut app, "window.nextDocument", json!({})).unwrap();
        seen.push(active(&app).unwrap());
    }
    assert_eq!(seen, vec![Tab { doc: 0, page: Some(b[1]) }, Tab { doc: 0, page: Some(b[2]) }, Tab::document(1), Tab { doc: 0, page: Some(b[0]) }]);
    invoke(&mut app, "window.pages.previous", json!({})).unwrap();
    assert_eq!(active(&app), Some(Tab { doc: 0, page: Some(b[2]) }), "Previous Page wraps around the document's pages");
    invoke(&mut app, "window.pages.next", json!({})).unwrap();
    assert_eq!(active(&app), Some(Tab { doc: 0, page: Some(b[0]) }));
}

#[test]
fn every_page_tab_keeps_its_own_camera() {
    let (mut app, _) = opened(true);
    let b = boards(&app);
    app.ui.views[0].zoom = 0.5;
    app.ui.views[0].fit_pending = false;
    activate(&mut app, Tab { doc: 0, page: Some(b[1]) });
    assert!(app.ui.views[0].fit_pending, "a page shown for the first time fits its artboard");
    app.ui.views[0].zoom = 2.0;
    app.ui.views[0].fit_pending = false;
    activate(&mut app, Tab { doc: 0, page: Some(b[0]) });
    assert_eq!((app.ui.views[0].zoom, app.ui.views[0].page), (0.5, Some(b[0])));
    assert_eq!(view_of(&app, Tab { doc: 0, page: Some(b[1]) }).zoom, 2.0);
    // Window › Arrange tiles the page tabs, each with its own view.
    app.ui.view.arrange = "tileAllVertically".into();
    let cells = super::super::tabs::arranged_cells(&app, egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 600.0))).unwrap();
    assert_eq!(cells.iter().map(|c| c.1.page).collect::<Vec<_>>(), b.iter().map(|x| Some(*x)).collect::<Vec<_>>());
}

#[test]
fn a_page_view_fits_its_artboard() {
    let (app, _) = opened(true);
    let doc = app.session.active().unwrap().doc.clone();
    let b = boards(&app);
    let mut view = View { page: Some(b[1]), ..View::default() };
    assert_eq!(page_rect(&view, &doc), Some(photocraft_geom::Rect::new(200, 0, 300, 50)));
    let board = page_rect(&view, &doc).unwrap();
    super::super::view_xform::fit_view_to_board(&mut view, board, [500, 50], egui::vec2(380.0, 600.0), 1.0, 1.0);
    assert_eq!(view.center, [250.0, 25.0]);
    assert_eq!(view.zoom, 1.0, "never past 100%");
    super::super::view_xform::fit_view_to_board(&mut view, board, [500, 50], egui::vec2(130.0, 600.0), 1.0, 1.0);
    assert!((view.zoom - 0.5).abs() < 1e-6, "{}", view.zoom);
    assert_eq!(page_rect(&View::default(), &doc), None, "a whole-canvas view");
}

/// The Layers panel of a page tab lists that page: its artboard and its layers, not the other
/// pages'.
#[test]
fn the_layers_panel_lists_only_the_page_shown() {
    let (app, _) = opened(true);
    let mut ready = false;
    let mut h = Harness::builder().with_size(egui::vec2(360.0, 650.0)).with_step_dt(1.0 / 60.0).build_ui_state(
        move |ui, app: &mut PhotocraftApp| {
            if !ready {
                PhotocraftApp::setup_context(ui.ctx(), crate::theme::ThemeKind::Pro);
                ready = true;
                return;
            }
            crate::panels::layers_panel(app, ui);
        },
        app,
    );
    h.run_steps(4);
    let shows = |h: &Harness<'_, PhotocraftApp>, name: &str| h.query_by_label(name).is_some();
    assert!(shows(&h, "Magenta") && !shows(&h, "Green") && !shows(&h, "Yellow"), "page 1's layers only");
    let b = boards(h.state());
    activate(h.state_mut(), Tab { doc: 0, page: Some(b[1]) });
    h.run_steps(4);
    assert!(shows(&h, "Green") && !shows(&h, "Magenta") && !shows(&h, "Yellow"), "page 2's layers only");
    // The whole canvas lists every page again.
    set_paged(h.state_mut(), 0, false).unwrap();
    h.run_steps(4);
    assert!(shows(&h, "Green") && shows(&h, "Magenta") && shows(&h, "Yellow"));
}

/// The pixels near `rgb` inside `area` of a rendered frame (`width` × `height`, RGBA rows).
fn count_in(raw: &[u8], width: u32, height: u32, area: egui::Rect, rgb: [u8; 3]) -> usize {
    let (x0, y0) = (area.min.x.max(0.0) as u32, area.min.y.max(0.0) as u32);
    let (x1, y1) = ((area.max.x.max(0.0) as u32).min(width), (area.max.y.max(0.0) as u32).min(height));
    let near = |x: u32, y: u32| {
        let at = ((y * width + x) * 4) as usize;
        raw.get(at..at + 3).is_some_and(|p| (0..3).all(|k| p[k].abs_diff(rgb[k]) <= 12))
    };
    (y0..y1).flat_map(|y| (x0..x1).map(move |x| (x, y))).filter(|&(x, y)| near(x, y)).count()
}

/// A page tab draws its artboard and nothing else of the canvas: the neighbouring pages, which
/// lie in the view 100 px left and right of it, do not show; the whole-canvas view shows them.
/// Two page tabs tiled side by side (Window › Arrange) each draw their own page. On the GPU
/// canvas and on the CPU one (the web's `?cpu`). Skips without a GPU adapter to render the frame
/// (like the other wgpu canvas tests).
#[test]
fn a_page_tab_draws_only_its_artboard() {
    page_tab_pixels(true);
    page_tab_pixels(false);
}

fn page_tab_pixels(gpu: bool) {
    let bytes = design();
    let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        egui_kittest::Harness::builder().with_size(egui::vec2(1000.0, 700.0)).with_pixels_per_point(1.0).with_max_steps(64).wgpu().build_eframe(move |cc| {
            PhotocraftApp::setup_context(&cc.egui_ctx, crate::theme::ThemeKind::Pro);
            let (mut app, _) = app(true);
            if let Some(rs) = cc.wgpu_render_state.as_ref().filter(|_| gpu) {
                app.set_wgpu(rs.clone());
            }
            app.open_bytes_from("Design.pcraft", &bytes, Some("Design.pcraft")).unwrap();
            app
        })
    }));
    let Ok(mut h) = built else {
        eprintln!("skipping: no GPU adapter");
        return;
    };
    let (magenta, green, yellow) = ([255, 0, 255], [0, 255, 0], [255, 255, 0]);
    let b = boards(h.state());
    activate(h.state_mut(), Tab { doc: 0, page: Some(b[1]) });
    h.run_steps(8);
    let img = h.render().expect("render");
    let count = |area: egui::Rect, rgb: [u8; 3]| count_in(img.as_raw(), img.width(), img.height(), area, rgb);
    let area = h.state().last_canvas_rect;
    // Page 2 at 100%, centred: its 80 × 30 green square shows whole; pages 1 and 3 would be in
    // view on either side.
    assert_eq!(h.state().gpu.is_some(), gpu);
    assert!(count(area, green) >= 80 * 30 * 9 / 10, "gpu {gpu}: page 2 shows: {}", count(area, green));
    assert_eq!((count(area, magenta), count(area, yellow)), (0, 0), "gpu {gpu}: nothing of pages 1 and 3");
    // The same view of the whole canvas shows the neighbours.
    let (zoom, center) = (h.state().ui.views[0].zoom, h.state().ui.views[0].center);
    set_paged(h.state_mut(), 0, false).unwrap();
    let v = &mut h.state_mut().ui.views[0];
    (v.zoom, v.center, v.fit_pending) = (zoom, center, false);
    h.run_steps(8);
    let img = h.render().expect("render");
    let count = |area: egui::Rect, rgb: [u8; 3]| count_in(img.as_raw(), img.width(), img.height(), area, rgb);
    assert!(count(area, magenta) > 0 && count(area, yellow) > 0, "gpu {gpu}: the canvas view shows pages 1 and 3");
    // Two page tabs side by side: each tile draws its own page only.
    set_paged(h.state_mut(), 0, true).unwrap();
    activate(h.state_mut(), Tab { doc: 0, page: Some(b[1]) });
    invoke(h.state_mut(), "window.pages.closeOthers", json!({})).unwrap();
    invoke(h.state_mut(), &format!("{PAGE_ITEM}{}", b[2]), json!({})).unwrap();
    activate(h.state_mut(), Tab { doc: 0, page: Some(b[1]) });
    h.state_mut().ui.view.arrange = "twoUpVertical".into();
    h.run_steps(8);
    let img = h.render().expect("render");
    let count = |area: egui::Rect, rgb: [u8; 3]| count_in(img.as_raw(), img.width(), img.height(), area, rgb);
    let area = h.state().last_canvas_rect;
    let (left, right) = (
        egui::Rect::from_min_max(area.min, egui::pos2(area.center().x, area.max.y)),
        egui::Rect::from_min_max(egui::pos2(area.center().x, area.min.y), area.max),
    );
    assert!(count(left, green) > 0 && count(left, yellow) == 0 && count(left, magenta) == 0, "gpu {gpu}: the left tile is page 2");
    assert!(count(right, yellow) > 0 && count(right, green) == 0 && count(right, magenta) == 0, "gpu {gpu}: the right tile is page 3");
}
