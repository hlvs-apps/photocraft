//! The canvas's alternate-layout switch: when a document has alternate layouts (an embedding
//! app's second arrangement, its "without QR" layout), a small segmented control at the canvas's
//! top right switches between the document's own arrangement and each layout, as the Layer Comps
//! panel's boxes do. On a page a layout does not reach (its scope) that segment is off, with the
//! reason as its tooltip. Every switch is a `layerComp.*` command, so it is one history step.

use egui::{Align2, Rect, RichText, vec2};
use photocraft_doc::{Document, LayerComp, LayerId};
use serde_json::{Value, json};

use crate::PhotocraftApp;
use crate::theme::Tokens;

/// One segment of the switch.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub label: String,
    /// The comp it shows; None for the document's own arrangement.
    pub comp: Option<u32>,
    pub selected: bool,
    /// Why it is off on this page, None when it can be chosen.
    pub disabled: Option<String>,
}

/// The switch for `doc` on `page` (the artboard a page tab shows; None for the whole canvas):
/// None when the document has no alternate layout.
pub fn segments(doc: &Document, page: Option<u64>) -> Option<Vec<Segment>> {
    let layouts: Vec<&LayerComp> = doc.layer_comps.iter().filter(|c| c.alternate).collect();
    let first = layouts.first()?;
    let reaches = |c: &LayerComp| c.scope.is_empty() || page.is_some_and(|p| c.scope.contains(&LayerId(p))) || (page.is_none() && doc.artboards().is_empty());
    // The shown layout is document-wide, but this page can still be using the base.
    let shown = doc.comp_edit.as_ref().map(|e| e.comp).filter(|id| layouts.iter().any(|c| c.id == *id && reaches(c)));
    let label = if first.document_label.is_empty() { tl!("Document").to_string() } else { first.document_label.clone() };
    let mut out = vec![Segment { label, comp: None, selected: shown.is_none(), disabled: None }];
    for c in layouts {
        let disabled = (!reaches(c)).then(|| crate::i18n::fmt(tl!("\u{201c}{name}\u{201d} does not apply to this page"), &[("name", &c.name)]));
        out.push(Segment { label: c.name.clone(), comp: Some(c.id), selected: shown == Some(c.id), disabled });
    }
    Some(out)
}

/// The command a click on `s` runs, None when it is already shown.
pub fn action(s: &Segment) -> Option<(&'static str, Value)> {
    match (s.selected, s.comp) {
        (true, _) => None,
        (false, None) => Some(("layerComp.restoreLastDocumentState", json!({}))),
        (false, Some(id)) => Some(("layerComp.apply", json!({"comp": id}))),
    }
}

/// Draws the switch over the canvas `rect` of document `idx`, page `page`.
pub fn show(app: &mut PhotocraftApp, ctx: &egui::Context, rect: Rect, idx: usize, page: Option<u64>) {
    let Some(doc) = app.session.documents().get(idx).map(|st| st.doc.clone()) else { return };
    let Some(segs) = segments(&doc, page) else { return };
    let t = Tokens::get(ctx);
    let mut clicked = None;
    egui::Area::new(egui::Id::unique(("layout-switch", idx)))
        .order(egui::Order::Foreground)
        .pivot(Align2::RIGHT_TOP)
        .fixed_pos(rect.right_top() + vec2(-12.0, 10.0))
        .show(ctx, |ui| {
            egui::Frame::NONE.fill(t.chrome).corner_radius(4.0).inner_margin(egui::Margin::same(3)).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    for s in &segs {
                        let text = RichText::new(&s.label).size(11.5);
                        let resp = ui.add_enabled(s.disabled.is_none(), egui::Button::selectable(s.selected, text));
                        let resp = match &s.disabled {
                            Some(why) => resp.on_disabled_hover_text(why.as_str()),
                            None => resp,
                        };
                        if resp.clicked() {
                            clicked = action(s);
                        }
                    }
                });
            });
        });
    if let Some((cmd, p)) = clicked
        && let Err(e) = app.run(cmd, p)
    {
        app.ui.status = e;
        app.ui.status_error = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use photocraft_engine::Session;

    fn doc_with_layout(scope: bool) -> (Session, u32, u64, u64) {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 64, "height": 48})).unwrap();
        let a = s.execute("layer.new.artboard", json!({"rect": [0, 0, 30, 30], "name": "One"})).unwrap()["layer"].as_u64().unwrap();
        let b = s.execute("layer.new.artboard", json!({"rect": [34, 0, 30, 30], "name": "Two"})).unwrap()["layer"].as_u64().unwrap();
        let scope = if scope { json!([a]) } else { json!(null) };
        let c = s.execute("layerComp.new", json!({"name": "Without QR", "alternate": true, "scope": scope, "documentLabel": "With QR"})).unwrap()["comp"]
            .as_u64()
            .unwrap() as u32;
        (s, c, a, b)
    }

    #[test]
    fn the_switch_names_both_arrangements_and_is_off_where_the_layout_does_not_reach() {
        let (mut s, c, a, b) = doc_with_layout(true);
        let doc = &s.active().unwrap().doc;
        let on_a = segments(doc, Some(a)).unwrap();
        assert_eq!(on_a.iter().map(|s| s.label.as_str()).collect::<Vec<_>>(), ["With QR", "Without QR"]);
        assert!(on_a[0].selected && !on_a[1].selected && on_a[1].disabled.is_none());
        assert_eq!(action(&on_a[1]), Some(("layerComp.apply", json!({"comp": c}))));
        let on_b = segments(doc, Some(b)).unwrap();
        assert!(on_b[1].disabled.is_some(), "a page without the layout cannot switch to it");
        s.execute("layerComp.apply", json!({"comp": c})).unwrap();
        let on_b = segments(&s.active().unwrap().doc, Some(b)).unwrap();
        assert!(on_b[0].selected && !on_b[1].selected && on_b[1].disabled.is_some(), "an unaffected page shows the base as selected");
        let mut plain = Session::new();
        plain.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
        assert!(segments(&plain.active().unwrap().doc, None).is_none(), "no layout, no switch");
    }

    #[test]
    fn clicking_the_switch_shows_and_leaves_the_layout() {
        use egui_kittest::{Harness, kittest::Queryable};
        let (s, c, a, _) = doc_with_layout(false);
        let app = PhotocraftApp::new(s, crate::Services::default());
        let mut h = Harness::builder().with_size(vec2(400.0, 200.0)).build_ui_state(
            move |ui, app: &mut PhotocraftApp| show(app, &ui.ctx().clone(), Rect::from_min_size(egui::pos2(0.0, 0.0), vec2(400.0, 200.0)), 0, Some(a)),
            app,
        );
        h.run_steps(3);
        h.get_by_label("Without QR").click();
        h.run_steps(3);
        assert_eq!(h.state().session.active().unwrap().doc.comp_edit.as_ref().map(|e| e.comp), Some(c));
        h.get_by_label("With QR").click();
        h.run_steps(3);
        assert!(h.state().session.active().unwrap().doc.comp_edit.is_none());
    }
}
