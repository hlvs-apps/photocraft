//! Alternate layouts (`LayerComp::alternate`, `CompLayerState::placement`) in `.pcraft`.

use photocraft_color::{ColorMode, SampleType};
use photocraft_doc::text::TextShape;
use photocraft_doc::{CompEdit, CompLayerState, CompPlacement, Document, Layer, LayerComp, LayerContent, Size, TextLayer};
use photocraft_geom::{Affine, Rect};

fn document() -> (Document, Layer, Layer) {
    let mut doc = Document::new("Layouts", Size::new(64, 48), ColorMode::Rgb, SampleType::U8);
    let fmt = doc.pixel_format();
    let mut a = Layer::raster("A", fmt);
    a.surface_mut().unwrap().fill_rect(Rect::new(4, 4, 14, 14), &photocraft_raster::from_rgba(&fmt, [1.0, 0.0, 0.0, 1.0]));
    let t = Layer::new(
        "T",
        LayerContent::Text(TextLayer {
            text: "Hi".into(),
            transform: Affine::translate(8.0, 30.0),
            shape: TextShape::Box { x: 0.0, y: 0.0, width: 40.0, height: 12.0 },
            ..Default::default()
        }),
    );
    doc.layers.push(a.clone());
    doc.layers.push(t.clone());
    (doc, a, t)
}

fn comp(id: u32, alternate: bool, states: Vec<CompLayerState>) -> LayerComp {
    LayerComp { id, name: format!("C{id}"), comment: String::new(), apply_visibility: true, apply_position: true, apply_appearance: false, alternate, states }
}

#[test]
fn alternate_layouts_and_exact_placements_round_trip() {
    let (mut doc, a, t) = document();
    let exact = Affine { m: [0.9993908270190958, 0.03489949670250097, -0.03489949670250097, 0.9993908270190958, 8.123456789012345, 30.000000000000004] };
    doc.layer_comps.push(comp(
        7,
        true,
        vec![
            CompLayerState {
                layer: a.id,
                visible: Some(false),
                position: None,
                appearance: None,
                placement: Some(CompPlacement::Bounds(Rect::new(7, 6, 27, 26))),
            },
            CompLayerState {
                layer: t.id,
                visible: None,
                position: None,
                appearance: None,
                placement: Some(CompPlacement::Transform {
                    transform: exact,
                    perspective: Some([1.0, 0.1, 3.0, 0.0, 1.0, 4.0, 0.001, 0.0, 1.0]),
                    text_box: Some([0.0, 0.0, 52.5, 12.25]),
                }),
            },
        ],
    ));
    doc.layer_comps.push(comp(8, false, vec![CompLayerState { layer: a.id, visible: Some(true), position: Some((4, 4)), appearance: None, placement: None }]));
    let bytes = photocraft_format::save_to_bytes(&doc, &Default::default()).unwrap();
    let back = photocraft_format::load_from_bytes(&bytes).unwrap();
    assert_eq!(back.layer_comps, doc.layer_comps, "every double comes back bit for bit");
}

#[test]
fn a_document_without_alternate_layouts_writes_the_manifest_it_wrote_before() {
    let (mut doc, a, _) = document();
    doc.layer_comps.push(comp(8, false, vec![CompLayerState { layer: a.id, visible: Some(true), position: Some((4, 4)), appearance: None, placement: None }]));
    let bytes = photocraft_format::save_to_bytes(&doc, &Default::default()).unwrap();
    let manifest = serde_json::to_string(&photocraft_format::read_manifest(&bytes).unwrap()).unwrap();
    assert!(!manifest.contains("\"alternate\"") && !manifest.contains("\"placement\""), "{manifest}");
}

#[test]
fn a_file_written_before_alternate_layouts_opens_with_ordinary_comps() {
    let (mut doc, a, _) = document();
    doc.layer_comps.push(comp(8, false, vec![CompLayerState { layer: a.id, visible: Some(false), position: Some((4, 4)), appearance: None, placement: None }]));
    let bytes = photocraft_format::save_to_bytes(&doc, &Default::default()).unwrap();
    // The manifest as an older writer made it: no `alternate`, no `placement` keys at all.
    let mut m = serde_json::to_value(photocraft_format::read_manifest(&bytes).unwrap()).unwrap();
    let c = &mut m["document"]["layer_comps"][0];
    assert!(c.as_object_mut().unwrap().remove("alternate").is_none());
    assert!(c["states"][0].as_object_mut().unwrap().remove("placement").is_none());
    let old: photocraft_format::Manifest = serde_json::from_value(m).unwrap();
    assert!(!old.document.layer_comps[0].alternate);
    assert_eq!(old.document.layer_comps[0].states[0].placement, None);
    let back = photocraft_format::load_from_bytes(&bytes).unwrap();
    assert_eq!(back.layer_comps, doc.layer_comps);
}

#[test]
fn a_save_writes_a_shown_layout_left() {
    let (mut doc, a, _) = document();
    doc.layer_comps.push(comp(7, true, Vec::new()));
    let base = vec![doc.layers[0].clone(), doc.layers[1].clone()];
    // What the engine does to show it: the layers as they were, then a move and a hide.
    doc.comp_edit = Some(CompEdit { comp: 7, base, shown: Vec::new() });
    doc.last_applied_comp = Some(7);
    let fmt = doc.pixel_format();
    let mut moved = photocraft_raster::Surface::new(fmt);
    moved.fill_rect(Rect::new(9, 4, 19, 14), &photocraft_raster::from_rgba(&fmt, [1.0, 0.0, 0.0, 1.0]));
    *doc.layers[0].surface_mut().unwrap() = moved;
    doc.layers[1].visible = false;
    let bytes = photocraft_format::save_to_bytes(&doc, &Default::default()).unwrap();
    let back = photocraft_format::load_from_bytes(&bytes).unwrap();
    assert!(back.comp_edit.is_none());
    assert_eq!(back.last_applied_comp, None);
    assert_eq!(back.layers[0].surface().unwrap().content_bounds(), Rect::new(4, 4, 14, 14));
    assert!(back.layers[1].visible);
    let c = back.comp(7).unwrap();
    assert_eq!(c.state(back.layers[0].id).unwrap().placement, Some(CompPlacement::Bounds(Rect::new(9, 4, 19, 14))));
    assert_eq!(c.state(back.layers[1].id).unwrap().visible, Some(false));
    let _ = a;
}
