//! A page tab's edits ([`crate::DocState::page`]): every new layer lands on the page's artboard,
//! Paste and Place centre on it, and nothing an edit does leaves a layer loose on the canvas.

use photocraft_doc::{Document, Layer, LayerId};
use photocraft_geom::Rect;
use serde_json::{Value, json};

use crate::Session;

fn id(v: Value) -> LayerId {
    LayerId(v["layer"].as_u64().unwrap())
}

fn doc(s: &Session) -> &Document {
    &s.active().unwrap().doc
}

/// The top level, but for the Background `file.new` made (it stays at the bottom, as Photoshop
/// keeps it).
fn top(s: &Session) -> Vec<LayerId> {
    doc(s).layers.iter().filter(|l| !doc(s).is_background(l.id)).map(|l| l.id).collect()
}

fn children(s: &Session, group: LayerId) -> Vec<LayerId> {
    doc(s).layer(group).unwrap().children().unwrap().iter().map(|l| l.id).collect()
}

fn select(s: &mut Session, layer: LayerId) {
    s.execute("layer.select", json!({"layer": layer.0})).unwrap();
}

fn bounds(s: &Session, layer: LayerId) -> Rect {
    crate::layer_multi_cmds::layer_bounds(doc(s).layer(layer).unwrap()).unwrap()
}

/// Three 100 × 50 artboards side by side, 100 px apart (as the server lays out a design's pages),
/// each with one pixel layer; the second is shown as a page.
fn pages() -> (Session, [LayerId; 3], [LayerId; 3]) {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 100, "height": 50})).unwrap();
    let mut boards = [LayerId(0); 3];
    let mut layers = [LayerId(0); 3];
    for (i, x) in [0, 200, 400].into_iter().enumerate() {
        boards[i] = id(s.execute("layer.new.artboard", json!({"rect": [x, 0, 100, 50], "name": format!("Page {}", i + 1)})).unwrap());
        layers[i] = id(s.execute("layer.new.layer", json!({"name": format!("Layer {}", i + 1)})).unwrap());
        assert_eq!(doc(&s).artboard_of(layers[i]), Some(boards[i]));
    }
    // The canvas grew to hold every board: the page is the board, not the canvas.
    assert_eq!(doc(&s).bounds(), Rect::new(0, 0, 500, 50));
    assert!(s.set_page(0, Some(boards[1])));
    select(&mut s, layers[1]);
    (s, boards, layers)
}

#[test]
fn page_rect_is_the_artboard_and_the_whole_canvas_without_a_page() {
    let (mut s, boards, _) = pages();
    let st = s.active().unwrap();
    assert_eq!(st.page_rect(), Some(Rect::new(200, 0, 300, 50)));
    assert_eq!(st.placement_area(), Rect::new(200, 0, 300, 50));
    s.set_page(0, None);
    assert_eq!(s.active().unwrap().placement_area(), Rect::new(0, 0, 500, 50));
    // A page that is no artboard (a deleted one, any other layer) is the whole canvas.
    s.set_page(0, Some(LayerId(u64::MAX)));
    assert_eq!(s.active().unwrap().page_rect(), None);
    assert!(!s.set_page(7, Some(boards[0])), "no such document");
}

#[test]
fn new_layers_land_on_the_page_whatever_is_active() {
    let (mut s, boards, layers) = pages();
    let a = id(s.execute("layer.new.layer", json!({})).unwrap());
    assert_eq!(children(&s, boards[1]), vec![layers[1], a], "above the active layer, on the page");
    // Nothing selected: Photoshop puts the layer on top of the stack; a page puts it on top of
    // the page.
    s.execute("select.deselectLayers", json!({})).unwrap();
    let b = id(s.execute("layer.new.layer", json!({})).unwrap());
    assert_eq!(children(&s, boards[1]), vec![layers[1], a, b]);
    let g = id(s.execute("layer.new.group", json!({})).unwrap());
    assert_eq!(doc(&s).artboard_of(g), Some(boards[1]));
    // No loose layer: the top level is still just the three boards.
    assert_eq!(top(&s), boards.to_vec());
    // One history step each: undo takes the group away, not just its move onto the page.
    s.execute("edit.undo", json!({})).unwrap();
    assert!(doc(&s).layer(g).is_none());
    assert_eq!(top(&s), boards.to_vec());
}

#[test]
fn paste_centres_on_the_page_and_paste_in_place_stays_on_it() {
    let (mut s, boards, layers) = pages();
    // A 10 × 10 square on the first page, copied.
    select(&mut s, layers[0]);
    s.execute("select.rect", json!({"x": 10, "y": 10, "width": 10, "height": 10})).unwrap();
    s.execute("edit.fill", json!({"color": "#ff0000"})).unwrap();
    s.execute("edit.copy", json!({})).unwrap();
    s.execute("select.deselect", json!({})).unwrap();
    // Back on the second page: Paste centres the square on the page, not on the canvas
    // (whose centre, x = 250, lies in the gap between the boards).
    select(&mut s, layers[1]);
    let pasted = id(s.execute("edit.paste", json!({})).unwrap());
    assert_eq!(doc(&s).artboard_of(pasted), Some(boards[1]));
    assert_eq!(bounds(&s, pasted), Rect::new(245, 20, 255, 30));
    // A centre the UI asks for still counts, kept on the page.
    let at = id(s.execute("edit.paste", json!({"center": [500.0, 25.0]})).unwrap());
    assert_eq!(bounds(&s, at), Rect::new(290, 20, 300, 30));
    // Paste in Place keeps the pixels where they were (on the first page's area, so the board
    // clips them away) but the layer still lands on this page, never loose.
    s.execute("select.deselectLayers", json!({})).unwrap();
    let in_place = id(s.execute("edit.pasteSpecial.pasteInPlace", json!({})).unwrap());
    assert_eq!(doc(&s).artboard_of(in_place), Some(boards[1]));
    assert_eq!(bounds(&s, in_place), Rect::new(10, 10, 20, 20));
    assert_eq!(top(&s), boards.to_vec());
}

#[test]
fn place_fits_and_centres_on_the_page() {
    let (mut s, boards, _) = pages();
    // A 200 × 100 image: larger than the 100 × 50 page, so it is fitted to the page.
    let mut src = Document::new("big", photocraft_doc::Size { width: 200, height: 100 }, photocraft_doc::ColorMode::Rgb, photocraft_doc::SampleType::U8);
    let mut l = Layer::raster("px", src.pixel_format());
    l.surface_mut().unwrap().fill_rect(Rect::new(0, 0, 200, 100), &[0.0, 0.0, 1.0, 1.0]);
    src.layers.push(l);
    let bytes = photocraft_format::save_to_bytes(&src, &Default::default()).unwrap();
    let r = crate::file_cmds::place_bytes(&mut s, "big.pcraft", bytes, None, &json!({})).unwrap();
    let placed = id(r.clone());
    assert_eq!(doc(&s).artboard_of(placed), Some(boards[1]));
    assert_eq!(r["scale"].as_f64(), Some(50.0), "fitted to the page, not to the 500 px canvas");
    assert_eq!(r["bounds"], json!([200.0, 0.0, 300.0, 50.0]));
}

#[test]
fn a_layer_dragged_to_the_root_stays_on_the_page() {
    let (mut s, boards, layers) = pages();
    let a = id(s.execute("layer.new.layer", json!({"name": "A"})).unwrap());
    // The Layers panel's root, above the page's board: on top of the page.
    s.execute("layer.moveTo", json!({"layer": layers[1].0, "target": boards[1].0, "position": "above"})).unwrap();
    assert_eq!(children(&s, boards[1]), vec![a, layers[1]]);
    // Below the board: at the bottom of the page.
    s.execute("layer.moveTo", json!({"layer": layers[1].0, "target": boards[1].0, "position": "below"})).unwrap();
    assert_eq!(children(&s, boards[1]), vec![layers[1], a]);
    assert_eq!(top(&s), boards.to_vec());
    // Into another page is a move like any other: the page only catches loose layers.
    s.execute("layer.moveTo", json!({"layer": a.0, "target": boards[2].0, "position": "into"})).unwrap();
    assert_eq!(doc(&s).artboard_of(a), Some(boards[2]));
}

#[test]
fn artboards_and_layers_that_were_loose_already_stay_where_they_are() {
    let (mut s, boards, _) = pages();
    // A loose layer from before the page was shown (a design saved with one).
    s.set_page(0, None);
    s.execute("select.deselectLayers", json!({})).unwrap();
    let loose = id(s.execute("layer.new.layer", json!({"name": "loose"})).unwrap());
    assert_eq!(top(&s), vec![boards[0], boards[1], boards[2], loose]);
    s.set_page(0, Some(boards[1]));
    // An unrelated edit keeps it loose, and a new artboard is a page of its own.
    s.execute("layer.setProps", json!({"layer": loose.0, "opacity": 0.5})).unwrap();
    let board = id(s.execute("layer.new.artboard", json!({"rect": [600, 0, 100, 50]})).unwrap());
    assert_eq!(top(&s), vec![boards[0], boards[1], boards[2], loose, board]);
}

#[test]
fn deleting_the_pages_artboard_is_not_refused() {
    let (mut s, boards, _) = pages();
    select(&mut s, boards[1]);
    s.execute("layer.delete", json!({})).unwrap();
    assert_eq!(top(&s), vec![boards[0], boards[2]]);
    assert_eq!(s.active().unwrap().page_rect(), None);
    // With the page gone, new layers go where they would without one.
    s.execute("select.deselectLayers", json!({})).unwrap();
    let l = id(s.execute("layer.new.layer", json!({})).unwrap());
    assert_eq!(top(&s).last(), Some(&l));
}

#[test]
fn the_page_is_view_state_and_never_changes_what_is_saved() {
    let (mut s, _, _) = pages();
    let before = photocraft_format::save_to_bytes(doc(&s), &Default::default()).unwrap();
    let dirty = s.active().unwrap().is_dirty();
    let revision = s.active().unwrap().revision;
    s.set_page(0, None);
    let after = photocraft_format::save_to_bytes(doc(&s), &Default::default()).unwrap();
    assert_eq!(before, after);
    assert_eq!(s.active().unwrap().is_dirty(), dirty);
    assert_eq!(s.active().unwrap().revision, revision, "not a history step");
}
