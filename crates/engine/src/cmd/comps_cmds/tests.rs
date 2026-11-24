use super::*;
use photocraft_color::BlendMode;
use photocraft_doc::Layer;
use photocraft_geom::Rect;

fn session(depth: u32) -> (Session, LayerId, LayerId) {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 64, "height": 48, "depth": depth})).unwrap();
    let (a, b) = s
        .edit("setup", |doc, _| {
            let fmt = doc.pixel_format();
            let mut a = Layer::raster("A", fmt);
            a.surface_mut().unwrap().fill_rect(Rect::new(4, 4, 14, 14), &photocraft_raster::from_rgba(&fmt, [1.0, 0.0, 0.0, 1.0]));
            let mut b = Layer::raster("B", fmt);
            b.surface_mut().unwrap().fill_rect(Rect::new(30, 20, 40, 30), &photocraft_raster::from_rgba(&fmt, [0.0, 0.0, 1.0, 1.0]));
            let (ia, ib) = (a.id, b.id);
            doc.layers.push(a);
            doc.layers.push(b);
            Ok((ia, ib))
        })
        .unwrap();
    (s, a, b)
}

fn doc(s: &Session) -> &Document {
    &s.active().unwrap().doc
}

fn pos(s: &Session, id: LayerId) -> Option<(i32, i32)> {
    layer_position(doc(s).layer(id).unwrap())
}

#[test]
fn capture_apply_visibility_position_appearance_independently() {
    for depth in [8, 16, 32] {
        let (mut s, a, b) = session(depth);
        let c1 = s.execute("layerComp.new", json!({"name": "Start"})).unwrap()["comp"].as_u64().unwrap();
        // Change all three properties.
        s.execute("layer.translate", json!({"layer": a.0, "dx": 10, "dy": 5})).unwrap();
        s.edit("t", |d, _| {
            let l = d.layer_mut(b).unwrap();
            l.visible = false;
            l.blend = BlendMode::Multiply;
            l.opacity = 0.5;
            Ok(())
        })
        .unwrap();
        assert_eq!(pos(&s, a), Some((14, 9)));

        // Visibility only.
        s.execute("layerComp.setOptions", json!({"comp": c1, "position": false, "appearance": false})).unwrap();
        s.execute("layerComp.apply", json!({"comp": c1})).unwrap();
        assert!(doc(&s).layer(b).unwrap().visible, "{depth}");
        assert_eq!(pos(&s, a), Some((14, 9)));
        assert_eq!(doc(&s).layer(b).unwrap().blend, BlendMode::Multiply);

        // Position only.
        s.edit("t", |d, _| {
            d.layer_mut(b).unwrap().visible = false;
            Ok(())
        })
        .unwrap();
        s.execute("layerComp.setOptions", json!({"comp": c1, "visibility": false, "position": true})).unwrap();
        s.execute("layerComp.apply", json!({})).unwrap();
        assert_eq!(pos(&s, a), Some((4, 4)));
        assert!(!doc(&s).layer(b).unwrap().visible);

        // Appearance only.
        s.execute("layerComp.setOptions", json!({"comp": "Start", "position": false, "appearance": true})).unwrap();
        s.execute("layerComp.apply", json!({})).unwrap();
        let lb = doc(&s).layer(b).unwrap();
        assert_eq!((lb.blend, lb.opacity, lb.visible), (BlendMode::Normal, 1.0, false));
        // Pixels really moved (not just bounds bookkeeping).
        let surf = doc(&s).layer(a).unwrap().surface().unwrap();
        assert!(surf.pixel(5, 5)[3] > 0.9 && surf.pixel(16, 11)[3] < 0.1);
    }
}

#[test]
fn undo_redo_and_last_document_state() {
    let (mut s, a, _) = session(8);
    s.execute("layerComp.new", json!({})).unwrap();
    assert_eq!(doc(&s).layer_comps[0].name, "Layer Comp 1");
    s.execute("layer.translate", json!({"layer": a.0, "dx": 3, "dy": 0})).unwrap();
    // A fresh document state (not a comp) is remembered when a comp is applied over it.
    s.execute("layerComp.apply", json!({})).unwrap();
    assert_eq!(pos(&s, a), Some((4, 4)));
    assert!(doc(&s).last_document_state.is_some());
    s.execute("layerComp.restoreLastDocumentState", json!({})).unwrap();
    assert_eq!(pos(&s, a), Some((7, 4)));
    assert_eq!(doc(&s).last_applied_comp, None);
    // Each step is one undo.
    assert!(s.undo());
    assert_eq!(pos(&s, a), Some((4, 4)));
    assert!(s.undo());
    assert_eq!(pos(&s, a), Some((7, 4)));
    assert!(s.redo());
    assert_eq!(pos(&s, a), Some((4, 4)));
    s.execute("layerComp.delete", json!({})).unwrap();
    assert!(doc(&s).layer_comps.is_empty());
    assert!(s.undo());
    assert_eq!(doc(&s).layer_comps.len(), 1);
}

#[test]
fn ordinary_layout_edits_become_the_last_document_state() {
    for depth in [8, 16, 32] {
        for (command, mut params) in [
            ("layer.translate", json!({"dx": 9, "dy": 3})),
            ("layer.setProps", json!({"visible": false})),
            ("layer.setProps", json!({"opacity": 0.25})),
            ("layer.setProps", json!({"blend": "multiply"})),
        ] {
            let (mut s, a, _) = session(depth);
            s.execute("layerComp.new", json!({})).unwrap();
            params["layer"] = json!(a.0);
            s.execute(command, params).unwrap();
            let edited = capture_states(doc(&s));
            s.execute("layerComp.apply", json!({})).unwrap();
            assert_ne!(capture_states(doc(&s)), edited);
            assert!(s.is_enabled("layerComp.restoreLastDocumentState"));
            s.execute("layerComp.restoreLastDocumentState", json!({})).unwrap();
            assert_eq!(capture_states(doc(&s)), edited, "{depth}-bit");
        }
    }
}

#[test]
fn cycling_comps_keeps_the_latest_edited_layout() {
    let (mut s, a, _) = session(8);
    let one = s.execute("layerComp.new", json!({"name": "One"})).unwrap()["comp"].clone();
    s.execute("layer.translate", json!({"layer": a.0, "dx": 10, "dy": 0})).unwrap();
    let two = s.execute("layerComp.new", json!({"name": "Two"})).unwrap()["comp"].clone();
    for dx in [7, 13] {
        s.execute("layer.translate", json!({"layer": a.0, "dx": dx, "dy": 0})).unwrap();
        let edited = capture_states(doc(&s));
        s.execute("layerComp.apply", json!({"comp": one})).unwrap();
        s.execute("layerComp.apply", json!({"comp": two})).unwrap();
        s.execute("layerComp.apply", json!({"comp": two})).unwrap();
        s.execute("layerComp.restoreLastDocumentState", json!({})).unwrap();
        assert_eq!(capture_states(doc(&s)), edited);
    }
}

#[test]
fn unrecorded_properties_do_not_replace_the_backup_on_reapply() {
    let (mut s, a, _) = session(8);
    s.execute("layerComp.new", json!({"position": false, "appearance": false})).unwrap();
    s.execute("layer.translate", json!({"layer": a.0, "dx": 10, "dy": 0})).unwrap();
    s.execute("layer.setProps", json!({"layer": a.0, "visible": false, "opacity": 0.5})).unwrap();
    let edited = capture_states(doc(&s));
    s.execute("layerComp.apply", json!({})).unwrap();
    assert!(doc(&s).layer(a).unwrap().visible);
    s.execute("layerComp.apply", json!({})).unwrap();
    s.execute("layerComp.restoreLastDocumentState", json!({})).unwrap();
    assert_eq!(capture_states(doc(&s)), edited);
}

#[test]
fn manage_comps_and_cycle() {
    let (mut s, _, b) = session(8);
    let c1 = s.execute("layerComp.new", json!({"name": "One", "comment": "first"})).unwrap()["comp"].as_u64().unwrap();
    s.edit("hide", |d, _| {
        d.layer_mut(b).unwrap().visible = false;
        Ok(())
    })
    .unwrap();
    let c2 = s.execute("layerComp.new", json!({"name": "Two"})).unwrap()["comp"].as_u64().unwrap();
    let c3 = s.execute("layerComp.duplicate", json!({"comp": c1})).unwrap()["comp"].as_u64().unwrap();
    let names: Vec<_> = doc(&s).layer_comps.iter().map(|c| c.name.clone()).collect();
    assert_eq!(names, ["One", "One copy", "Two"]);
    s.execute("layerComp.rename", json!({"comp": c3, "name": "Three"})).unwrap();
    s.execute("layerComp.setComment", json!({"comp": "Three", "comment": "dup"})).unwrap();
    assert_eq!(doc(&s).comp(c3 as u32).unwrap().comment, "dup");
    // Next/previous cycle through the list order and wrap.
    s.execute("layerComp.apply", json!({"comp": c2})).unwrap();
    s.execute("layerComp.next", json!({})).unwrap();
    assert_eq!(doc(&s).last_applied_comp, Some(c1 as u32));
    assert!(doc(&s).layer(b).unwrap().visible);
    s.execute("layerComp.previous", json!({})).unwrap();
    assert_eq!(doc(&s).last_applied_comp, Some(c2 as u32));
    assert!(!doc(&s).layer(b).unwrap().visible);
    let l = s.execute("layerComp.list", json!({})).unwrap();
    assert_eq!(l["comps"].as_array().unwrap().len(), 3);
    // Update one property: the visibility recorded in "One" follows the document again.
    s.execute("layerComp.update", json!({"comp": c1, "what": "visibility"})).unwrap();
    assert_eq!(doc(&s).comp(c1 as u32).unwrap().state(b).unwrap().visible, Some(false));
    s.execute("layerComp.update", json!({"comp": "*"})).unwrap();
}

#[test]
fn warnings_for_deleted_layers() {
    let (mut s, a, _) = session(8);
    s.execute("layerComp.new", json!({})).unwrap();
    assert_eq!(s.execute("layerComp.updateWarnings", json!({})).unwrap()["count"], 0);
    s.edit("del", |d, _| {
        d.remove(a);
        Ok(())
    })
    .unwrap();
    let w = s.execute("layerComp.updateWarnings", json!({})).unwrap();
    assert_eq!(w["count"], 1);
    assert_eq!(w["warnings"][0]["missingLayers"][0], a.0);
    // Applying still works (missing layers are skipped) and reports them.
    assert_eq!(s.execute("layerComp.apply", json!({})).unwrap()["missingLayers"], 1);
    s.execute("layerComp.updateWarnings", json!({"clear": true})).unwrap();
    assert_eq!(s.execute("layerComp.updateWarnings", json!({})).unwrap()["count"], 0);
}

#[test]
fn disabled_states_and_bad_params() {
    let mut s = Session::new();
    assert!(!s.is_enabled("layerComp.new"));
    s.execute("file.new", json!({"width": 8, "height": 8})).unwrap();
    assert!(s.is_enabled("layerComp.new"));
    for id in
        ["layerComp.apply", "layerComp.update", "layerComp.next", "layerComp.delete", "file.export.layerCompsToFiles", "layerComp.restoreLastDocumentState"]
    {
        assert!(!s.is_enabled(id), "{id}");
    }
    s.execute("layerComp.new", json!({})).unwrap();
    assert!(s.execute("layerComp.apply", json!({"comp": 999})).is_err());
    assert!(s.execute("layerComp.apply", json!({"comp": "nope"})).is_err());
    assert!(s.execute("layerComp.update", json!({"what": "colour"})).is_err());
    assert!(s.execute("layerComp.rename", json!({})).is_err());
    assert!(s.execute("layerComp.setOptions", json!({})).is_err());
}

#[test]
fn export_comps_to_files_naming() {
    let (mut s, _, b) = session(8);
    s.execute("layerComp.new", json!({"name": "With B"})).unwrap();
    s.edit("hide", |d, _| {
        d.layer_mut(b).unwrap().visible = false;
        Ok(())
    })
    .unwrap();
    s.execute("layerComp.new", json!({"name": "No/B"})).unwrap();
    let dir = std::env::temp_dir().join(format!("photocraft-comps-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_string_lossy().into_owned();
    let r = s.execute("file.export.layerCompsToFiles", json!({"dir": d, "prefix": "doc"})).unwrap();
    let files: Vec<String> = r["files"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
    assert_eq!(files.len(), 2);
    assert!(files[0].ends_with("doc_0000_With B.png"), "{files:?}");
    assert!(files[1].ends_with("doc_0001_No_B.png"), "{files:?}");
    // Each file shows its comp: B's blue square only in the first.
    let blue = |path: &str| {
        let doc = photocraft_io::import("x.png", &std::fs::read(path).unwrap()).unwrap().document;
        photocraft_compose::render(&doc, Rect::new(35, 25, 36, 26)).px[0]
    };
    assert!(blue(&files[0])[2] > 0.9 && blue(&files[0])[0] < 0.1);
    assert!(blue(&files[1])[2] > 0.9 && blue(&files[1])[0] > 0.9);
    let r = s.execute("file.export.layerCompsToFiles", json!({"dir": d, "selectedOnly": true, "format": "jpg"})).unwrap();
    assert_eq!(r["files"].as_array().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn out_of_range_comp_ids_are_rejected_not_wrapped() {
    // #914: 2^32 + id used to wrap to a real comp.
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 16, "height": 16})).unwrap();
    let c = s.execute("layerComp.new", json!({"name": "Start"})).unwrap()["comp"].as_u64().unwrap();
    let wrapped = (1u64 << 32) + c;
    assert!(s.execute("layerComp.delete", json!({"comp": wrapped})).is_err());
    assert!(s.execute("layerComp.setOptions", json!({"comp": wrapped, "name": "renamed"})).is_err());
    let doc = &s.active().unwrap().doc;
    assert_eq!(doc.layer_comps.len(), 1);
    assert_eq!(doc.layer_comps[0].name, "Start");
    let dir = std::env::temp_dir().join(format!("photocraft-comps-wrap-{}", std::process::id()));
    let r = s.execute("file.export.layerCompsToFiles", json!({"dir": dir.to_string_lossy(), "comps": [wrapped]}));
    assert!(r.is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unreachable_comp_positions_leave_the_layer_in_place() {
    // #1017: a recorded position whose distance from the layer's current one leaves i32 range
    // (e.g. from a file) panicked on apply. The move is skipped; visibility still applies.
    for target in [(i32::MIN, 4), (4, i32::MIN)] {
        let (mut s, a, b) = session(8);
        let comp = LayerComp {
            id: 1,
            name: "Far".into(),
            comment: String::new(),
            apply_visibility: true,
            apply_position: true,
            apply_appearance: false,
            alternate: false,
            scope: Vec::new(),
            document_label: String::new(),
            states: vec![
                CompLayerState { layer: a, visible: Some(false), position: Some(target), appearance: None, placement: None },
                // Control: an ordinary position on another layer still moves it.
                CompLayerState { layer: b, visible: None, position: Some((31, 22)), appearance: None, placement: None },
            ],
        };
        s.edit("far", |d, _| {
            apply_comp(d, &comp, false);
            Ok(())
        })
        .unwrap();
        assert_eq!(pos(&s, a), Some((4, 4)), "{target:?}");
        assert!(!doc(&s).layer(a).unwrap().visible);
        assert_eq!(pos(&s, b), Some((31, 22)));
    }
}

#[test]
fn clearing_one_comps_warning_leaves_the_others() {
    // Regression for #4197: the per-comp warning icon cleared every comp's warnings.
    let (mut s, a, _) = session(8);
    let c1 = s.execute("layerComp.new", json!({"name": "One"})).unwrap()["comp"].as_u64().unwrap();
    let c2 = s.execute("layerComp.new", json!({"name": "Two"})).unwrap()["comp"].as_u64().unwrap();
    s.edit("del", |d, _| {
        d.remove(a);
        Ok(())
    })
    .unwrap();
    // Reporting can be limited to one comp too.
    let w = s.execute("layerComp.updateWarnings", json!({"comp": c2})).unwrap();
    assert_eq!(w["count"], 1);
    assert_eq!(w["warnings"][0]["comp"], c2);
    s.execute("layerComp.updateWarnings", json!({"clear": true, "comp": c1})).unwrap();
    assert!(doc(&s).comp(c1 as u32).unwrap().missing_layers(doc(&s)).is_empty());
    assert_eq!(doc(&s).comp(c2 as u32).unwrap().missing_layers(doc(&s)), vec![a]);
    let w = s.execute("layerComp.updateWarnings", json!({})).unwrap();
    assert_eq!(w["count"], 1);
    assert_eq!(w["warnings"][0]["comp"], c2);
    // Without "comp" every comp is cleared (Clear All Layer Comp Warnings).
    s.execute("layerComp.updateWarnings", json!({"clear": true})).unwrap();
    assert_eq!(s.execute("layerComp.updateWarnings", json!({})).unwrap()["count"], 0);
}

#[test]
fn clearing_warnings_of_an_unknown_comp_is_an_error() {
    let (mut s, a, _) = session(8);
    let c1 = s.execute("layerComp.new", json!({})).unwrap()["comp"].as_u64().unwrap();
    s.edit("del", |d, _| {
        d.remove(a);
        Ok(())
    })
    .unwrap();
    let before = doc(&s).layer_comps.clone();
    for bad in [json!(c1 + 99), json!("nope"), json!(-1), json!(1.5), json!(true), json!([c1])] {
        assert!(s.execute("layerComp.updateWarnings", json!({"clear": true, "comp": bad})).is_err(), "{bad}");
    }
    assert_eq!(doc(&s).layer_comps, before);
    assert_eq!(s.execute("layerComp.updateWarnings", json!({})).unwrap()["count"], 1);
}

#[test]
fn reorder_comps_preserves_capture_and_applied_state_and_undoes() {
    let (mut s, _, _) = session(8);
    for name in ["One", "Two", "Three"] {
        s.execute("layerComp.new", json!({"name":name})).unwrap();
    }
    let before = doc(&s).layer_comps.clone();
    let applied = doc(&s).last_applied_comp;
    let steps = s.active().unwrap().history.past_len();
    s.execute("layerComp.reorder", json!({"comp":"One", "index":2})).unwrap();
    assert_eq!(doc(&s).layer_comps, vec![before[1].clone(), before[2].clone(), before[0].clone()]);
    assert_eq!(doc(&s).last_applied_comp, applied);
    assert_eq!(s.active().unwrap().history.past_len(), steps + 1);
    assert!(s.undo());
    assert_eq!(doc(&s).layer_comps, before);
    assert!(s.redo());
    s.execute("layerComp.reorder", json!({"comp":before[0].id, "index":0})).unwrap();
    assert_eq!(doc(&s).layer_comps, before);
    let steps = s.active().unwrap().history.past_len();
    s.execute("layerComp.reorder", json!({"comp":"One", "index":0})).unwrap();
    assert_eq!(s.active().unwrap().history.past_len(), steps);
}

#[test]
fn reorder_comps_rejects_bad_indices_without_changes() {
    let mut s = Session::new();
    assert!(!s.is_enabled("layerComp.reorder"));
    s.execute("file.new", json!({"width":8, "height":8})).unwrap();
    assert!(!s.is_enabled("layerComp.reorder"));
    s.execute("layerComp.new", json!({"name":"Only"})).unwrap();
    let before = doc(&s).layer_comps.clone();
    let steps = s.active().unwrap().history.past_len();
    for index in [Value::Null, json!(-1), json!(1), json!(1.5), json!("0"), json!(u64::MAX)] {
        assert!(s.execute("layerComp.reorder", json!({"index":index})).is_err());
    }
    assert!(s.execute("layerComp.reorder", json!({"comp":"Missing", "index":0})).is_err());
    assert_eq!(doc(&s).layer_comps, before);
    assert_eq!(s.active().unwrap().history.past_len(), steps);
}

/// Apply Layer Comp is recorded by the comp's name, not its document-local id: played in another
/// document it applies that document's comp of the name, or stops when there is none (#7181).
#[test]
fn a_recorded_apply_layer_comp_finds_the_comp_by_name() {
    let (mut s, _, _) = session(8);
    s.execute("layerComp.new", json!({"name": "Wanted"})).unwrap();
    let wanted = doc(&s).layer_comps[0].id;
    s.execute("actions.record", json!({"name": "ApplyWanted"})).unwrap();
    s.execute("layerComp.apply", json!({"comp": wanted})).unwrap();
    s.execute("actions.stop", json!({})).unwrap();
    // `{}` and `null` apply the last applied comp; they are recorded by name too.
    s.execute("actions.record", json!({"name": "ApplyLast"})).unwrap();
    s.execute("layerComp.apply", json!({})).unwrap();
    s.execute("actions.stop", json!({})).unwrap();
    s.execute("actions.record", json!({"name": "ApplyNull"})).unwrap();
    s.execute("layerComp.apply", Value::Null).unwrap();
    s.execute("actions.stop", json!({})).unwrap();
    for action in ["ApplyWanted", "ApplyLast", "ApplyNull"] {
        let steps = s.execute("actions.get", json!({"action": action})).unwrap()["steps"].clone();
        assert_eq!(steps, json!([["layerComp.apply", {"comp": "Wanted"}]]), "{action}");
    }

    // A second document whose comp with the recorded id is another one.
    let (mut other, _, ob) = session(8);
    other.actions = s.actions.clone();
    let show = |s: &mut Session, visible: bool| {
        s.edit("t", |d, _| {
            d.layer_mut(ob).unwrap().visible = visible;
            Ok(())
        })
        .unwrap();
    };
    show(&mut other, false);
    other.execute("layerComp.new", json!({"name": "Different"})).unwrap();
    show(&mut other, true);
    other.execute("layerComp.new", json!({"name": "Wanted"})).unwrap();
    assert_eq!(doc(&other).layer_comps[0].id, wanted, "the recorded id names `Different` here");
    show(&mut other, false);
    other.execute("actions.play", json!({"action": "ApplyWanted"})).unwrap();
    let applied = doc(&other).last_applied_comp.and_then(|id| doc(&other).comp(id)).map(|c| c.name.clone());
    assert_eq!(applied.as_deref(), Some("Wanted"));
    assert!(doc(&other).layer(ob).unwrap().visible, "Wanted shows the layer");

    // A document without a comp of that name stops the action instead of applying another one.
    other.execute("layerComp.delete", json!({"comp": "Wanted"})).unwrap();
    show(&mut other, false);
    let before = (doc(&other).last_applied_comp, doc(&other).layer(ob).unwrap().visible);
    let played = other.execute("actions.play", json!({"action": "ApplyWanted"})).unwrap();
    assert_eq!(played["ran"], 0, "{played}");
    assert!(played["failed"]["error"].as_str().unwrap().contains("no layer comp named \"Wanted\""), "{played}");
    assert_eq!((doc(&other).last_applied_comp, doc(&other).layer(ob).unwrap().visible), before, "the document is unchanged");
}

// ─── Alternate layouts ──────────────────────────────────────────────────────────────────────

/// A document with raster A, B (see `session`), a smart object S and a paragraph type layer T,
/// plus an empty alternate layout "QR off".
fn layout_session() -> (Session, [LayerId; 4], u32) {
    use photocraft_doc::{SmartObject, SmartSource, TextLayer};
    let (mut s, a, b) = session(8);
    let (sm, t) = s
        .edit("setup", |doc, _| {
            let fmt = doc.pixel_format();
            let mut cache = photocraft_raster::Surface::new(fmt);
            cache.fill_rect(Rect::new(20, 10, 28, 14), &photocraft_raster::from_rgba(&fmt, [0.0, 1.0, 0.0, 1.0]));
            let so = SmartObject::new(SmartSource::Linked { path: "missing.png".into() }, Affine { m: [2.0, 0.0, 0.0, 2.0, 20.0, 10.0] }, Some(cache));
            let mut sl = Layer::new("S", LayerContent::Smart(so));
            sl.locks.position = true;
            let tl = Layer::new(
                "T",
                LayerContent::Text(TextLayer {
                    text: "Hi".into(),
                    transform: Affine::translate(8.0, 30.0),
                    shape: TextShape::Box { x: 0.0, y: 0.0, width: 40.0, height: 12.0 },
                    ..Default::default()
                }),
            );
            let ids = (sl.id, tl.id);
            doc.layers.push(sl);
            doc.layers.push(tl);
            Ok(ids)
        })
        .unwrap();
    let c = s.execute("layerComp.new", json!({"name": "QR off", "alternate": true})).unwrap()["comp"].as_u64().unwrap() as u32;
    (s, [a, b, sm, t], c)
}

fn placement_of(s: &Session, id: LayerId) -> Option<CompPlacement> {
    layer_placement(doc(s).layer(id).unwrap())
}

#[test]
fn an_alternate_layout_starts_empty_and_is_shown_not_applied() {
    let (mut s, [a, ..], c) = layout_session();
    let comp = doc(&s).comp(c).unwrap();
    assert!(comp.alternate && comp.states.is_empty() && !comp.apply_appearance);
    assert_eq!(doc(&s).last_applied_comp, None, "creating it shows nothing");
    let before = doc(&s).layers.clone();
    let r = s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    assert_eq!(r["alternate"], true);
    assert_eq!(doc(&s).comp_edit.as_ref().map(|e| e.comp), Some(c));
    assert_eq!(doc(&s).last_applied_comp, Some(c));
    assert_eq!(doc(&s).layers, before, "an empty layout moves nothing");
    assert_eq!(pos(&s, a), Some((4, 4)));
    let listed = s.execute("layerComp.list", json!({})).unwrap();
    assert_eq!(listed["showing"], c);
    assert_eq!(listed["comps"][0]["alternate"], true);
}

#[test]
fn edits_while_a_layout_is_shown_go_into_it_and_leaving_restores_the_document() {
    let (mut s, [a, b, sm, t], c) = layout_session();
    let base = doc(&s).layers.clone();
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    // Move A by whole pixels, hide B, scale and move S (position-locked: an edit, not the user's
    // drag, sets it here), widen T's box and move it.
    s.execute("layer.translate", json!({"layer": a.0, "dx": 3, "dy": 2})).unwrap();
    let moved_s = Affine { m: [3.0, 0.0, 0.0, 3.0, 12.5, 7.25] };
    let moved_t = Affine::translate(9.5, 33.0);
    s.edit("arrange", |d, _| {
        d.layer_mut(b).unwrap().visible = false;
        if let LayerContent::Smart(so) = &mut d.layer_mut(sm).unwrap().content {
            so.transform = moved_s;
        }
        if let LayerContent::Text(tx) = &mut d.layer_mut(t).unwrap().content {
            tx.transform = moved_t;
            tx.shape = TextShape::Box { x: 0.0, y: 0.0, width: 50.0, height: 12.0 };
        }
        Ok(())
    })
    .unwrap();
    let shown = doc(&s).layers.clone();
    s.execute("layerComp.restoreLastDocumentState", json!({})).unwrap();
    let d = doc(&s);
    assert!(d.comp_edit.is_none());
    assert_eq!(d.last_applied_comp, None);
    // The document's own arrangement is back, exactly.
    for (now, was) in d.layers.iter().zip(&base) {
        assert_eq!(now.visible, was.visible, "{}", now.name);
        assert_eq!(layer_placement(now), layer_placement(was), "{}", now.name);
    }
    // The layout holds what was shown, for the layers it moves only.
    let comp = d.comp(c).unwrap();
    assert_eq!(comp.states.len(), 4);
    let st = |id| comp.state(id).unwrap();
    assert_eq!(st(a).placement, Some(CompPlacement::Bounds(Rect::new(7, 6, 17, 16))));
    assert_eq!(st(a).visible, None);
    assert_eq!((st(b).visible, st(b).placement.clone()), (Some(false), None));
    assert_eq!(st(sm).placement, Some(CompPlacement::Transform { transform: moved_s, perspective: None, text_box: None }));
    assert_eq!(st(t).placement, Some(CompPlacement::Transform { transform: moved_t, perspective: None, text_box: Some([0.0, 0.0, 50.0, 12.0]) }));

    // Showing it again puts every layer back where it was shown, exactly, and leaving it
    // untouched records exactly the same.
    let recorded = comp.clone();
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    for id in [a, b, sm, t] {
        let now = doc(&s).layer(id).unwrap();
        let was = shown.iter().find(|l| l.id == id).unwrap();
        assert_eq!((now.visible, layer_placement(now)), (was.visible, layer_placement(was)), "{}", now.name);
    }
    s.execute("layerComp.restoreLastDocumentState", json!({})).unwrap();
    assert_eq!(doc(&s).comp(c).unwrap(), &recorded);

    // One history: undo walks back through leaving, showing and the edits.
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(doc(&s).comp_edit.as_ref().map(|e| e.comp), Some(c));
    assert_eq!(placement_of(&s, sm), Some(CompPlacement::Transform { transform: moved_s, perspective: None, text_box: None }));
    s.execute("edit.undo", json!({})).unwrap();
    assert!(doc(&s).comp_edit.is_none());
    assert_eq!(placement_of(&s, sm), layer_placement(base.iter().find(|l| l.id == sm).unwrap()));
}

#[test]
fn a_layer_nobody_moved_keeps_following_the_document() {
    let (mut s, [a, b, ..], c) = layout_session();
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    s.edit("hide", |d, _| {
        d.layer_mut(b).unwrap().visible = false;
        Ok(())
    })
    .unwrap();
    s.execute("layerComp.restoreLastDocumentState", json!({})).unwrap();
    assert!(doc(&s).comp(c).unwrap().state(a).is_none());
    // Moving A in the document moves it in the layout too.
    s.execute("layer.translate", json!({"layer": a.0, "dx": 5, "dy": 0})).unwrap();
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    assert_eq!(pos(&s, a), Some((9, 4)));
    assert!(!doc(&s).layer(b).unwrap().visible);
}

#[test]
fn a_layer_added_while_a_layout_is_shown_is_only_in_it() {
    let (mut s, _, c) = layout_session();
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    let added = LayerId(s.execute("layer.new.layer", json!({})).unwrap()["layer"].as_u64().unwrap());
    s.execute("layerComp.restoreLastDocumentState", json!({})).unwrap();
    assert!(!doc(&s).layer(added).unwrap().visible, "hidden in the document");
    assert_eq!(doc(&s).comp(c).unwrap().state(added).unwrap().visible, Some(true), "visible in the layout");
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    assert!(doc(&s).layer(added).unwrap().visible);
}

#[test]
fn delete_hides_a_layer_in_a_shown_layout_instead_of_deleting_it() {
    let (mut s, [a, ..], c) = layout_session();
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    let r = s.execute("layer.delete", json!({"layer": a.0})).unwrap();
    assert_eq!(r["hidden"], json!([a.0]));
    assert!(!doc(&s).layer(a).unwrap().visible);
    s.execute("layerComp.restoreLastDocumentState", json!({})).unwrap();
    assert!(doc(&s).layer(a).unwrap().visible, "still in the document");
    assert_eq!(doc(&s).comp(c).unwrap().state(a).unwrap().visible, Some(false));
    // Outside the layout Delete deletes.
    s.execute("layer.delete", json!({"layer": a.0})).unwrap();
    assert!(doc(&s).layer(a).is_none());
}

#[test]
fn applying_another_comp_or_deleting_the_layout_leaves_it_first() {
    let (mut s, [a, ..], c) = layout_session();
    let plain = s.execute("layerComp.new", json!({"name": "Plain"})).unwrap()["comp"].as_u64().unwrap();
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    s.execute("layer.translate", json!({"layer": a.0, "dx": 3, "dy": 0})).unwrap();
    s.execute("layerComp.apply", json!({"comp": plain})).unwrap();
    assert!(doc(&s).comp_edit.is_none());
    assert_eq!(pos(&s, a), Some((4, 4)), "the plain comp applies over the document's arrangement");
    assert!(doc(&s).comp(c).unwrap().state(a).is_some());
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    assert_eq!(pos(&s, a), Some((7, 4)));
    s.execute("layerComp.delete", json!({"comp": c})).unwrap();
    assert!(doc(&s).comp_edit.is_none());
    assert_eq!(pos(&s, a), Some((4, 4)));
}

#[test]
fn update_never_captures_into_an_alternate_layout() {
    let (mut s, _, c) = layout_session();
    let plain = s.execute("layerComp.new", json!({"name": "Plain"})).unwrap()["comp"].as_u64().unwrap();
    assert!(s.execute("layerComp.update", json!({"comp": c})).is_err());
    let r = s.execute("layerComp.update", json!({"comp": "*"})).unwrap();
    assert_eq!(r["updated"], json!([plain]));
    assert!(doc(&s).comp(c).unwrap().states.is_empty());
}

#[test]
fn a_save_while_a_layout_is_shown_writes_the_document_arrangement() {
    let (mut s, [a, b, sm, _], c) = layout_session();
    let base_a = pos(&s, a);
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    s.execute("layer.translate", json!({"layer": a.0, "dx": 3, "dy": 2})).unwrap();
    s.edit("hide", |d, _| {
        d.layer_mut(b).unwrap().visible = false;
        Ok(())
    })
    .unwrap();
    let bytes = photocraft_format::save_to_bytes(doc(&s), &photocraft_format::SaveOptions::default()).unwrap();
    // The live document still shows the layout.
    assert!(doc(&s).comp_edit.is_some());
    let back = photocraft_format::load_from_bytes(&bytes).unwrap();
    assert!(back.comp_edit.is_none());
    let la = back.layers.iter().find(|l| l.name == "A").unwrap();
    let lb = back.layers.iter().find(|l| l.name == "B").unwrap();
    assert_eq!(layer_position(la), base_a);
    assert!(lb.visible);
    let comp = back.comp(c).unwrap();
    assert!(comp.alternate);
    assert_eq!(comp.state(la.id).unwrap().placement, Some(CompPlacement::Bounds(Rect::new(7, 6, 17, 16))));
    assert_eq!(comp.state(lb.id).unwrap().visible, Some(false));
    assert!(back.layers.iter().find(|l| l.name == "S").is_some_and(|l| comp.state(l.id).is_none()));
    let _ = sm;
}

#[test]
fn a_raster_layer_takes_its_recorded_size_and_a_locked_one_moves() {
    let (mut s, [a, _, sm, _], c) = layout_session();
    s.edit("record", |d, _| {
        let comp = d.layer_comps.iter_mut().find(|x| x.id == c).unwrap();
        comp.states = vec![
            CompLayerState { layer: a, visible: None, position: None, appearance: None, placement: Some(CompPlacement::Bounds(Rect::new(10, 10, 30, 30))) },
            CompLayerState {
                layer: sm,
                visible: None,
                position: None,
                appearance: None,
                placement: Some(CompPlacement::Transform { transform: Affine { m: [0.0, 2.0, -2.0, 0.0, 40.0, 5.0] }, perspective: None, text_box: None }),
            },
        ];
        Ok(())
    })
    .unwrap();
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    assert_eq!(placement_of(&s, a), Some(CompPlacement::Bounds(Rect::new(10, 10, 30, 30))), "resampled onto the recorded bounds");
    assert_eq!(
        placement_of(&s, sm),
        Some(CompPlacement::Transform { transform: Affine { m: [0.0, 2.0, -2.0, 0.0, 40.0, 5.0] }, perspective: None, text_box: None })
    );
    assert!(doc(&s).layer(sm).unwrap().locks.position, "the lock is kept");
    s.execute("layerComp.restoreLastDocumentState", json!({})).unwrap();
    assert_eq!(placement_of(&s, a), Some(CompPlacement::Bounds(Rect::new(4, 4, 14, 14))));
}

#[test]
fn a_layer_left_where_the_layout_put_it_keeps_its_recorded_placement_exactly() {
    let (mut s, [a, ..], c) = layout_session();
    // Bounds no resample of this layer can reach exactly: showing lands near them.
    let recorded = CompPlacement::Bounds(Rect::new(10, 10, 27, 23));
    s.edit("record", |d, _| {
        let comp = d.layer_comps.iter_mut().find(|x| x.id == c).unwrap();
        comp.states = vec![CompLayerState { layer: a, visible: None, position: None, appearance: None, placement: Some(recorded.clone()) }];
        Ok(())
    })
    .unwrap();
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    let shown = placement_of(&s, a).unwrap();
    // Simulate a resample that lands a pixel off: the layout records what it had, not this.
    s.edit("land", |d, _| {
        d.comp_edit.as_mut().unwrap().shown = vec![(a, shown.clone())];
        Ok(())
    })
    .unwrap();
    s.execute("layerComp.restoreLastDocumentState", json!({})).unwrap();
    assert_eq!(doc(&s).comp(c).unwrap().state(a).unwrap().placement, Some(recorded.clone()));
    // Moved while shown: the new place is recorded.
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    s.execute("layer.translate", json!({"layer": a.0, "dx": 2, "dy": 0})).unwrap();
    let moved = placement_of(&s, a);
    s.execute("layerComp.restoreLastDocumentState", json!({})).unwrap();
    assert_eq!(doc(&s).comp(c).unwrap().state(a).unwrap().placement, moved);
    assert_ne!(moved, Some(recorded));
}

#[test]
fn painting_in_an_alternate_layout_is_refused_without_history_or_pixel_changes() {
    for rect in [Rect::new(15, 5, 18, 8), Rect::new(1, 1, 8, 8)] {
        let (mut s, [a, ..], c) = layout_session();
        s.execute("layerComp.apply", json!({"comp": c})).unwrap();
        s.execute("layer.translate", json!({"layer": a.0, "dx": 10, "dy": 0})).unwrap();
        let before = doc(&s).clone();
        let steps = s.active().unwrap().history.entries().len();
        let result = s.edit("Paint", |d, _| {
            let fmt = d.pixel_format();
            d.layer_mut(a).unwrap().surface_mut().unwrap().fill_rect(rect, &photocraft_raster::from_rgba(&fmt, [0.0, 1.0, 0.0, 1.0]));
            Ok(())
        });
        assert!(result.unwrap_err().to_string().contains("Back to the document"));
        assert_eq!(doc(&s), &before);
        assert_eq!(s.active().unwrap().history.entries().len(), steps);
    }
}

#[test]
fn mask_paint_in_an_alternate_layout_is_refused_atomically() {
    let (mut s, [a, ..], c) = layout_session();
    s.edit("mask", |d, _| {
        d.layer_mut(a).unwrap().mask = Some(photocraft_doc::LayerMask::reveal_all());
        Ok(())
    })
    .unwrap();
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    s.execute("layer.translate", json!({"layer": a.0, "dx": 10, "dy": 0})).unwrap();
    let before = doc(&s).clone();
    let steps = s.active().unwrap().history.entries().len();
    let result = s.edit("Paint mask", |d, _| {
        d.layer_mut(a).unwrap().mask.as_mut().unwrap().surface.fill_rect(Rect::new(15, 5, 18, 8), &[0.0]);
        Ok(())
    });
    assert!(result.unwrap_err().to_string().contains("Back to the document"));
    assert_eq!(doc(&s), &before);
    assert_eq!(s.active().unwrap().history.entries().len(), steps);
}

/// A group around raster A with a pixel mask and a vector mask (a COMPOSITE_GROUP's frame), an
/// alternate layout, and the group's id.
fn masked_group_session() -> (Session, LayerId, LayerId, u32) {
    use photocraft_doc::{LayerMask, Path, Subpath, VectorMask, vector::Knot};
    let (mut s, a, _) = session(8);
    let g = s
        .edit("group", |doc, _| {
            let i = doc.layers.iter().position(|l| l.id == a).unwrap();
            let inner = doc.layers.remove(i);
            let mut g = Layer::group("G", vec![inner]);
            let mut m = LayerMask::reveal_all();
            m.surface.fill_rect(Rect::new(2, 2, 20, 20), &[0.0]);
            g.mask = Some(m);
            let k = Knot::corner;
            g.vector_mask = Some(VectorMask {
                path: Path {
                    subpaths: vec![Subpath { knots: vec![k(3.0, 3.0), k(18.0, 3.0), k(18.0, 18.0)], closed: true, ..Default::default() }],
                    ..Default::default()
                },
                enabled: true,
                linked: true,
                density: 1.0,
                feather: 0.0,
            });
            let id = g.id;
            doc.layers.insert(i, g);
            Ok(id)
        })
        .unwrap();
    let c = s.execute("layerComp.new", json!({"name": "QR off", "alternate": true})).unwrap()["comp"].as_u64().unwrap() as u32;
    (s, g, a, c)
}

#[test]
fn moving_a_masked_group_in_a_layout_records_its_mask_frame_and_its_children() {
    let (mut s, g, a, c) = masked_group_session();
    let base_mask = doc(&s).layer(g).unwrap().vector_mask.clone();
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    // The Move tool on the group: its masks and its child move by whole pixels (the content
    // guard accepts that as placement).
    s.execute("layer.translate", json!({"layer": g.0, "dx": 5, "dy": 2})).unwrap();
    s.execute("layerComp.restoreLastDocumentState", json!({})).unwrap();
    let comp = doc(&s).comp(c).unwrap();
    assert_eq!(comp.state(g).unwrap().placement, Some(CompPlacement::Mask { bounds: [8.0, 5.0, 23.0, 20.0] }), "the group's own frame");
    assert_eq!(comp.state(a).unwrap().placement, Some(CompPlacement::Bounds(Rect::new(9, 6, 19, 16))), "and its child");
    assert_eq!(doc(&s).layer(g).unwrap().vector_mask, base_mask, "the document's mask is back");
    // Shown again, the masks go where the layout puts them.
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    assert_eq!(placement_of(&s, g), Some(CompPlacement::Mask { bounds: [8.0, 5.0, 23.0, 20.0] }));
    assert_eq!(placement_of(&s, a), Some(CompPlacement::Bounds(Rect::new(9, 6, 19, 16))));
}

#[test]
fn a_layout_with_a_scope_leaves_the_other_pages_to_the_document() {
    let mut s = Session::new();
    s.execute("file.new", json!({"width": 64, "height": 32})).unwrap();
    let one = LayerId(s.execute("layer.new.artboard", json!({"rect": [0, 0, 30, 30], "name": "One"})).unwrap()["layer"].as_u64().unwrap());
    let two = LayerId(s.execute("layer.new.artboard", json!({"rect": [34, 0, 30, 30], "name": "Two"})).unwrap()["layer"].as_u64().unwrap());
    let (in_one, in_two) = s
        .edit("layers", |doc, _| {
            let fmt = doc.pixel_format();
            let mut x = Layer::raster("x", fmt);
            x.surface_mut().unwrap().fill_rect(Rect::new(2, 2, 8, 8), &photocraft_raster::from_rgba(&fmt, [1.0, 0.0, 0.0, 1.0]));
            let mut y = Layer::raster("y", fmt);
            y.surface_mut().unwrap().fill_rect(Rect::new(36, 2, 42, 8), &photocraft_raster::from_rgba(&fmt, [0.0, 1.0, 0.0, 1.0]));
            let ids = (x.id, y.id);
            doc.layer_mut(one).unwrap().children_mut().unwrap().push(x);
            doc.layer_mut(two).unwrap().children_mut().unwrap().push(y);
            Ok(ids)
        })
        .unwrap();
    let c = s.execute("layerComp.new", json!({"name": "Without QR", "alternate": true, "scope": [one.0], "documentLabel": "With QR"})).unwrap()["comp"]
        .as_u64()
        .unwrap() as u32;
    assert!(s.execute("layerComp.new", json!({"alternate": true, "scope": [in_one.0]})).is_err(), "a scope names top-level layers");
    let listed = s.execute("layerComp.list", json!({})).unwrap();
    assert_eq!(listed["comps"][0]["scope"], json!([one.0]));
    assert_eq!(listed["comps"][0]["documentLabel"], "With QR");
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    s.execute("layer.translate", json!({"layer": in_one.0, "dx": 3, "dy": 0})).unwrap();
    // On the page it does not reach the document is edited as usual: a move stays, pixels may
    // be painted, and Delete deletes.
    s.execute("layer.translate", json!({"layer": in_two.0, "dx": 4, "dy": 0})).unwrap();
    s.edit("paint", |d, _| {
        let fmt = d.pixel_format();
        d.layer_mut(in_two).unwrap().surface_mut().unwrap().fill_rect(Rect::new(40, 10, 44, 14), &photocraft_raster::from_rgba(&fmt, [0.0, 0.0, 1.0, 1.0]));
        Ok(())
    })
    .unwrap();
    s.execute("layerComp.restoreLastDocumentState", json!({})).unwrap();
    let comp = doc(&s).comp(c).unwrap();
    assert!(comp.state(in_one).is_some());
    assert!(comp.state(in_two).is_none(), "nothing is recorded outside the scope");
    assert_eq!(pos(&s, in_one), Some((2, 2)), "inside: the document's place is back");
    assert_eq!(pos(&s, in_two), Some((40, 2)), "outside: the edit stays");
    s.execute("layerComp.apply", json!({"comp": c})).unwrap();
    s.execute("layer.delete", json!({"layer": in_two.0})).unwrap();
    assert!(doc(&s).layer(in_two).is_none(), "outside the scope Delete deletes");
}
