//! Layer comps (Window › Layer Comps) and artboards: pure data plus capture/apply helpers that need
//! nothing beyond the document model.
//!
//! A layer comp records, per layer, its visibility, position and appearance (blend mode, opacity,
//! fill opacity, layer style). Three flags choose which of those [`apply`](LayerComp) restores,
//! like the checkboxes of Photoshop's Layer Comp Options. Position is the top-left of the layer's
//! content bounds (artboards: of the board), so applying a comp moves a layer by the difference
//! between that and where it is now.
//!
//! An *alternate layout* ([`LayerComp::alternate`]) is a second arrangement of the same layers:
//! it records, for the layers it moves, their exact placement ([`CompPlacement`]: position, size
//! and rotation, in doubles) and their visibility. It is not applied over the document: showing it
//! ([`CompEdit`]) keeps the document's own arrangement aside, edits to visibility and placement
//! made while it is shown are recorded in it, and leaving it ([`Document::leave_comp_edit`]) puts
//! the document's arrangement back. A save writes the document with it left
//! ([`Document::committed`]).

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use photocraft_color::{BlendMode, Color};
use photocraft_geom::{Affine, Rect};
use serde::{Deserialize, Serialize};

use crate::text::TextShape;
use crate::{Document, Effects, Layer, LayerContent, LayerId};

/// One layer's state as captured by a comp. `None` = not recorded (PSD comps record only what
/// their capture flags asked for).
#[derive(Clone, Debug, PartialEq)]
pub struct CompLayerState {
    pub layer: LayerId,
    pub visible: Option<bool>,
    /// Top-left of the content bounds (or artboard rect). None for layers without a position
    /// (adjustment and fill layers, empty layers, plain groups).
    pub position: Option<(i32, i32)>,
    pub appearance: Option<CompAppearance>,
    /// Exact placement (alternate layouts only; ordinary comps record `position`). None = not
    /// recorded: the layer stays where the document has it.
    pub placement: Option<CompPlacement>,
}

/// A layer's exact placement, as an alternate layout records it.
#[derive(Clone, Debug, PartialEq)]
pub enum CompPlacement {
    /// A smart object or a type layer: its transform exactly (position, size and rotation as
    /// doubles), with a smart object's projective map when it is distorted and a paragraph type
    /// layer's box (`[x, y, width, height]` in text space, before the transform).
    Transform { transform: Affine, perspective: Option<[f64; 9]>, text_box: Option<[f32; 4]> },
    /// A layer drawn from its own pixels (a raster layer, a shape's rendering): its content
    /// bounds in document pixels. Showing it transforms the layer onto them.
    Bounds(Rect),
}

/// An alternate layout being shown: the comp and every layer as the document had it when it was
/// shown (a group without its children), which leaving it restores.
#[derive(Clone, Debug, PartialEq)]
pub struct CompEdit {
    pub comp: u32,
    pub base: Vec<Layer>,
    /// Where showing the layout put the layers it places. A layer still there when the layout is
    /// left keeps its recorded placement exactly: resampling pixels onto recorded bounds can land
    /// a pixel off, and that is not an edit.
    pub shown: Vec<(LayerId, CompPlacement)>,
}

/// Layer › Layer Style state captured by "Appearance".
#[derive(Clone, Debug, PartialEq)]
pub struct CompAppearance {
    pub blend: BlendMode,
    pub opacity: f32,
    pub fill_opacity: f32,
    pub effects: Effects,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LayerComp {
    /// Stable id (the PSD `compID`). Never 0: Photoshop uses 0 for the Last Document State.
    pub id: u32,
    pub name: String,
    pub comment: String,
    pub apply_visibility: bool,
    pub apply_position: bool,
    pub apply_appearance: bool,
    /// An alternate layout: a second arrangement of the layers (placement and visibility) that is
    /// shown and edited rather than applied over the document (see the module docs). Not part of
    /// a PSD comp: a PSD writes it as an ordinary comp of what it records there.
    pub alternate: bool,
    /// Per-layer state, in document walk order.
    pub states: Vec<CompLayerState>,
}

impl LayerComp {
    /// PSD `capturedInfo` bit flags: 1 visibility, 2 position, 4 appearance.
    pub fn captured_info(&self) -> i32 {
        i32::from(self.apply_visibility) | i32::from(self.apply_position) << 1 | i32::from(self.apply_appearance) << 2
    }
    pub fn set_captured_info(&mut self, v: i32) {
        self.apply_visibility = v & 1 != 0;
        self.apply_position = v & 2 != 0;
        self.apply_appearance = v & 4 != 0;
    }
    pub fn state(&self, id: LayerId) -> Option<&CompLayerState> {
        self.states.iter().find(|s| s.layer == id)
    }
    /// Ids of recorded layers that no longer exist in `doc` (Photoshop's warning triangle).
    pub fn missing_layers(&self, doc: &Document) -> Vec<LayerId> {
        let live: std::collections::HashSet<LayerId> = doc.walk().into_iter().map(|(_, _, l)| l.id).collect();
        self.states.iter().map(|s| s.layer).filter(|id| !live.contains(id)).collect()
    }
}

/// Content bounds used for comp positions: pixels (or the rendered cache) of a layer, the board of
/// an artboard. Empty for adjustment/fill layers and plain groups.
pub fn position_bounds(l: &Layer) -> Rect {
    match &l.content {
        LayerContent::Group(g) => g.artboard.as_ref().map_or(Rect::EMPTY, |a| a.rect),
        LayerContent::Adjustment(_) | LayerContent::Fill(_) => Rect::EMPTY,
        _ => l.surface().map_or(Rect::EMPTY, |s| s.content_bounds()),
    }
}

/// Comp position of a layer (see [`position_bounds`]).
pub fn layer_position(l: &Layer) -> Option<(i32, i32)> {
    let r = position_bounds(l);
    (!r.is_empty()).then_some((r.x0, r.y0))
}

/// Capture the full state of every layer.
pub fn capture_states(doc: &Document) -> Vec<CompLayerState> {
    doc.walk()
        .into_iter()
        .map(|(_, _, l)| CompLayerState {
            layer: l.id,
            visible: Some(l.visible),
            position: layer_position(l),
            appearance: Some(CompAppearance { blend: l.blend, opacity: l.opacity, fill_opacity: l.fill_opacity, effects: l.effects.clone() }),
            placement: None,
        })
        .collect()
}

/// A layer's exact placement (see [`CompPlacement`]): a smart object's or a type layer's
/// transform, the content bounds of a raster or shape layer, None for a layer without a place of
/// its own (a group, an adjustment or fill layer, an empty layer).
pub fn layer_placement(l: &Layer) -> Option<CompPlacement> {
    match &l.content {
        LayerContent::Smart(sm) => Some(CompPlacement::Transform { transform: sm.transform, perspective: sm.perspective, text_box: None }),
        LayerContent::Text(t) => Some(CompPlacement::Transform {
            transform: t.transform,
            perspective: None,
            text_box: match t.shape {
                TextShape::Box { x, y, width, height } => Some([x, y, width, height]),
                _ => None,
            },
        }),
        LayerContent::Raster(_) | LayerContent::Shape(_) => {
            let r = position_bounds(l);
            (!r.is_empty()).then_some(CompPlacement::Bounds(r))
        }
        _ => None,
    }
}

impl Document {
    /// The alternate layout being shown, if any.
    pub fn editing_comp(&self) -> Option<&LayerComp> {
        self.comp_edit.as_ref().and_then(|e| self.comp(e.comp))
    }

    /// Leaves the alternate layout being shown and returns its id (None when none is shown).
    ///
    /// What it shows now is recorded in its comp: a layer gets a state where it already had one
    /// or where its visibility or placement differs from the document's, so a layer nobody moved
    /// keeps following the document. Then every layer goes back to the visibility and placement
    /// the document had (anything else edited meanwhile, its text or style, stays). A layer added
    /// while it was shown belongs to it alone: hidden in the document, visible in the comp. The
    /// states of layers deleted meanwhile stay, as they do in any comp.
    pub fn leave_comp_edit(&mut self) -> Option<u32> {
        let edit = self.comp_edit.take()?;
        let base: HashMap<LayerId, &Layer> = edit.base.iter().map(|l| (l.id, l)).collect();
        let shown: HashMap<LayerId, &CompPlacement> = edit.shown.iter().map(|(id, p)| (*id, p)).collect();
        let live: Vec<(LayerId, bool, Option<CompPlacement>)> = self.walk().into_iter().map(|(_, _, l)| (l.id, l.visible, layer_placement(l))).collect();
        if let Some(comp) = self.layer_comps.iter_mut().find(|c| c.id == edit.comp) {
            let ids: HashSet<LayerId> = live.iter().map(|(id, _, _)| *id).collect();
            let mut states = Vec::with_capacity(live.len());
            for (id, visible, placement) in live {
                let old = comp.state(id);
                let st = match base.get(&id) {
                    None => CompLayerState { layer: id, visible: Some(visible), position: None, appearance: None, placement: None },
                    Some(b) => CompLayerState {
                        layer: id,
                        visible: (old.is_some_and(|o| o.visible.is_some()) || visible != b.visible).then_some(visible),
                        position: old.and_then(|o| o.position),
                        appearance: old.and_then(|o| o.appearance.clone()),
                        placement: match old.and_then(|o| o.placement.as_ref()) {
                            // Untouched since it was shown: what it records, exactly.
                            Some(was) if placement.as_ref() == shown.get(&id).copied() => Some(was.clone()),
                            Some(_) => placement,
                            None if placement != layer_placement(b) => placement,
                            None => None,
                        },
                    },
                };
                if st.visible.is_some() || st.position.is_some() || st.appearance.is_some() || st.placement.is_some() {
                    states.push(st);
                }
            }
            states.extend(comp.states.iter().filter(|s| !ids.contains(&s.layer)).cloned());
            comp.states = states;
        }
        restore_tree(&mut self.layers, &base);
        if self.last_applied_comp == Some(edit.comp) {
            self.last_applied_comp = None;
        }
        Some(edit.comp)
    }

    /// The document as a save writes it: the alternate layout being shown left
    /// ([`Self::leave_comp_edit`]), so the saved layers are the document's own arrangement and the
    /// comp holds what was shown.
    pub fn committed(&self) -> Cow<'_, Document> {
        if self.comp_edit.is_none() {
            return Cow::Borrowed(self);
        }
        let mut d = self.clone();
        d.leave_comp_edit();
        Cow::Owned(d)
    }
}

/// [`Document::leave_comp_edit`]'s restore, over a stack and its groups.
fn restore_tree(layers: &mut [Layer], base: &HashMap<LayerId, &Layer>) {
    for l in layers.iter_mut() {
        match base.get(&l.id) {
            None => l.visible = false,
            Some(b) => restore_layout(l, b),
        }
        if let Some(ch) = l.children_mut() {
            restore_tree(ch, base);
        }
    }
}

/// Puts `l` back where the document had it (`b`): its visibility, and when it was moved, resized
/// or rotated while the layout was shown, what that changed (its transform or pixels, a paragraph
/// box, its masks). A layer without a place of its own (a group, an adjustment) moves only its
/// masks, so a mask that changed is put back. A layer whose kind changed keeps its new content.
fn restore_layout(l: &mut Layer, b: &Layer) {
    l.visible = b.visible;
    let placed = layer_placement(b).is_some();
    let moved = layer_placement(l) != layer_placement(b);
    if moved {
        match (&mut l.content, &b.content) {
            (LayerContent::Smart(s), LayerContent::Smart(o)) => {
                s.transform = o.transform;
                s.perspective = o.perspective;
                s.cache = o.cache.clone();
                s.filter_mask = o.filter_mask.clone();
            }
            (LayerContent::Text(t), LayerContent::Text(o)) => {
                let mut same = t.clone();
                same.transform = o.transform;
                same.shape = o.shape.clone();
                same.cache = o.cache.clone();
                let unchanged = same == *o;
                t.transform = o.transform;
                if matches!((&t.shape, &o.shape), (TextShape::Box { .. }, TextShape::Box { .. })) {
                    t.shape = o.shape.clone();
                }
                // Its rendering is the document's again when nothing else about it changed;
                // otherwise it is drawn anew.
                t.cache = if unchanged { o.cache.clone() } else { None };
            }
            (LayerContent::Raster(s), LayerContent::Raster(o)) => *s = o.clone(),
            (LayerContent::Shape(s), LayerContent::Shape(o)) => *s = o.clone(),
            _ => {}
        }
        l.effects.reference = b.effects.reference;
        l.fill_cache = b.fill_cache.clone();
    }
    if moved || !placed {
        let bounds = |m: &Option<crate::LayerMask>| m.as_ref().map(|m| m.surface.content_bounds());
        if moved || bounds(&l.mask) != bounds(&b.mask) {
            l.mask = b.mask.clone();
        }
        if moved || l.vector_mask != b.vector_mask {
            l.vector_mask = b.vector_mask.clone();
        }
    }
}

/// A fresh comp id, unique among `doc`'s comps (never 0).
pub fn next_comp_id(doc: &Document) -> u32 {
    // Photoshop uses large pseudo-random ids; any unique non-zero value round-trips.
    let mut id = doc.layer_comps.iter().map(|c| c.id).max().unwrap_or(0).wrapping_add(1).max(1);
    while doc.layer_comps.iter().any(|c| c.id == id) {
        id = id.wrapping_add(1).max(1);
    }
    id
}

/// What an artboard paints behind its contents.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ArtboardBackground {
    #[default]
    White,
    Black,
    Transparent,
    Custom(Color),
}

impl ArtboardBackground {
    /// sRGB colour painted under the contents (None = transparent).
    pub fn rgba(&self) -> Option<[f32; 4]> {
        match self {
            ArtboardBackground::White => Some([1.0, 1.0, 1.0, 1.0]),
            ArtboardBackground::Black => Some([0.0, 0.0, 0.0, 1.0]),
            ArtboardBackground::Transparent => None,
            ArtboardBackground::Custom(c) => {
                let [r, g, b] = c.to_rgb();
                (c.alpha > 0.0).then_some([r, g, b, c.alpha])
            }
        }
    }
    /// PSD `artboardBackgroundType`: 1 white, 2 black, 3 transparent, 4 other.
    pub fn psd_type(&self) -> i32 {
        match self {
            ArtboardBackground::White => 1,
            ArtboardBackground::Black => 2,
            ArtboardBackground::Transparent => 3,
            ArtboardBackground::Custom(_) => 4,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            ArtboardBackground::White => "white",
            ArtboardBackground::Black => "black",
            ArtboardBackground::Transparent => "transparent",
            ArtboardBackground::Custom(_) => "custom",
        }
    }
}

/// An artboard: a top-level group that clips its children to `rect` and paints `background`
/// behind them (Layer › New › Artboard).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Artboard {
    /// The board in document pixels.
    pub rect: Rect,
    pub background: ArtboardBackground,
    /// Size preset the board was made from ("" = custom).
    #[serde(default)]
    pub preset: String,
}

impl Artboard {
    pub fn new(rect: Rect) -> Self {
        Self { rect, background: ArtboardBackground::White, preset: String::new() }
    }
}

impl Document {
    /// Top-level artboards, bottom to top: (layer id, name, artboard).
    pub fn artboards(&self) -> Vec<(LayerId, &str, &Artboard)> {
        self.layers
            .iter()
            .filter_map(|l| match &l.content {
                LayerContent::Group(g) => g.artboard.as_ref().map(|a| (l.id, l.name.as_str(), a)),
                _ => None,
            })
            .collect()
    }
    pub fn has_artboards(&self) -> bool {
        self.layers.iter().any(|l| l.artboard().is_some())
    }
    pub fn comp(&self, id: u32) -> Option<&LayerComp> {
        self.layer_comps.iter().find(|c| c.id == id)
    }
    /// The artboard containing layer `id` (itself, or its top-level ancestor).
    pub fn artboard_of(&self, id: LayerId) -> Option<LayerId> {
        let path = self.path_of(id)?;
        let top = self.layers.get(*path.first()?)?;
        top.artboard().map(|_| top.id)
    }
}

impl Layer {
    pub fn artboard(&self) -> Option<&Artboard> {
        match &self.content {
            LayerContent::Group(g) => g.artboard.as_ref(),
            _ => None,
        }
    }
    pub fn artboard_mut(&mut self) -> Option<&mut Artboard> {
        match &mut self.content {
            LayerContent::Group(g) => g.artboard.as_mut(),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ColorMode, PixelFormat, SampleType, Size};

    fn doc() -> Document {
        let mut d = Document::with_background("t", Size::new(64, 64), ColorMode::Rgb, SampleType::U8, Color::WHITE);
        let mut l = Layer::raster("A", PixelFormat::RGBA8);
        l.surface_mut().unwrap().fill_rect(Rect::new(10, 12, 20, 22), &[1.0, 0.0, 0.0, 1.0]);
        d.layers.push(l);
        d
    }

    #[test]
    fn capture_records_every_layer() {
        let d = doc();
        let s = capture_states(&d);
        assert_eq!(s.len(), 2);
        assert_eq!(s[1].position, Some((10, 12)));
        assert_eq!(s[1].visible, Some(true));
        assert!(s[1].appearance.is_some());
    }

    #[test]
    fn captured_info_bits() {
        let mut c = LayerComp {
            id: 1,
            name: "c".into(),
            comment: String::new(),
            apply_visibility: false,
            apply_position: false,
            apply_appearance: false,
            alternate: false,
            states: vec![],
        };
        c.set_captured_info(5);
        assert!(c.apply_visibility && !c.apply_position && c.apply_appearance);
        assert_eq!(c.captured_info(), 5);
    }

    #[test]
    fn missing_layers_and_ids() {
        let mut d = doc();
        let states = capture_states(&d);
        d.layer_comps.push(LayerComp {
            id: next_comp_id(&d),
            name: "c".into(),
            comment: String::new(),
            apply_visibility: true,
            apply_position: true,
            apply_appearance: true,
            alternate: false,
            states,
        });
        assert_eq!(d.layer_comps[0].id, 1);
        assert_eq!(next_comp_id(&d), 2);
        let gone = d.layers[1].id;
        d.layers.pop();
        assert_eq!(d.layer_comps[0].missing_layers(&d), vec![gone]);
    }

    fn shown(d: &mut Document, comp: u32) {
        let base = d.walk().into_iter().map(|(_, _, l)| l.clone()).collect();
        d.comp_edit = Some(CompEdit { comp, base, shown: Vec::new() });
        d.last_applied_comp = Some(comp);
    }

    fn alternate(id: u32, states: Vec<CompLayerState>) -> LayerComp {
        LayerComp {
            id,
            name: "QR off".into(),
            comment: String::new(),
            apply_visibility: true,
            apply_position: true,
            apply_appearance: false,
            alternate: true,
            states,
        }
    }

    #[test]
    fn leaving_a_layout_records_only_what_differs_and_keeps_its_other_states() {
        let mut d = doc();
        let a = d.layers[1].id;
        let bg = d.layers[0].id;
        let gone = LayerId(u64::MAX - 7);
        let dangling = CompLayerState { layer: gone, visible: Some(false), position: None, appearance: None, placement: None };
        // The background had a state (an absolute value equal to the document's): it keeps one.
        let kept = CompLayerState { layer: bg, visible: Some(true), position: None, appearance: None, placement: None };
        d.layer_comps.push(alternate(3, vec![kept.clone(), dangling.clone()]));
        shown(&mut d, 3);
        d.layers[1].visible = false;
        assert_eq!(d.leave_comp_edit(), Some(3));
        assert!(d.comp_edit.is_none() && d.last_applied_comp.is_none());
        assert!(d.layers[1].visible, "the document's visibility is back");
        let c = d.comp(3).unwrap();
        assert_eq!(c.states, vec![kept, CompLayerState { layer: a, visible: Some(false), position: None, appearance: None, placement: None }, dangling]);
        assert_eq!(d.leave_comp_edit(), None, "nothing shown");
    }

    #[test]
    fn leaving_puts_moved_pixels_back_and_keeps_other_edits() {
        let mut d = doc();
        d.layer_comps.push(alternate(3, vec![]));
        shown(&mut d, 3);
        let fmt = d.layers[1].surface().unwrap().format();
        let mut moved = photocraft_raster::Surface::new(fmt);
        moved.fill_rect(Rect::new(30, 30, 40, 40), &[0.0, 1.0, 0.0, 1.0]);
        d.layers[1].content = LayerContent::Raster(moved);
        d.layers[1].name = "Renamed".into();
        d.leave_comp_edit();
        assert_eq!(layer_position(&d.layers[1]), Some((10, 12)));
        assert_eq!(d.layers[1].name, "Renamed", "what is not placement stays");
        assert_eq!(d.comp(3).unwrap().states[0].placement, Some(CompPlacement::Bounds(Rect::new(30, 30, 40, 40))));
    }

    #[test]
    fn the_committed_document_is_the_one_a_save_writes() {
        let mut d = doc();
        assert!(matches!(d.committed(), Cow::Borrowed(_)));
        d.layer_comps.push(alternate(3, vec![]));
        shown(&mut d, 3);
        d.layers[1].visible = false;
        let c = d.committed();
        assert!(c.comp_edit.is_none() && c.layers[1].visible);
        assert_eq!(c.comp(3).unwrap().states[0].visible, Some(false));
        assert!(d.comp_edit.is_some() && !d.layers[1].visible, "the live document still shows the layout");
    }

    #[test]
    fn artboards_listed_and_found() {
        let mut d = doc();
        let child = Layer::raster("in", PixelFormat::RGBA8);
        let cid = child.id;
        let mut g = Layer::group("Artboard 1", vec![child]);
        if let LayerContent::Group(gr) = &mut g.content {
            gr.artboard = Some(Artboard::new(Rect::new(0, 0, 32, 32)));
        }
        let gid = g.id;
        d.layers.push(g);
        assert!(d.has_artboards());
        assert_eq!(d.artboards().len(), 1);
        assert_eq!(d.artboard_of(cid), Some(gid));
        assert_eq!(layer_position(d.layer(gid).unwrap()), Some((0, 0)));
        assert_eq!(ArtboardBackground::Black.rgba(), Some([0.0, 0.0, 0.0, 1.0]));
        assert_eq!(ArtboardBackground::Transparent.rgba(), None);
    }
}
