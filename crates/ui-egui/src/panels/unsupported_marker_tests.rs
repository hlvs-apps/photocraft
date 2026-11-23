//! #6541: the Layers panel marks an adjustment layer PhotoCraft keeps as imported but cannot
//! render, so a picture that differs from Photoshop's has a visible reason.

use egui::vec2;
use egui_kittest::Harness;
use photocraft_doc::{Adjustment, Layer, LayerContent, LayerId};
use serde_json::json;

use crate::PhotocraftApp;
use crate::layer_row_ui::{Indicator, RowRects, recorded};

fn row(h: &Harness<'_, PhotocraftApp>, id: LayerId) -> RowRects {
    recorded(&h.ctx).into_iter().find(|r| r.layer == id.0).unwrap()
}

#[test]
fn an_unsupported_adjustment_row_carries_a_warning_marker() {
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", json!({"width": 64, "height": 48})).unwrap();
    let plain = LayerId(s.execute("layer.new.layer", json!({"name": "plain"})).unwrap()["layer"].as_u64().unwrap());
    let mystery = Layer::new("Mystery", LayerContent::Adjustment(Adjustment::Unsupported { psd_key: "xyzw".into(), raw: vec![1, 2, 3].into() }));
    let mystery_id = mystery.id;
    s.edit("unsupported fixture", |doc, active| {
        doc.layers.push(mystery);
        *active = Some(mystery_id);
        Ok(())
    })
    .unwrap();
    let mut h = Harness::builder().with_size(vec2(1440.0, 900.0)).with_max_steps(64).build_eframe(move |cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
        PhotocraftApp::new(s, crate::Services::default())
    });
    h.run_steps(8);
    let marked = row(&h, mystery_id);
    assert!(marked.indicators.iter().any(|(k, _)| *k == Indicator::Unsupported), "{:?}", marked.indicators);
    assert!(!row(&h, plain).indicators.iter().any(|(k, _)| *k == Indicator::Unsupported));
    // The marker takes room from the name, never overlaps it.
    let (_, marker) = marked.indicators.iter().find(|(k, _)| *k == Indicator::Unsupported).copied().unwrap();
    if let Some(name) = marked.name {
        assert!(name.right() <= marker.left(), "name {name:?} runs under the marker {marker:?}");
    }
}

/// A layer an alternate layout places or shows differently carries the layout's badge while the
/// layout is shown (the embedding app's QR-off view, its ledger UX-4); the others do not, and
/// nothing does once it is left.
#[test]
fn a_layer_the_shown_layout_moves_carries_its_badge() {
    let mut s = photocraft_engine::Session::new();
    s.execute("file.new", json!({"width": 64, "height": 48})).unwrap();
    let moved = LayerId(s.execute("layer.new.layer", json!({"name": "moved"})).unwrap()["layer"].as_u64().unwrap());
    let still = LayerId(s.execute("layer.new.layer", json!({"name": "still"})).unwrap()["layer"].as_u64().unwrap());
    let comp = s.execute("layerComp.new", json!({"name": "QR off", "alternate": true})).unwrap()["comp"].as_u64().unwrap();
    s.execute("layerComp.apply", json!({"comp": comp})).unwrap();
    s.execute("layer.setProps", json!({"layer": moved.0, "visible": false})).unwrap();
    s.execute("layerComp.restoreLastDocumentState", json!({})).unwrap();
    s.execute("layerComp.apply", json!({"comp": comp})).unwrap();
    let mut h = Harness::builder().with_size(vec2(1440.0, 900.0)).with_max_steps(64).build_eframe(move |cc| {
        PhotocraftApp::setup_context(&cc.egui_ctx, Default::default());
        PhotocraftApp::new(s, crate::Services::default())
    });
    h.run_steps(8);
    let badge = |h: &Harness<'_, PhotocraftApp>, id| row(h, id).indicators.iter().any(|(k, _)| *k == Indicator::Layout);
    assert!(badge(&h, moved), "{:?}", row(&h, moved).indicators);
    assert!(!badge(&h, still));
    h.state_mut().run("layerComp.restoreLastDocumentState", json!({})).unwrap();
    h.run_steps(8);
    assert!(!badge(&h, moved), "the badge stays only while the layout is shown");
}
