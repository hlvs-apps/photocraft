//! Window › Layer Comps: capture, apply and manage layer comps (`layerComp.*`), plus
//! File › Export › Layer Comps to Files.
//!
//! A comp records every layer's visibility, position and appearance; its three options choose
//! which of them applying restores (see [`photocraft_doc::comps`]). Comps live in the document,
//! so every change here is one undoable history step. Commands that act on "the" comp take
//! `"comp": id | name` and default to the comp applied (or created) last.
//!
//! An alternate layout (`LayerComp::alternate`) is shown rather than applied: `layerComp.apply`
//! shows it ([`show_layout`]), edits to visibility and placement go into it while it is shown,
//! and `layerComp.restoreLastDocumentState` (or applying any other comp) leaves it
//! (`Document::leave_comp_edit`). Delete Layer hides a layer in it instead of deleting it.

use photocraft_algo::transform::{Homography, Interp, warp_surface};
use photocraft_doc::comps::{CompPlacement, capture_states, layer_placement, layer_position, mask_bounds, next_comp_id, position_bounds};
use photocraft_doc::text::TextShape;
use photocraft_doc::{CompEdit, CompLayerState, Document, Layer, LayerComp, LayerContent, LayerId, Locks};
use photocraft_geom::Affine;
use serde_json::{Value, json};

use crate::commands::CommandSpec;
use crate::file_cmds::{SaveOpts, join, native_doc, sanitize, save_doc, stem, str_param};
use crate::{EngineError, Result, Session};

fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}

fn has_doc(s: &Session) -> std::result::Result<(), String> {
    s.active().map(|_| ()).ok_or_else(|| "no document open".into())
}

fn has_comps(s: &Session) -> std::result::Result<(), String> {
    let d = s.active().ok_or("no document open")?;
    if d.doc.layer_comps.is_empty() { Err("the document has no layer comps".into()) } else { Ok(()) }
}

fn has_last_state(s: &Session) -> std::result::Result<(), String> {
    let d = s.active().ok_or("no document open")?;
    if d.doc.last_document_state.is_some() || d.doc.comp_edit.is_some() { Ok(()) } else { Err("no comp has been applied over the document state".into()) }
}

fn export_comps(s: &Session) -> std::result::Result<(), String> {
    native_doc(s)?;
    has_comps(s)
}

/// The comp a command acts on: `"comp"` as an id or a name, else the last applied comp.
fn comp_param(doc: &Document, p: &Value, cmd: &str) -> Result<u32> {
    match p.get("comp") {
        Some(v @ Value::Number(_)) => {
            let id = crate::commands::u32_id(cmd, "comp", v)?;
            doc.comp(id).map(|c| c.id).ok_or_else(|| bad(cmd, format!("no layer comp with id {id}")))
        }
        Some(Value::String(name)) => {
            doc.layer_comps.iter().find(|c| c.name == *name).map(|c| c.id).ok_or_else(|| bad(cmd, format!("no layer comp named \"{name}\"")))
        }
        Some(_) => Err(bad(cmd, "\"comp\" must be a comp id or name")),
        None => doc
            .last_applied_comp
            .filter(|id| doc.comp(*id).is_some())
            .or_else(|| doc.layer_comps.first().map(|c| c.id))
            .ok_or_else(|| bad(cmd, "the document has no layer comps")),
    }
}

fn index_of(doc: &Document, id: u32) -> usize {
    doc.layer_comps.iter().position(|c| c.id == id).unwrap_or(0)
}

fn flag(p: &Value, key: &str, default: bool) -> bool {
    p.get(key).and_then(Value::as_bool).unwrap_or(default)
}

/// Restores `comp`'s recorded state (only what its options select; `all` forces everything).
/// Layers deleted since the capture are skipped. Position-locked layers still move, as in
/// Photoshop, because the comp is not a user drag. An alternate layout's exact placements go
/// with "position" ([`apply_placement`]).
pub fn apply_comp(doc: &mut Document, comp: &LayerComp, all: bool) {
    let (vis, pos, app) = if all { (true, true, true) } else { (comp.apply_visibility, comp.apply_position, comp.apply_appearance) };
    // Walk order (parents before children): a moved artboard carries its children, which are
    // then placed absolutely, so the result does not depend on how far the parent moved.
    let order: Vec<LayerId> = doc.walk().into_iter().map(|(_, _, l)| l.id).collect();
    let mut placed = Vec::new();
    for id in order {
        let Some(st) = comp.state(id) else { continue };
        // Outside an alternate layout's reach the document stays as it is.
        if !comp.applies_to(doc, id) {
            continue;
        }
        if pos && let Some(p) = &st.placement {
            // A placement the layer cannot take (another kind of layer now, a degenerate
            // transform, a pixel budget) leaves it where it is: a comp never fails to apply.
            if apply_placement(doc, id, p).unwrap_or(false) {
                placed.push(id);
            }
        }
        if pos
            && let Some((x, y)) = st.position
            && let Some((cx, cy)) = doc.layer(id).and_then(layer_position)
            && (x, y) != (cx, cy)
            // Recorded positions can come from files; a move that does not fit an `i32` is
            // skipped rather than wrapped onto some other position (#1017).
            && let (Some(dx), Some(dy)) = (x.checked_sub(cx), y.checked_sub(cy))
        {
            let snapshot = doc.clone();
            if let Some(l) = doc.layer_mut(id) {
                crate::commands::translate_layer(&snapshot, l, dx, dy);
                crate::vector_cmds::translate_vectors(&snapshot, l, f64::from(dx), f64::from(dy));
            }
        }
        let Some(l) = doc.layer_mut(id) else { continue };
        if vis && let Some(v) = st.visible {
            l.visible = v;
        }
        if app && let Some(a) = &st.appearance {
            l.blend = a.blend;
            l.opacity = a.opacity;
            l.fill_opacity = a.fill_opacity;
            l.effects = a.effects.clone();
        }
    }
    if !placed.is_empty() {
        // Type and shapes draw again from their new placement (smart objects re-render as after
        // any transform, keeping their warped rendering when the source cannot).
        let snapshot = doc.clone();
        for id in placed {
            if let Some(l) = doc.layer_mut(id) {
                crate::transform_cmds::refresh_text(&snapshot, l);
            }
        }
    }
}

/// The homogeneous form of an affine map.
fn homography(a: &Affine) -> Homography {
    let [a, b, c, d, e, f] = a.m;
    Homography([a, c, e, b, d, f, 0.0, 0.0, 1.0])
}

/// The map of the document that takes layer `l` from where it is to placement `p` (document
/// pixels to document pixels), with its affine form when it has one. None when `l` cannot take
/// `p`: another kind of layer, nothing to place, a degenerate transform.
fn placement_step(l: &Layer, p: &CompPlacement) -> Option<(Homography, Option<Affine>)> {
    if let CompPlacement::Mask { bounds } = p {
        let [x0, y0, x1, y1] = mask_bounds(l)?;
        let [u0, v0, u1, v1] = *bounds;
        let (sx, sy) = ((u1 - u0) / (x1 - x0), (v1 - v0) / (y1 - y0));
        if !(sx.is_finite() && sy.is_finite() && sx > 0.0 && sy > 0.0) {
            return None;
        }
        let a = Affine { m: [sx, 0.0, 0.0, sy, u0 - x0 * sx, v0 - y0 * sy] };
        return Some((homography(&a), Some(a)));
    }
    match (p, &l.content) {
        (CompPlacement::Transform { transform, perspective, .. }, LayerContent::Smart(sm)) => {
            let from = crate::smart_cmds::placement(sm);
            let to = perspective.map(Homography).unwrap_or_else(|| homography(transform));
            let h = to.mul(&from.inverse()?);
            let affine = match (perspective, sm.perspective) {
                (None, None) => Some(transform.mul(&sm.transform.inverse()?)),
                _ => None,
            };
            Some((h, affine))
        }
        (CompPlacement::Transform { transform, .. }, LayerContent::Text(t)) => {
            let a = transform.mul(&t.transform.inverse()?);
            Some((homography(&a), Some(a)))
        }
        (CompPlacement::Bounds(r), LayerContent::Raster(_) | LayerContent::Shape(_)) => {
            let cur = position_bounds(l);
            if cur.is_empty() || r.is_empty() {
                return None;
            }
            let (sx, sy) = (f64::from(r.width()) / f64::from(cur.width()), f64::from(r.height()) / f64::from(cur.height()));
            let a = Affine { m: [sx, 0.0, 0.0, sy, f64::from(r.x0) - f64::from(cur.x0) * sx, f64::from(r.y0) - f64::from(cur.y0) * sy] };
            Some((homography(&a), Some(a)))
        }
        _ => None,
    }
}

/// Puts layer `id` at placement `p` exactly: a smart object's or type layer's transform becomes
/// the recorded one (its masks, filter mask and rendering moved along, as Free Transform moves
/// them), a raster or shape layer is transformed onto the recorded bounds. Locks do not stop it.
/// Returns whether the layer moved. The caller re-renders type and shapes.
pub fn apply_placement(doc: &mut Document, id: LayerId, p: &CompPlacement) -> Result<bool> {
    let l = doc.layer(id).ok_or(EngineError::NoLayer(id))?;
    if layer_placement(l).as_ref() == Some(p) {
        return Ok(false);
    }
    let Some((h, affine)) = placement_step(l, p) else { return Ok(false) };
    let l = doc.layer_mut(id).ok_or(EngineError::NoLayer(id))?;
    if let (CompPlacement::Mask { .. }, Some(a)) = (p, affine) {
        // Only the layer's own masks: a group's children have states of their own.
        move_own_masks(l, &h, &a)?;
        return Ok(true);
    }
    let taken = crate::smart_cmds::take_caches(std::slice::from_mut(l));
    let locks = std::mem::take(&mut l.locks);
    let moved = crate::transform_cmds::transform_layer(None, Locks::default(), l, &h, affine, Interp::Bicubic);
    l.locks = locks;
    moved?;
    // The recorded values themselves, not the product's rounding: showing an alternate layout
    // and leaving it untouched records exactly what it had.
    match (p, &mut l.content) {
        (CompPlacement::Transform { transform, perspective, .. }, LayerContent::Smart(sm)) => {
            sm.transform = *transform;
            sm.perspective = *perspective;
        }
        (CompPlacement::Transform { transform, text_box, .. }, LayerContent::Text(t)) => {
            t.transform = *transform;
            if let (Some([x, y, width, height]), TextShape::Box { .. }) = (text_box, &t.shape) {
                t.shape = TextShape::Box { x: *x, y: *y, width: *width, height: *height };
            }
            t.cache = None;
        }
        _ => {}
    }
    crate::smart_cmds::restore_unrendered(doc, taken, |c| warp_surface(c, c.content_bounds(), &h, Interp::Bicubic))?;
    Ok(true)
}

/// Reject content mutations that leaving an alternate layout would discard. This runs
/// against a candidate document before it enters history, including direct Session edits.
/// A placement change is allowed only when its pixels/masks are exactly that transform
/// of the previous content. Shared text and style edits are unaffected.
pub(crate) fn validate_layout_edit(before: &Document, after: &Document) -> Result<()> {
    let Some(edit) = &before.comp_edit else { return Ok(()) };
    if after.comp_edit.as_ref().map(|e| e.comp) != Some(edit.comp) {
        return Ok(());
    }
    fn content_at_risk(a: &Layer, b: &Layer) -> bool {
        let pixels = match (&a.content, &b.content) {
            (LayerContent::Raster(a), LayerContent::Raster(b)) => a != b,
            (LayerContent::Shape(a), LayerContent::Shape(b)) => a != b,
            (LayerContent::Smart(a), LayerContent::Smart(b)) => a.filter_mask != b.filter_mask,
            _ => false,
        };
        pixels || a.mask != b.mask || a.vector_mask != b.vector_mask
    }
    let comp = before.editing_comp();
    for (_, _, layer) in after.walk() {
        let Some(old) = before.layer(layer.id) else { continue };
        // Outside the layout's reach the document is edited as usual.
        if !content_at_risk(old, layer) || comp.is_some_and(|c| !c.applies_to(before, layer.id)) {
            continue;
        }
        let mut expected = before.clone();
        if let Some(placement) = layer_placement(layer) {
            // Integer translation is lossless; resampling it again changes edge pixels.
            let shift = match (layer_placement(old), &placement) {
                (Some(CompPlacement::Bounds(a)), CompPlacement::Bounds(b)) if a.width() == b.width() && a.height() == b.height() => {
                    b.x0.checked_sub(a.x0).zip(b.y0.checked_sub(a.y0))
                }
                // A group's masks moved by whole pixels (Move tool): the same lossless shift.
                (Some(CompPlacement::Mask { bounds: a }), CompPlacement::Mask { bounds: b }) => {
                    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
                    let same = (b[2] - b[0] - (a[2] - a[0])).abs() < 1e-9 && (b[3] - b[1] - (a[3] - a[1])).abs() < 1e-9;
                    (same && dx.fract() == 0.0 && dy.fract() == 0.0 && dx.abs() < 1e9 && dy.abs() < 1e9).then_some((dx as i32, dy as i32))
                }
                _ => None,
            };
            if let Some((dx, dy)) = shift {
                if let Some(l) = expected.layer_mut(layer.id) {
                    crate::commands::translate_layer(before, l, dx, dy);
                    crate::vector_cmds::translate_vectors(before, l, f64::from(dx), f64::from(dy));
                }
            } else {
                apply_placement(&mut expected, layer.id, &placement)?;
            }
        }
        if expected.layer(layer.id).is_some_and(|l| content_at_risk(l, layer)) {
            return Err(EngineError::Other(
                "Pixel, shape and mask edits cannot be kept while an alternate layout is shown. Choose Back to the document, then make this edit.".into(),
            ));
        }
    }
    Ok(())
}

/// Moves `l`'s own pixel and vector masks by `h` (its affine form `a`), nothing else.
fn move_own_masks(l: &mut Layer, h: &Homography, a: &Affine) -> Result<()> {
    if let Some(m) = l.mask.as_mut() {
        m.surface = crate::transform_cmds::warp_gray(&m.surface, h, Interp::Bicubic)?;
    }
    if let Some(vm) = l.vector_mask.as_mut() {
        vm.path = vm.path.transform(a);
    }
    if let Some((x, y)) = l.effects.reference {
        let p = a.apply(photocraft_geom::Point::new(x, y));
        l.effects.reference = Some((p.x, p.y));
    }
    Ok(())
}

/// Shows alternate layout `comp` (see the module docs): leaves the one shown, keeps every layer
/// as the document has it, and puts the layers where the layout records them.
fn show_layout(doc: &mut Document, comp: &LayerComp) {
    doc.leave_comp_edit();
    let base = doc
        .walk()
        .into_iter()
        .map(|(_, _, l)| {
            let mut b = l.clone();
            if let Some(ch) = b.children_mut() {
                ch.clear();
            }
            b
        })
        .collect();
    apply_comp(doc, comp, false);
    let shown =
        comp.states.iter().filter(|s| s.placement.is_some()).filter_map(|s| doc.layer(s.layer).and_then(layer_placement).map(|p| (s.layer, p))).collect();
    doc.comp_edit = Some(CompEdit { comp: comp.id, base, shown });
    doc.last_applied_comp = Some(comp.id);
}

/// Delete Layer while an alternate layout is shown: the layers are hidden in the layout, never
/// deleted from the document (a layer deleted from one arrangement would be gone from both).
/// Leave the layout to delete.
/// None when no target is within the shown layout's reach (outside it Delete deletes).
pub(crate) fn hide_in_layout(s: &mut Session, p: &Value) -> Result<Option<Value>> {
    let ids = crate::layer_multi_cmds::targets(s, p)?;
    let doc = &s.active().ok_or(EngineError::NoDocument)?.doc;
    let Some(comp) = doc.editing_comp() else { return Ok(None) };
    let ids: Vec<LayerId> = ids.into_iter().filter(|id| comp.applies_to(doc, *id)).collect();
    if ids.is_empty() {
        return Ok(None);
    }
    s.edit("Hide Layer in Alternate Layout", |doc, _| {
        for id in &ids {
            doc.layer_mut(*id).ok_or(EngineError::NoLayer(*id))?.visible = false;
        }
        Ok(())
    })?;
    Ok(Some(json!({"hidden": ids.iter().map(|l| l.0).collect::<Vec<_>>()})))
}

fn new_comp(s: &mut Session, p: &Value) -> Result<Value> {
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let name = p.get("name").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| {
        let names: Vec<&str> = d.doc.layer_comps.iter().map(|c| c.name.as_str()).collect();
        // `names.len() + 1` candidates always include a free one.
        (1..=names.len() + 1).map(|n| format!("Layer Comp {n}")).find(|n| !names.contains(&n.as_str())).unwrap_or_else(|| "Layer Comp".into())
    });
    let comment = p.get("comment").and_then(Value::as_str).unwrap_or_default().to_string();
    let alternate = flag(p, "alternate", false);
    let document_label = p.get("documentLabel").and_then(Value::as_str).unwrap_or_default().to_string();
    let scope: Vec<LayerId> = match p.get("scope") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) if a.len() <= 10_000 => {
            let mut ids = Vec::with_capacity(a.len());
            for v in a {
                let id = v.as_u64().map(LayerId).ok_or_else(|| bad("layerComp.new", "`scope` holds top-level layer ids"))?;
                if !d.doc.layers.iter().any(|l| l.id == id) {
                    return Err(bad("layerComp.new", format!("layer {} is not a top-level layer of the document", id.0)));
                }
                ids.push(id);
            }
            ids
        }
        Some(_) => return Err(bad("layerComp.new", "`scope` is a list of top-level layer ids")),
    };
    // An alternate layout starts empty (every layer where the document has it) and never carries
    // appearance.
    let (v, pos, a) = (flag(p, "visibility", true), flag(p, "position", true), !alternate && flag(p, "appearance", true));
    let id = s.edit("New Layer Comp", |doc, _| {
        let id = next_comp_id(doc);
        let states = if alternate { Vec::new() } else { capture_states(doc) };
        let comp = LayerComp { id, name, comment, apply_visibility: v, apply_position: pos, apply_appearance: a, alternate, scope, document_label, states };
        doc.layer_comps.push(comp);
        if !alternate {
            doc.last_applied_comp = Some(id);
        }
        Ok(id)
    })?;
    Ok(json!({"comp": id}))
}

fn update_comp(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "layerComp.update";
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let what = p.get("what").and_then(Value::as_str).unwrap_or("all");
    if !matches!(what, "all" | "visibility" | "position" | "appearance") {
        return Err(bad(cmd, format!("unknown \"what\" `{what}` (all|visibility|position|appearance)")));
    }
    let every = p.get("comp").and_then(Value::as_str) == Some("*");
    // An alternate layout records itself while it is shown; capturing every layer's state into
    // it would stop the layers it does not move from following the document.
    let ids: Vec<u32> = if every {
        d.doc.layer_comps.iter().filter(|c| !c.alternate).map(|c| c.id).collect()
    } else {
        let id = comp_param(&d.doc, p, cmd)?;
        if d.doc.comp(id).is_some_and(|c| c.alternate) {
            return Err(bad(cmd, "an alternate layout records what it shows: show it (Apply), change it, then return to the document"));
        }
        vec![id]
    };
    let label = match what {
        "visibility" => "Update Layer Comp Visibility",
        "position" => "Update Layer Comp Position",
        "appearance" => "Update Layer Comp Appearance",
        _ => "Update Layer Comp",
    };
    s.edit(label, |doc, _| {
        let now = capture_states(doc);
        for id in &ids {
            let Some(c) = doc.layer_comps.iter_mut().find(|c| c.id == *id) else { continue };
            if what == "all" {
                c.states = now.clone();
                continue;
            }
            // One property: refresh it on every live layer, keep the others as recorded.
            let mut states: Vec<CompLayerState> = Vec::with_capacity(now.len());
            for n in &now {
                let mut st =
                    c.state(n.layer).cloned().unwrap_or(CompLayerState { layer: n.layer, visible: None, position: None, appearance: None, placement: None });
                match what {
                    "visibility" => st.visible = n.visible,
                    "position" => st.position = n.position,
                    _ => st.appearance = n.appearance.clone(),
                }
                states.push(st);
            }
            c.states = states;
        }
        Ok(())
    })?;
    Ok(json!({"updated": ids}))
}

/// Whether the live layout still matches the properties the last applied comp restores.
/// Compare only on comp application: ordinary edits must not scan pixel bounds each frame.
fn matches_last_comp(doc: &Document, now: &[CompLayerState]) -> bool {
    let Some(comp) = doc.last_applied_comp.and_then(|id| doc.comp(id)) else { return false };
    let saved: std::collections::HashMap<_, _> = comp.states.iter().map(|st| (st.layer, st)).collect();
    now.len() == saved.len()
        && now.iter().all(|live| {
            saved.get(&live.layer).is_some_and(|saved| {
                (!comp.apply_visibility || saved.visible.is_none() || saved.visible == live.visible)
                    && (!comp.apply_position || saved.position.is_none() || saved.position == live.position)
                    && (!comp.apply_appearance || saved.appearance.is_none() || saved.appearance == live.appearance)
            })
        })
}

fn apply(s: &mut Session, id: u32, label: &str) -> Result<Value> {
    if s.active().and_then(|d| d.doc.comp(id)).is_some_and(|c| c.alternate) {
        s.edit("Show Alternate Layout", |doc, _| {
            let comp = doc.comp(id).cloned().ok_or_else(|| EngineError::Other(format!("no layer comp with id {id}")))?;
            show_layout(doc, &comp);
            Ok(())
        })?;
        let d = s.active().ok_or(EngineError::NoDocument)?;
        let missing = d.doc.comp(id).map(|c| c.missing_layers(&d.doc).len()).unwrap_or(0);
        return Ok(json!({"comp": id, "missingLayers": missing, "alternate": true}));
    }
    s.edit(label, |doc, _| {
        let comp = doc.comp(id).cloned().ok_or_else(|| EngineError::Other(format!("no layer comp with id {id}")))?;
        // Any other comp applies over the document's own arrangement.
        doc.leave_comp_edit();
        // Ordinary edits can leave last_applied_comp pointing at an outdated layout (#919).
        // Preserve the edited layout, but keep the original backup while cycling unedited comps.
        let states = capture_states(doc);
        if !matches_last_comp(doc, &states) {
            doc.last_document_state = Some(LayerComp {
                id: 0,
                name: "Last Document State".into(),
                comment: String::new(),
                apply_visibility: true,
                apply_position: true,
                apply_appearance: true,
                alternate: false,
                scope: Vec::new(),
                document_label: String::new(),
                states,
            });
        }
        apply_comp(doc, &comp, false);
        doc.last_applied_comp = Some(id);
        Ok(())
    })?;
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let missing = d.doc.comp(id).map(|c| c.missing_layers(&d.doc).len()).unwrap_or(0);
    Ok(json!({"comp": id, "missingLayers": missing}))
}

fn apply_cmd(s: &mut Session, p: &Value) -> Result<Value> {
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let id = comp_param(&d.doc, p, "layerComp.apply")?;
    apply(s, id, "Apply Layer Comp")
}

/// How an action records Apply Layer Comp: by the comp's name, as Photoshop does. Comp ids are
/// local to a document, so a recorded id would apply whichever comp of another document has it
/// (#7181); by name the step finds that document's comp of the name, or stops with an error.
/// The comp is the one just applied, so `{}` and `null` (the last applied) record its name too.
pub(crate) fn recorded_apply(s: &Session, mut p: Value) -> Value {
    let Some(d) = s.active() else { return p };
    let Some(comp) = d.doc.last_applied_comp.and_then(|id| d.doc.comp(id)) else { return p };
    // `null` params run as `{}` (the last applied comp), so they are recorded by name too.
    if p.is_null() {
        p = json!({});
    }
    if let Value::Object(m) = &mut p {
        m.insert("comp".into(), json!(comp.name));
    }
    p
}

fn step(s: &mut Session, delta: i64) -> Result<Value> {
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let n = d.doc.layer_comps.len() as i64;
    let cur = d.doc.last_applied_comp.and_then(|id| d.doc.layer_comps.iter().position(|c| c.id == id));
    let next = match cur {
        Some(i) => (i as i64 + delta).rem_euclid(n),
        None if delta > 0 => 0,
        None => n - 1,
    };
    let id = d.doc.layer_comps[next as usize].id;
    apply(s, id, "Apply Layer Comp")
}

fn delete(s: &mut Session, p: &Value) -> Result<Value> {
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let id = comp_param(&d.doc, p, "layerComp.delete")?;
    s.edit("Delete Layer Comp", |doc, _| {
        if doc.comp_edit.as_ref().is_some_and(|e| e.comp == id) {
            doc.leave_comp_edit();
        }
        doc.layer_comps.retain(|c| c.id != id);
        if doc.last_applied_comp == Some(id) {
            doc.last_applied_comp = None;
        }
        Ok(())
    })?;
    Ok(json!({"deleted": id}))
}

fn reorder(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "layerComp.reorder";
    let doc = &s.active().ok_or(EngineError::NoDocument)?.doc;
    let id = comp_param(doc, p, CMD)?;
    let index = p
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|i| *i < doc.layer_comps.len())
        .ok_or_else(|| bad(CMD, "index must name a position in the comp list"))?;
    let from = index_of(doc, id);
    if from != index {
        s.edit("Reorder Layer Comp", |doc, _| {
            let comp = doc.layer_comps.remove(from);
            doc.layer_comps.insert(index, comp);
            Ok(())
        })?;
    }
    Ok(json!({"comp": id, "index": index}))
}

fn duplicate(s: &mut Session, p: &Value) -> Result<Value> {
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let id = comp_param(&d.doc, p, "layerComp.duplicate")?;
    let new = s.edit("Duplicate Layer Comp", |doc, _| {
        let i = index_of(doc, id);
        let mut c = doc.layer_comps[i].clone();
        c.id = next_comp_id(doc);
        c.name = format!("{} copy", c.name);
        let nid = c.id;
        doc.layer_comps.insert(i + 1, c);
        Ok(nid)
    })?;
    Ok(json!({"comp": new}))
}

fn rename(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "layerComp.rename";
    let name = str_param(p, "name", cmd)?.to_string();
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let id = comp_param(&d.doc, p, cmd)?;
    s.edit("Rename Layer Comp", |doc, _| {
        let i = index_of(doc, id);
        doc.layer_comps[i].name = name;
        Ok(())
    })?;
    Ok(json!({"comp": id}))
}

fn set_comment(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "layerComp.setComment";
    let comment = p.get("comment").and_then(Value::as_str).ok_or_else(|| bad(cmd, "missing \"comment\""))?.to_string();
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let id = comp_param(&d.doc, p, cmd)?;
    s.edit("Layer Comp Options", |doc, _| {
        let i = index_of(doc, id);
        doc.layer_comps[i].comment = comment;
        Ok(())
    })?;
    Ok(json!({"comp": id}))
}

fn set_options(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "layerComp.setOptions";
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let id = comp_param(&d.doc, p, cmd)?;
    let get = |k: &str| p.get(k).and_then(Value::as_bool);
    let (v, pos, a, name) = (get("visibility"), get("position"), get("appearance"), p.get("name").and_then(Value::as_str).map(str::to_string));
    if v.is_none() && pos.is_none() && a.is_none() && name.is_none() && p.get("comment").is_none() {
        return Err(bad(cmd, "give \"visibility\", \"position\", \"appearance\", \"name\" or \"comment\""));
    }
    let comment = p.get("comment").and_then(Value::as_str).map(str::to_string);
    s.edit("Layer Comp Options", |doc, _| {
        let i = index_of(doc, id);
        let c = &mut doc.layer_comps[i];
        if let Some(v) = v {
            c.apply_visibility = v;
        }
        if let Some(v) = pos {
            c.apply_position = v;
        }
        if let Some(v) = a {
            c.apply_appearance = v;
        }
        if let Some(n) = name {
            c.name = n;
        }
        if let Some(n) = comment {
            c.comment = n;
        }
        Ok(())
    })?;
    Ok(json!({"comp": id}))
}

fn restore_last(s: &mut Session) -> Result<Value> {
    if s.active().is_some_and(|d| d.doc.comp_edit.is_some()) {
        s.edit("Leave Alternate Layout", |doc, _| {
            doc.leave_comp_edit();
            Ok(())
        })?;
        return Ok(Value::Null);
    }
    s.edit("Restore Last Document State", |doc, _| {
        let last = doc.last_document_state.clone().ok_or_else(|| EngineError::Other("no Last Document State".into()))?;
        apply_comp(doc, &last, true);
        doc.last_applied_comp = None;
        Ok(())
    })?;
    Ok(Value::Null)
}

/// Comps that refer to deleted layers (Photoshop's warning triangle). `"clear": true` drops
/// those stale states as one history step: from `"comp"` only when given (Clear Layer Comp
/// Warning), else from every comp (Clear All Layer Comp Warnings). `"comp"` also limits the
/// report to that comp; an unknown comp is an error and changes nothing.
fn update_warnings(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "layerComp.updateWarnings";
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let only = match p.get("comp") {
        None | Some(Value::Null) => None,
        Some(_) => Some(comp_param(&d.doc, p, cmd)?),
    };
    let warnings: Vec<Value> = d
        .doc
        .layer_comps
        .iter()
        .filter(|c| only.is_none_or(|id| c.id == id))
        .filter_map(|c| {
            let missing = c.missing_layers(&d.doc);
            (!missing.is_empty()).then(|| json!({"comp": c.id, "name": c.name, "missingLayers": missing.iter().map(|l| l.0).collect::<Vec<_>>()}))
        })
        .collect();
    let n = warnings.len();
    if flag(p, "clear", false) && n > 0 {
        let label = if only.is_some() { "Clear Layer Comp Warning" } else { "Clear All Layer Comp Warnings" };
        s.edit(label, |doc, _| {
            let live: std::collections::HashSet<LayerId> = doc.walk().into_iter().map(|(_, _, l)| l.id).collect();
            for c in doc.layer_comps.iter_mut().filter(|c| only.is_none_or(|id| c.id == id)) {
                c.states.retain(|st| live.contains(&st.layer));
            }
            Ok(())
        })?;
    }
    Ok(json!({"warnings": warnings, "count": n}))
}

fn list(s: &mut Session) -> Result<Value> {
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let comps: Vec<Value> = d
        .doc
        .layer_comps
        .iter()
        .map(|c| {
            json!({
                "id": c.id, "name": c.name, "comment": c.comment,
                "visibility": c.apply_visibility, "position": c.apply_position, "appearance": c.apply_appearance,
                "alternate": c.alternate, "scope": c.scope.iter().map(|l| l.0).collect::<Vec<_>>(), "documentLabel": c.document_label,
                "layers": c.states.len(), "missingLayers": c.missing_layers(&d.doc).len(),
            })
        })
        .collect();
    Ok(json!({
        "comps": comps, "lastApplied": d.doc.last_applied_comp, "hasLastDocumentState": d.doc.last_document_state.is_some(),
        "showing": d.doc.comp_edit.as_ref().map(|e| e.comp),
    }))
}

/// File › Export › Layer Comps to Files: one file per comp, named
/// `<prefix>_<index>_<comp name>.<format>`.
fn comps_to_files(s: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "file.export.layerCompsToFiles";
    let dir = str_param(p, "dir", cmd).or_else(|_| str_param(p, "output", cmd))?.to_string();
    let format = p.get("format").and_then(Value::as_str).unwrap_or("png").trim_start_matches('.').to_ascii_lowercase();
    let save = SaveOpts::from_params(s, p, cmd)?;
    let d = s.active().ok_or(EngineError::NoDocument)?;
    let prefix = p.get("prefix").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| stem(&d.doc.name));
    let doc = d.doc.clone();
    let chosen: Vec<u32> = match p.get("comps") {
        Some(Value::Array(a)) => a.iter().map(|v| crate::commands::u32_id(cmd, "comps", v)).collect::<Result<Vec<_>>>()?,
        _ if flag(p, "selectedOnly", false) => vec![comp_param(&doc, &json!({}), cmd)?],
        _ => doc.layer_comps.iter().map(|c| c.id).collect(),
    };
    let mut files = Vec::new();
    for (i, c) in doc.layer_comps.iter().enumerate().filter(|(_, c)| chosen.contains(&c.id)) {
        let mut one = (*doc).clone();
        one.leave_comp_edit();
        apply_comp(&mut one, c, false);
        one.last_applied_comp = Some(c.id);
        let path = join(&dir, &format!("{}_{:04}_{}.{format}", sanitize(&prefix), i, sanitize(&c.name)));
        save_doc(&one, &path, save)?;
        files.push(path);
    }
    if files.is_empty() {
        return Err(bad(cmd, "no matching layer comps"));
    }
    Ok(json!({"files": files}))
}

pub fn specs() -> Vec<CommandSpec> {
    macro_rules! spec {
        ($id:expr, $label:expr, $menu:expr, $params:expr, $enabled:expr, $run:expr) => {
            CommandSpec { id: $id, label: $label, menu: $menu, shortcut: None, params: $params, enabled: $enabled, journal: true, run: $run }
        };
    }
    vec![
        spec!(
            "layerComp.new",
            "New Layer Comp…",
            &[],
            r##"{"name":str="Layer Comp N","comment":str="","visibility":bool=true,"position":bool=true,"appearance":bool=true,"alternate":bool=false (an alternate layout: starts empty, is shown and edited by Apply, records exact placement; never appearance),"scope":[top-level layer id]? (an alternate layout's reach: those artboards only),"documentLabel":str? (what a switch calls the document's own arrangement)} → {comp}"##,
            has_doc,
            new_comp
        ),
        spec!(
            "layerComp.update",
            "Update Layer Comp",
            &[],
            r##"{"comp":id|name|"*"? (default: last applied; "*" = all),"what":"all|visibility|position|appearance"="all"}"##,
            has_comps,
            update_comp
        ),
        spec!(
            "layerComp.apply",
            "Apply Layer Comp",
            &[],
            r##"{"comp":id|name? (default: last applied)} → {comp, missingLayers, alternate?} (an alternate layout is shown for editing instead; Restore Last Document State leaves it)"##,
            has_comps,
            apply_cmd
        ),
        spec!("layerComp.previous", "Apply Previous Layer Comp", &[], "{}", has_comps, |s, _| step(s, -1)),
        spec!("layerComp.next", "Apply Next Layer Comp", &[], "{}", has_comps, |s, _| step(s, 1)),
        spec!("layerComp.delete", "Delete Layer Comp", &[], r##"{"comp":id|name?}"##, has_comps, delete),
        spec!("layerComp.reorder", "Reorder Layer Comp", &[], r##"{"comp":id|name?,"index":integer (zero-based final position)}"##, has_comps, reorder),
        spec!("layerComp.duplicate", "Duplicate Layer Comp", &[], r##"{"comp":id|name?} → {comp}"##, has_comps, duplicate),
        spec!("layerComp.rename", "Rename Layer Comp", &[], r##"{"comp":id|name?,"name":str}"##, has_comps, rename),
        spec!("layerComp.setComment", "Layer Comp Comment", &[], r##"{"comp":id|name?,"comment":str}"##, has_comps, set_comment),
        spec!(
            "layerComp.setOptions",
            "Layer Comp Options…",
            &[],
            r##"{"comp":id|name?,"visibility":bool?,"position":bool?,"appearance":bool?,"name":str?,"comment":str?}"##,
            has_comps,
            set_options
        ),
        spec!("layerComp.restoreLastDocumentState", "Restore Last Document State", &[], "{}", has_last_state, |s, _| restore_last(s)),
        spec!(
            "layerComp.updateWarnings",
            "Layer Comp Warnings",
            &[],
            r##"{"comp":id|name? (default: every comp; limits the report and the clear to that comp),"clear":bool=false (drop states of deleted layers)} → {warnings:[{comp,name,missingLayers}],count}"##,
            has_doc,
            update_warnings
        ),
        CommandSpec {
            journal: false,
            ..spec!(
                "layerComp.list",
                "List Layer Comps",
                &[],
                "{} → {comps:[{id,name,comment,visibility,position,appearance,alternate,scope,documentLabel,layers,missingLayers}],lastApplied,hasLastDocumentState,showing (the alternate layout shown, or null)}",
                has_doc,
                |s, _| list(s)
            )
        },
        spec!(
            "file.export.layerCompsToFiles",
            "Layer Comps to Files…",
            &["File", "Export"],
            r##"{"dir":folder,"format":"png|jpg|psd|tiff|…"="png","prefix":str=document name,"selectedOnly":bool=false (only the last applied comp),"comps":[id]?,"quality":0..12?} → {files}"##,
            export_comps,
            comps_to_files
        ),
    ]
}

#[cfg(test)]
#[path = "comps_cmds/tests.rs"]
mod tests;
