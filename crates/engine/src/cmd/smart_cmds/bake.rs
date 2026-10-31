//! Baking a smart object's filters where the document is untrusted (a server rendering uploads):
//! the pixels [`render_below_filter`] gives, made with explicit budgets, cancellable, from the
//! document alone, and without touching the process-wide caches an editor keeps.
//!
//! [`bake_below_filter`] returns two surfaces in document pixels: the contents as placed (the
//! first filter's input) and the same after the visible filters below the bake point, without the
//! filter mask. A host that draws the remaining filters itself keeps both, so it can show either
//! and mix them through the mask.
//!
//! What differs from [`render_below_filter`], and why:
//!
//! * **Memory only.** The contents come from the smart object's embedded bytes or the document's
//!   own placed-layer data; a linked file is never read. Every filter runs under
//!   [`deny_external_inputs`], so one that would read a file its parameters name or run an
//!   installed plug-in is refused instead.
//! * **The host decodes.** [`BakeOptions::decode`] opens the contents within the host's budgets
//!   (which formats, how large); compositing layered contents takes [`BakeOptions::render`].
//! * **Budgets before work.** The placed contents' bounds are known before anything is placed (the
//!   transform of the source box, or of a warp's control hull, which holds the warped surface), so
//!   a placement larger than [`BakeOptions::max_pixels`] is refused, not attempted.
//! * **Cancellable.** The interrupt is checked between stages, per tile inside placement and inside
//!   the tiled pixel filters. The special kernels run their controlled twins
//!   ([`crate::filters::apply_filter_to_surface_controlled`]): Liquify per dab and tile batch,
//!   Puppet and Perspective Warp per mesh step, solver iteration and destination tile, Lens
//!   Correction per tile, Adaptive Wide Angle per solver iteration and tile, Camera Raw and
//!   Shadows/Highlights per stage and row, the other adjustments per tile. Their output, working
//!   memory and solver work are held to [`BakeOptions::max_pixels`],
//!   [`BakeOptions::max_filter_bytes`] and [`BakeOptions::max_work`]; past them a filter is a
//!   [`BakeError::Limit`], never the unfiltered pixels.
//! * **No caches.** Nothing is read from or written to the decoded-source or stage caches, and
//!   nothing the contents cache is drawn as it is: their text, shapes and smart objects are drawn
//!   again from what they are (fonts, paths, nested contents, filters), and fill layers from
//!   their fill, before the contents are composited. A stored rendering may be stale (contents
//!   edited since), missing (a document built without one) or anything at all (a file nobody
//!   vetted). Text in a font the bake's text engine lacks ([`BakeOptions::text`]), and a nested
//!   smart object whose own contents or filters the bake cannot render, are refused instead.
//!   Nesting is held to [`BakeOptions::max_depth`] levels, [`RenderOptions::max_layers`] smart
//!   objects in all and [`BakeOptions::max_stack_bytes`] of contents open at once.
//! * **The filter switch is ignored.** The bake point says which filters run; a host keeps the
//!   stack's on/off state itself.
//!
//! [`render_below_filter`]: super::render_below_filter

use std::cell::{Cell, OnceCell};
use std::sync::{Mutex, PoisonError};

use photocraft_algo::transform::Interp;
use photocraft_compose::{RenderError, RenderOptions};
use photocraft_doc::{LayerContent, ShapeLayer, TextLayer};
use photocraft_raster::Interrupt;
use photocraft_text::TextEngine;

use super::rendering::{blend_surfaces, filter_bounds, renders_in_object, stack_stat, unapplied_filters};
use super::source::{buffer_to_surface, embedded_source_bytes, source_surface, vector_scale};
use super::{Affine, BlendMode, Document, EngineError, Homography, Layer, PixelFormat, Rect, SmartFilter, SmartObject, SmartSource, Surface, Value};
use crate::filters::FilterRefused;

/// Opens a smart object's contents (file name, bytes) as a document, see [`BakeOptions::decode`].
pub type BakeDecoder<'a> = dyn Fn(&str, &[u8], &Interrupt<'_>) -> Result<Document, BakeError> + 'a;

/// The host's side of a bake.
pub struct BakeOptions<'a> {
    /// Opens a smart object's contents (file name, bytes) as a document within the host's budgets;
    /// the host decides which formats it reads. SVG contents never reach it: they are rasterised
    /// here at the placement's scale, as the editor does (within photocraft's raster cap).
    pub decode: &'a BakeDecoder<'a>,
    /// Budgets for compositing layered contents: once, or once per layer for a stack mode.
    pub render: RenderOptions,
    /// Most pixels of the placed contents' bounds, and of the filters' extent (the canvas and the
    /// placed contents together).
    pub max_pixels: u64,
    /// Stack modes: most bytes of the per-layer frames held at once (16 per pixel).
    pub max_stack_bytes: u64,
    /// Estimated pixel buffers and concurrent filter source windows.
    pub max_filter_bytes: u64,
    /// Most solver and geometry steps one filter may take (puppet and wide-angle solvers, mesh
    /// warps' triangles, plane lookups), in inner-loop steps; see
    /// [`photocraft_algo::control::Budget::max_work`]. Drawing a text layer of the contents is held
    /// to it too (its glyphs' areas).
    pub max_work: u64,
    /// Most levels of contents: the baked smart object's are level 1, a smart object inside them
    /// opens level 2, and so on.
    pub max_depth: usize,
    /// The engine text layers in the contents are drawn with; only its fonts are used. `None`: a
    /// fresh [`TextEngine::new`] per bake (photocraft's bundled fonts). A host that serves fonts
    /// registers them with its engine ([`photocraft_text::FontDb::register_font_data`]).
    pub text: Option<&'a Mutex<TextEngine>>,
}

/// Longest text (in characters) a text layer of the contents may have: its layout is one step no
/// interrupt splits.
pub const MAX_TEXT_CHARS: usize = 1 << 14;

/// What [`bake_below_filter`] made, in document pixels at the document's pixel format.
#[derive(Clone, Debug)]
pub struct Baked {
    /// The contents placed through the warp, transform or perspective: the first filter's input.
    pub original: Surface,
    /// `original` after the visible filters below the bake point, each blended by its own mode
    /// and opacity, without the filter mask.
    pub filtered: Surface,
}

/// Why a bake made nothing.
#[derive(Debug, thiserror::Error)]
pub enum BakeError {
    /// The interrupt was set.
    #[error("cancelled")]
    Cancelled,
    /// A budget refused the work before it was done.
    #[error("{0}")]
    Limit(String),
    /// The bake needs what it does not read: a linked file that is not in the document, a file a
    /// filter's parameters name, an installed plug-in, a filter photocraft does not implement, or
    /// contents the host's decoder does not open. The smart object's stored rendering is then the
    /// best picture of it there is.
    #[error("{0}")]
    Refused(String),
    /// A filter or the decoder failed.
    #[error(transparent)]
    Engine(EngineError),
}

impl From<EngineError> for BakeError {
    fn from(e: EngineError) -> Self {
        match e {
            EngineError::Cancelled => Self::Cancelled,
            // What the authority denied.
            EngineError::Disabled(..) => Self::Refused(e.to_string()),
            e => Self::Engine(e),
        }
    }
}

impl From<photocraft_raster::AllocationError> for BakeError {
    fn from(e: photocraft_raster::AllocationError) -> Self {
        Self::Limit(format!("not enough memory to place the contents ({e})"))
    }
}

fn render_error(what: &str) -> impl Fn(RenderError) -> BakeError + '_ {
    move |e| match e {
        RenderError::Cancelled => BakeError::Cancelled,
        RenderError::LimitExceeded(limit) => BakeError::Limit(format!("{what}: {limit} over budget")),
    }
}

/// The authority a bake runs under: anything that would read outside the document is refused.
/// Stored smart filters re-read the files their parameters name (Displace's `mapPath`, Color
/// Lookup's `file`) and run installed plug-ins (`plugin.run`); a linked source is a `path`.
pub fn deny_external_inputs(id: &str, params: &Value) -> crate::Result<()> {
    if id == crate::plugin_cmds::RUN {
        return Err(EngineError::Disabled(id.into(), "a plug-in filter runs installed code; a bake runs none".into()));
    }
    for key in ["file", "mapPath", "path"] {
        if params.get(key).and_then(Value::as_str).is_some_and(|s| !s.is_empty()) {
            return Err(EngineError::Disabled(id.into(), format!("it reads the file its `{key}` names; a bake reads nothing outside the document")));
        }
    }
    Ok(())
}

fn check(ctl: &Interrupt<'_>) -> Result<(), BakeError> {
    if ctl.cancelled() { Err(BakeError::Cancelled) } else { Ok(()) }
}

fn area(r: Rect) -> u64 {
    if r.is_empty() { 0 } else { u64::from(r.width()) * u64::from(r.height()) }
}

/// The smart object `sm` of `doc` placed, and after its visible filters below `upto` (an index
/// into `smart_filters`, hidden ones counted), as the module docs describe.
pub fn bake_below_filter(doc: &Document, sm: &SmartObject, upto: usize, opts: &BakeOptions<'_>, ctl: &Interrupt<'_>) -> Result<Baked, BakeError> {
    if upto > sm.smart_filters.len() {
        return Err(BakeError::Engine(EngineError::BadParams { cmd: "bake".into(), msg: format!("filter {upto} of a stack of {}", sm.smart_filters.len()) }));
    }
    let _authority = crate::jobs::render_authorization_scope(Some(deny_external_inputs));
    let filters: Vec<&SmartFilter> = sm.smart_filters[..upto].iter().filter(|f| f.visible).collect();
    let mut below = sm.clone();
    below.smart_filters.truncate(upto);
    below.filters_enabled = true;
    let missing = unapplied_filters(&below);
    if !missing.is_empty() {
        return Err(BakeError::Refused(format!("photocraft does not implement the smart filter {}", missing.join(", "))));
    }
    for f in &filters {
        deny_external_inputs(&f.command, &f.params)?;
    }
    check(ctl)?;
    let Some((name, bytes)) = embedded_source_bytes(&doc.metadata, &sm.source) else {
        return Err(BakeError::Refused(match &sm.source {
            SmartSource::Linked { path } => format!("the linked file {path} is not in the document"),
            SmartSource::Embedded { .. } => "the smart object has no contents".into(),
        }));
    };
    let (fmt, canvas) = (doc.pixel_format(), doc.bounds());
    let nest = Nesting::default();
    let original = place(&name, &bytes, fmt, sm, opts, &nest, ctl)?;
    check(ctl)?;
    // Edge pixels repeat at the canvas ∪ placed-content edge, as for layer filters.
    let extent = canvas.union(&original.content_bounds());
    if area(extent) > opts.max_pixels {
        return Err(BakeError::Limit(format!("the filters would work on {} pixels; the limit is {}", area(extent), opts.max_pixels)));
    }
    let mut cur = original.clone();
    let mut object_bounds: Option<Rect> = None;
    for f in filters {
        check(ctl)?;
        let bounds = if renders_in_object(f) { *object_bounds.get_or_insert_with(|| filter_bounds(f, &original, canvas)) } else { canvas };
        cur = apply(f, &cur, bounds, extent, opts, ctl)?;
    }
    check(ctl)?;
    Ok(Baked { original, filtered: cur })
}

/// One visible filter over `cur`, blended by its mode and opacity (as the editor's re-render).
fn apply(f: &SmartFilter, cur: &Surface, bounds: Rect, extent: Rect, opts: &BakeOptions<'_>, ctl: &Interrupt<'_>) -> Result<Surface, BakeError> {
    let max_bytes = opts.max_filter_bytes;
    // The algorithm's own source-window limit is per worker, not per bake. Account for
    // the caller's pool before it allocates windows (a large halo can exceed the canvas).
    let pixels = area(extent);
    let mut working = pixels.saturating_mul(64);
    if let Some(params) = crate::filters::params_for(&f.command, &f.params) {
        let window = match params.halo_for(bounds) {
            photocraft_algo::Halo::Bounds => pixels,
            photocraft_algo::Halo::Radius(r) => {
                let halo = u64::try_from(r).unwrap_or(u64::MAX).saturating_mul(2);
                (u64::from(extent.width()).min(2048).saturating_add(halo)).saturating_mul(u64::from(extent.height()).min(2048).saturating_add(halo))
            }
        };
        #[cfg(not(target_arch = "wasm32"))]
        let workers = rayon::current_num_threads() as u64;
        #[cfg(target_arch = "wasm32")]
        let workers = 1;
        working = working.saturating_add(window.saturating_mul(cur.format().channels() as u64).saturating_mul(4).saturating_mul(workers));
    }
    if working > max_bytes {
        return Err(BakeError::Limit(format!("the smart filter {} needs an estimated {working} bytes; the limit is {max_bytes}", f.command)));
    }
    // The special kernels hold their own buffers on top of what the estimate above reserves for
    // the bake's surfaces; they get what it leaves.
    let budget = photocraft_algo::control::Budget { max_pixels: opts.max_pixels, max_bytes: max_bytes - working, max_work: opts.max_work };
    let out = match crate::filters::apply_filter_to_surface_controlled(&f.command, &f.params, cur, bounds, None, extent, &budget, ctl) {
        Ok(Some(out)) => out,
        // A registered command is not necessarily a renderable smart filter.
        Ok(None) => return Err(BakeError::Refused(format!("photocraft cannot apply {} as a smart filter", f.command))),
        Err(FilterRefused::NotApplied) => {
            return Err(BakeError::Limit(format!("the smart filter {} did not run: its geometry or working memory was refused", f.command)));
        }
        Err(FilterRefused::Limit(m)) => return Err(BakeError::Limit(format!("the smart filter {}: {m}", f.command))),
        Err(FilterRefused::Engine(e)) => return Err(e.into()),
    };
    Ok(if f.blend == BlendMode::Normal && f.opacity >= 1.0 { out } else { blend_surfaces(cur, &out, f.blend, f.opacity.clamp(0.0, 1.0)) })
}

/// What the levels of one bake share (see the module docs): how deep the contents open now are,
/// how many nested smart objects were drawn, what the open levels hold, and the text engine.
#[derive(Default)]
struct Nesting {
    depth: Cell<usize>,
    drawn: Cell<usize>,
    held: Cell<u64>,
    text: OnceCell<Mutex<TextEngine>>,
}

impl Nesting {
    fn text<'a>(&'a self, opts: &BakeOptions<'a>) -> &'a Mutex<TextEngine> {
        opts.text.unwrap_or_else(|| self.text.get_or_init(|| Mutex::new(TextEngine::new())))
    }
}

fn surface_bytes(s: &Surface) -> u64 {
    (s.tile_count() as u64)
        .saturating_mul(photocraft_geom::TILE_SIZE as u64 * photocraft_geom::TILE_SIZE as u64)
        .saturating_mul(s.format().bytes_per_pixel() as u64)
}

impl Nesting {
    fn check_surface(&self, rect: Rect, fmt: PixelFormat, opts: &BakeOptions<'_>) -> Result<(), BakeError> {
        let ts = photocraft_geom::TILE_SIZE as u64;
        let tiles = if rect.is_empty() {
            0
        } else {
            let ts = photocraft_geom::TILE_SIZE;
            let cols = i64::from((rect.x1 - 1).div_euclid(ts)) - i64::from(rect.x0.div_euclid(ts)) + 1;
            let rows = i64::from((rect.y1 - 1).div_euclid(ts)) - i64::from(rect.y0.div_euclid(ts)) + 1;
            (cols as u64).saturating_mul(rows as u64)
        };
        let bytes = tiles.saturating_mul(ts * ts).saturating_mul(fmt.bytes_per_pixel() as u64);
        if self.held.get().saturating_add(bytes) > opts.max_stack_bytes {
            return Err(BakeError::Limit("the redrawn contents exceed the retained surface budget".into()));
        }
        Ok(())
    }
}

/// Bytes the layers of `doc` store (their own surfaces and caches, masks included).
fn stored_bytes(layers: &[Layer]) -> u64 {
    let surface = surface_bytes;
    layers.iter().fold(0u64, |acc, l| {
        let own = l.surface().map_or(0, surface).saturating_add(l.mask.as_ref().map_or(0, |m| surface(&m.surface)));
        let fills = l.fill_cache.as_ref().map_or(0, |c| surface(&c.surface));
        let filters = match &l.content {
            LayerContent::Smart(sm) => sm.filter_mask.as_ref().map_or(0, |m| surface(&m.surface)),
            _ => 0,
        };
        acc.saturating_add(own).saturating_add(fills).saturating_add(filters).saturating_add(l.children().map_or(0, stored_bytes))
    })
}

/// The contents in their own pixels: composited (or stacked) once, uncached, their text, shape,
/// fill and smart layers drawn again first ([`redraw`]).
fn contents(
    name: &str,
    bytes: &[u8],
    fmt: PixelFormat,
    sm: &SmartObject,
    opts: &BakeOptions<'_>,
    nest: &Nesting,
    ctl: &Interrupt<'_>,
) -> Result<(Surface, Rect), BakeError> {
    let depth = nest.depth.get() + 1;
    if depth > opts.max_depth {
        return Err(BakeError::Limit(format!("the contents nest smart objects more than {} deep", opts.max_depth)));
    }
    let mut doc = (opts.decode)(name, bytes, ctl)?;
    check(ctl)?;
    let held = stored_bytes(&doc.layers);
    if nest.held.get().saturating_add(held) > opts.max_stack_bytes {
        return Err(BakeError::Limit(format!(
            "the open contents would hold {} bytes; the limit is {}",
            nest.held.get().saturating_add(held),
            opts.max_stack_bytes
        )));
    }
    let previous_held = nest.held.get();
    nest.depth.set(depth);
    nest.held.set(previous_held + held);
    let out = redraw(&mut doc, opts, nest, ctl).and_then(|()| composite(&doc, fmt, sm, opts, nest, ctl));
    nest.depth.set(depth - 1);
    nest.held.set(previous_held);
    out
}

/// [`contents`] once its layers are drawn again.
fn composite(
    doc: &Document,
    fmt: PixelFormat,
    sm: &SmartObject,
    opts: &BakeOptions<'_>,
    nest: &Nesting,
    ctl: &Interrupt<'_>,
) -> Result<(Surface, Rect), BakeError> {
    let bounds = doc.bounds();
    let buf = match sm.stack_mode {
        None => photocraft_compose::flatten_with(doc, &opts.render, ctl).map_err(render_error("compositing the smart object's contents"))?,
        Some(mode) => {
            // As `stack_image`: the source's layers (or a single group's), each rendered alone.
            let layers: &[Layer] = match doc.layers.as_slice() {
                [only] if only.is_group() => only.children().unwrap_or_default(),
                all => all,
            };
            let visible: Vec<&Layer> = layers.iter().filter(|l| l.visible).collect();
            if visible.is_empty() {
                return Err(BakeError::Engine(EngineError::Other("the smart object's contents have no visible layers".into())));
            }
            let held = (visible.len() as u64).saturating_mul(area(bounds)).saturating_mul(16);
            if held.saturating_add(nest.held.get()) > opts.max_stack_bytes {
                return Err(BakeError::Limit(format!("the stack mode would hold {held} bytes of frames; the limit is {}", opts.max_stack_bytes)));
            }
            let mut frames = Vec::with_capacity(visible.len());
            for l in visible {
                check(ctl)?;
                let mut one = l.clone();
                one.blend = BlendMode::Normal;
                one.clipped = false;
                // `render_layer`'s setting: no document patterns, sRGB transfer, default light.
                let mut alone = Document::new(doc.name.clone(), doc.size, doc.mode, doc.depth);
                alone.layers = vec![one];
                frames.push(photocraft_compose::render_with(&alone, bounds, &opts.render, ctl).map_err(render_error("rendering a layer of the stack"))?.px);
            }
            check(ctl)?;
            photocraft_compose::Buffer { rect: bounds, px: photocraft_algo::stack::combine(&frames, stack_stat(mode)) }
        }
    };
    check(ctl)?;
    Ok((source_surface(doc, &buf, fmt)?, bounds))
}

/// Draws the layers of `doc` that the compositor would otherwise take from a stored rendering:
/// text from its text and fonts, shapes from their paths, smart objects from their own contents
/// and filters, fills from their fill (see the module docs).
fn redraw(doc: &mut Document, opts: &BakeOptions<'_>, nest: &Nesting, ctl: &Interrupt<'_>) -> Result<(), BakeError> {
    let mut layers = std::mem::take(&mut doc.layers);
    let out = redraw_layers(&mut layers, doc, "", opts, nest, ctl);
    doc.layers = layers;
    out
}

/// `doc` holds everything but `layers` (its canvas, metadata and linked files); `path` names the
/// group they are in.
fn redraw_layers(layers: &mut [Layer], doc: &Document, path: &str, opts: &BakeOptions<'_>, nest: &Nesting, ctl: &Interrupt<'_>) -> Result<(), BakeError> {
    for l in layers {
        check(ctl)?;
        if !l.visible {
            continue;
        }
        let before = l.surface().map_or(0, surface_bytes);
        let name = if path.is_empty() { format!("\"{}\"", l.name) } else { format!("{path} › \"{}\"", l.name) };
        let named = |e: BakeError| match e {
            BakeError::Limit(m) => BakeError::Limit(format!("the layer {name} of the smart object's contents: {m}")),
            BakeError::Refused(m) => BakeError::Refused(format!("the layer {name} of the smart object's contents: {m}")),
            e => e,
        };
        l.fill_cache = None;
        match &mut l.content {
            LayerContent::Group(g) => redraw_layers(&mut g.children, doc, &name, opts, nest, ctl)?,
            LayerContent::Text(t) => redraw_text(t, doc, opts, nest, ctl).map_err(named)?,
            LayerContent::Shape(sh) => redraw_shape(sh, doc, opts, nest, ctl).map_err(named)?,
            LayerContent::Smart(sm) => {
                let drawn = redraw_smart(doc, sm, opts, nest, ctl).map_err(named)?;
                sm.cache = Some(drawn);
            }
            _ => {}
        }
        if !l.is_group() {
            nest.held.set(nest.held.get().saturating_sub(before).saturating_add(l.surface().map_or(0, surface_bytes)));
            if nest.held.get() > opts.max_stack_bytes {
                return Err(named(BakeError::Limit("the redrawn contents exceed the retained surface budget".into())));
            }
        }
    }
    Ok(())
}

/// The stored renderings [`refresh_renderings`] draws again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Renderings {
    pub text: bool,
    pub shapes: bool,
    /// Gradient and pattern fills (the compositor draws a solid fill from its colour).
    pub fills: bool,
    pub smart_objects: bool,
}

impl Renderings {
    pub const ALL: Self = Self { text: true, shapes: true, fills: true, smart_objects: true };
}

/// Draws the stored renderings of `doc` again, as a bake draws its contents' ([`redraw`]), for a
/// host that builds a document from another model (it has no renderings) or receives one from an
/// editor (its renderings are whatever the file says): text from its text and the fonts of
/// [`BakeOptions::text`], shapes from their paths, smart objects from their contents and filters,
/// and gradient and pattern fills into a [`photocraft_doc::FillCache`] of the pixels the
/// compositor would draw (a solid fill's is dropped: the compositor draws its colour). `which`
/// says which kinds. Hidden layers are drawn too: an editor shows
/// a layer as soon as it is turned on.
///
/// A layer that cannot be drawn here ([`BakeError::Refused`]: a font the text engine lacks, a
/// linked file the document does not hold, a filter photocraft does not implement) keeps the
/// rendering it has, and is named in the returned list with the reason. A budget, the interrupt or
/// an engine failure ends the whole refresh with that error, the layer named.
pub fn refresh_renderings(doc: &mut Document, which: Renderings, opts: &BakeOptions<'_>, ctl: &Interrupt<'_>) -> Result<Vec<String>, BakeError> {
    check(ctl)?;
    // Check every layer, including hidden groups, before recursive inventory or redraw.
    let mut stack = vec![(doc.layers.iter(), 0usize)];
    let mut count = 0usize;
    while let Some((layers, depth)) = stack.last_mut() {
        check(ctl)?;
        let Some(layer) = layers.next() else {
            stack.pop();
            continue;
        };
        count = count.saturating_add(1);
        if count > opts.render.max_layers || *depth > photocraft_doc::MAX_GROUP_DEPTH {
            return Err(BakeError::Limit("the refresh exceeds the layer count or group depth budget".into()));
        }
        if let Some(children) = layer.children() {
            let depth = *depth + 1;
            stack.push((children.iter(), depth));
        }
    }
    if area(doc.bounds()) > opts.max_pixels.min(opts.render.max_pixels) {
        return Err(BakeError::Limit("the refresh canvas exceeds the pixel budget".into()));
    }
    let _authority = crate::jobs::render_authorization_scope(Some(deny_external_inputs));
    let nest = Nesting::default();
    let held = stored_bytes(&doc.layers).saturating_add(doc.patterns.iter().map(|p| surface_bytes(&p.surface)).fold(0u64, u64::saturating_add));
    if held > opts.max_stack_bytes {
        return Err(BakeError::Limit(format!("the document holds {held} bytes of pixels; the limit is {}", opts.max_stack_bytes)));
    }
    nest.held.set(held);
    let mut layers = std::mem::take(&mut doc.layers);
    let mut kept = Vec::new();
    let out = refresh_layers(&mut layers, doc, "", which, opts, &nest, ctl, &mut kept);
    doc.layers = layers;
    out.map(|()| kept)
}

#[allow(clippy::too_many_arguments)]
fn refresh_layers(
    layers: &mut [Layer],
    doc: &Document,
    path: &str,
    which: Renderings,
    opts: &BakeOptions<'_>,
    nest: &Nesting,
    ctl: &Interrupt<'_>,
    kept: &mut Vec<String>,
) -> Result<(), BakeError> {
    for l in layers {
        check(ctl)?;
        let name = if path.is_empty() { format!("\"{}\"", l.name) } else { format!("{path} › \"{}\"", l.name) };
        let held = |l: &Layer| l.surface().map_or(0, surface_bytes).saturating_add(l.fill_cache.as_ref().map_or(0, |c| surface_bytes(&c.surface)));
        let before = held(l);
        let drawn = match &mut l.content {
            LayerContent::Group(g) => {
                refresh_layers(&mut g.children, doc, &name, which, opts, nest, ctl, kept)?;
                continue;
            }
            LayerContent::Text(t) if which.text => redraw_text(t, doc, opts, nest, ctl),
            LayerContent::Shape(sh) if which.shapes => redraw_shape(sh, doc, opts, nest, ctl),
            LayerContent::Smart(sm) if which.smart_objects => redraw_smart(doc, sm, opts, nest, ctl).map(|drawn| sm.cache = Some(drawn)),
            // A solid fill is drawn from its colour; another application's pixels for it may be
            // anything.
            LayerContent::Fill(photocraft_doc::Fill::Solid(_)) if which.fills => {
                l.fill_cache = None;
                Ok(())
            }
            LayerContent::Fill(f) if which.fills => {
                let f = f.clone();
                draw_fill(l, &f, doc, opts, nest, ctl).map(|surface| l.fill_cache = Some(photocraft_doc::FillCache { fill: f, surface }))
            }
            _ => continue,
        };
        match drawn {
            Ok(()) => {}
            Err(BakeError::Refused(m)) => {
                kept.push(format!("the layer {name}: {m}"));
                continue;
            }
            Err(BakeError::Limit(m)) => return Err(BakeError::Limit(format!("the layer {name}: {m}"))),
            Err(e) => return Err(e),
        }
        nest.held.set(nest.held.get().saturating_sub(before).saturating_add(held(l)));
        if nest.held.get() > opts.max_stack_bytes {
            return Err(BakeError::Limit(format!("the layer {name}: the redrawn document exceeds the retained surface budget")));
        }
    }
    Ok(())
}

/// A gradient or pattern fill's pixels over the canvas, before its masks (what a
/// [`photocraft_doc::FillCache`] holds), drawn tile by tile.
fn draw_fill(l: &Layer, f: &photocraft_doc::Fill, doc: &Document, opts: &BakeOptions<'_>, nest: &Nesting, ctl: &Interrupt<'_>) -> Result<Surface, BakeError> {
    let (fmt, canvas) = (doc.pixel_format(), doc.bounds());
    nest.check_surface(canvas, fmt, opts)?;
    let tile_floats = (photocraft_geom::TILE_SIZE as u64).pow(2).saturating_mul(16);
    if area(canvas) > opts.max_pixels || tile_floats > opts.max_filter_bytes {
        return Err(BakeError::Limit("the fill exceeds the pixel or working memory budget".into()));
    }
    let pixels = area(canvas);
    let (extra_bytes, work) = match f {
        photocraft_doc::Fill::Pattern { id, name, .. } => {
            let p = photocraft_doc::pattern::find(&doc.patterns, id, name)
                .filter(|p| p.is_paintable())
                .ok_or_else(|| BakeError::Refused("the fill's pattern is missing or cannot be painted".into()))?;
            let bytes = p.pixel_count().saturating_mul(16);
            // Above the compositor's uncached conversion cap it would silently paint nothing.
            if bytes > 256 << 20 {
                return Err(BakeError::Limit("the fill's pattern exceeds the conversion limit".into()));
            }
            // Conservatively include expansion for each tile (large patterns are not cached).
            let ts = photocraft_geom::TILE_SIZE as u64;
            let tiles = u64::from(canvas.width()).div_ceil(ts).saturating_mul(u64::from(canvas.height()).div_ceil(ts));
            (bytes, p.pixel_count().saturating_mul(tiles).saturating_add(pixels))
        }
        photocraft_doc::Fill::Gradient { stops, opacity_stops, midpoints, .. } => {
            let entries = (stops.len() as u64).saturating_add(opacity_stops.len() as u64).saturating_add(midpoints.len() as u64);
            // Sampling scans stops for each pixel; preparation also clones and sorts the ramp.
            (entries.saturating_mul(64), pixels.max(1).saturating_mul(entries.max(1)).saturating_mul(2))
        }
        _ => (0, pixels),
    };
    if tile_floats.saturating_add(extra_bytes) > opts.max_filter_bytes || work > opts.max_work {
        return Err(BakeError::Limit("the fill exceeds the pattern/gradient working memory or work budget".into()));
    }
    let render = photocraft_compose::fill_content_renderer(l, f, canvas, &doc.patterns, doc.depth);
    let mut drawn = Surface::new(fmt);
    for tile in canvas.tiles() {
        check(ctl)?;
        let part = buffer_to_surface(&render(tile.rect().intersect(&canvas)), fmt);
        drawn.put_tiles(part.tiles().map(|(tc, px)| (*tc, px.clone())));
    }
    drawn.prune();
    Ok(drawn)
}

/// A text layer drawn by the bake's text engine, with the fonts it has: a family it lacks, or a
/// character none of its fonts has, is refused (a substitute font is not the layer's text).
fn redraw_text(t: &mut TextLayer, doc: &Document, opts: &BakeOptions<'_>, nest: &Nesting, ctl: &Interrupt<'_>) -> Result<(), BakeError> {
    let chars = t.text.chars().count();
    if chars > MAX_TEXT_CHARS {
        return Err(BakeError::Limit(format!("its text has {chars} characters; the most a bake draws is {MAX_TEXT_CHARS}")));
    }
    // Layout clones the run metadata; rasterization scans glyphs once per colour.
    if t.runs.len().max(t.paragraphs.len()) > MAX_TEXT_CHARS || chars.saturating_mul(t.runs.len().max(1)) > 1 << 20 {
        return Err(BakeError::Limit("its text style runs exceed the layout work budget".into()));
    }
    let path_error = |e| BakeError::Limit(format!("its text path: {e}"));
    match &t.shape {
        photocraft_doc::text::TextShape::Area { path } => photocraft_vector::flatten::validate_shape(path, None).map_err(path_error)?,
        photocraft_doc::text::TextShape::Path { path, .. } => {
            if path.knots.len() > 4096 {
                return Err(path_error("more than 4096 knots"));
            }
            let path = photocraft_doc::Path { subpaths: vec![path.clone()], ..Default::default() };
            photocraft_vector::flatten::validate_shape(&path, None).map_err(path_error)?;
        }
        _ => {}
    }
    let mut engine = nest.text(opts).lock().unwrap_or_else(PoisonError::into_inner);
    let layout = engine.layout(t, doc.resolution_dpi);
    for style in &layout.styles {
        let family = if style.font_family.is_empty() { photocraft_text::fonts::DEFAULT_FAMILY } else { style.font_family.as_str() };
        if !engine.fonts.has_family(family) {
            return Err(BakeError::Refused(format!("its font \"{family}\" is not one this server has")));
        }
    }
    if layout.glyphs.iter().any(|g| g.id == 0) {
        return Err(BakeError::Refused("some of its characters are in none of this server's fonts".into()));
    }
    // Its drawn area (text space through the layer's transform), and the glyphs' areas drawn into it.
    let scale = t.transform.determinant().abs().sqrt();
    if let Some([x0, y0, x1, y1]) = layout.bounds() {
        let [a, b, c, d, e, f] = t.transform.m;
        let at = |x: f32, y: f32| (a * f64::from(x) + c * f64::from(y) + e, b * f64::from(x) + d * f64::from(y) + f);
        let corners = [at(x0, y0), at(x1, y0), at(x1, y1), at(x0, y1)];
        let (lx, hx, ly, hy) = corners.iter().fold((f64::INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY), |(lx, hx, ly, hy), p| {
            (lx.min(p.0), hx.max(p.0), ly.min(p.1), hy.max(p.1))
        });
        let (w, h) = (hx - lx, hy - ly);
        // A warp bends the lines within about their own size again.
        let warped = if t.warp.is_some() || !matches!(t.shape, photocraft_doc::text::TextShape::Point | photocraft_doc::text::TextShape::Box { .. }) {
            2.0
        } else {
            1.0
        };
        let drawn = w * warped * h * warped;
        if drawn.is_nan() || drawn > opts.max_pixels as f64 {
            return Err(BakeError::Limit(format!(
                "it would be drawn over {:.0} × {:.0} pixels; the limit is {} pixels",
                w * warped,
                h * warped,
                opts.max_pixels
            )));
        }
    }
    let glyph_area = layout.glyphs.iter().fold(0f64, |acc, g| {
        let px = layout.faces.get(g.face as usize).map_or(0.0, |f| f64::from(f.size_px)) * scale;
        acc + px * px
    });
    if glyph_area.is_nan() || glyph_area > opts.max_work as f64 {
        return Err(BakeError::Limit(format!("its glyphs cover about {glyph_area:.0} pixels; the limit is {}", opts.max_work)));
    }
    check(ctl)?;
    let warp = photocraft_text::render::layout_deform(&layout, t);
    let ink = photocraft_text::render::ink_rect_warped(&layout, &t.transform, warp.as_ref());
    if area(ink) > opts.max_pixels || area(ink).saturating_mul(64) > opts.max_filter_bytes {
        return Err(BakeError::Limit("the text raster exceeds the pixel or working memory budget".into()));
    }
    nest.check_surface(ink, doc.pixel_format(), opts)?;
    check(ctl)?;
    let drawn = photocraft_text::render::rasterize_warped(&layout, &t.transform, doc.pixel_format(), t.antialias, warp.as_ref());
    t.cache = Some(drawn.surface);
    Ok(())
}

/// A shape layer drawn from its path, fill and stroke over the canvas, as the editor does after
/// an edit, once its path passes the editor's own checks for one.
fn redraw_shape(sh: &mut ShapeLayer, doc: &Document, opts: &BakeOptions<'_>, nest: &Nesting, ctl: &Interrupt<'_>) -> Result<(), BakeError> {
    photocraft_vector::flatten::validate_shape(&sh.path, sh.stroke.as_ref()).map_err(|e| BakeError::Limit(format!("its path: {e}")))?;
    nest.check_surface(doc.bounds(), doc.pixel_format(), opts)?;
    if area(doc.bounds()).saturating_mul(64) > opts.max_filter_bytes {
        return Err(BakeError::Limit("the shape raster exceeds the working memory budget".into()));
    }
    let compiled = photocraft_vector::CompiledShape::new(sh, photocraft_vector::DEFAULT_TOLERANCE, doc.bounds());
    let mut drawn = Surface::new(doc.pixel_format());
    for tile in compiled.bounds().unwrap_or(Rect::EMPTY).intersect(&doc.bounds()).tiles() {
        check(ctl)?;
        let part = compiled.render(doc.pixel_format(), tile.rect().intersect(&doc.bounds()));
        drawn.put_tiles(part.tiles().map(|(tc, px)| (*tc, px.clone())));
    }
    sh.cache = Some(drawn);
    Ok(())
}

/// A smart object inside the contents, drawn as the editor draws it ([`super::render`]) from its
/// own contents (one level deeper), its visible filters (when the stack is on) and its filter mask.
fn redraw_smart(doc: &Document, sm: &SmartObject, opts: &BakeOptions<'_>, nest: &Nesting, ctl: &Interrupt<'_>) -> Result<Surface, BakeError> {
    let drawn = nest.drawn.get() + 1;
    if drawn > opts.render.max_layers {
        return Err(BakeError::Limit(format!("the contents hold more than {} smart objects", opts.render.max_layers)));
    }
    nest.drawn.set(drawn);
    let missing = unapplied_filters(sm);
    if !missing.is_empty() {
        return Err(BakeError::Refused(format!("photocraft does not implement its smart filter {}", missing.join(", "))));
    }
    let filters: Vec<&SmartFilter> = sm.smart_filters.iter().filter(|f| sm.filters_enabled && f.visible).collect();
    for f in &filters {
        deny_external_inputs(&f.command, &f.params)?;
    }
    let Some((name, bytes)) = embedded_source_bytes(&doc.metadata, &sm.source) else {
        return Err(BakeError::Refused(match &sm.source {
            SmartSource::Linked { path } => format!("its linked file {path} is not in the contents"),
            SmartSource::Embedded { .. } => "it has no contents".into(),
        }));
    };
    let (fmt, canvas) = (doc.pixel_format(), doc.bounds());
    let placed = place(&name, &bytes, fmt, sm, opts, nest, ctl)?;
    check(ctl)?;
    if filters.is_empty() {
        return Ok(placed);
    }
    let extent = canvas.union(&placed.content_bounds());
    if area(extent) > opts.max_pixels {
        return Err(BakeError::Limit(format!("its filters would work on {} pixels; the limit is {}", area(extent), opts.max_pixels)));
    }
    let mut cur = placed.clone();
    let mut object_bounds: Option<Rect> = None;
    for f in filters {
        check(ctl)?;
        let bounds = if renders_in_object(f) { *object_bounds.get_or_insert_with(|| filter_bounds(f, &placed, canvas)) } else { canvas };
        cur = apply(f, &cur, bounds, extent, opts, ctl)?;
    }
    Ok(match sm.filter_mask.as_ref().filter(|m| m.enabled) {
        Some(m) => photocraft_compose::filter_mask::mix(&placed, &cur, m),
        None => cur,
    })
}

/// The source placed in document pixels, as `rendering::place` does it, within the budget and
/// cancellable.
fn place(
    name: &str,
    bytes: &[u8],
    fmt: PixelFormat,
    sm: &SmartObject,
    opts: &BakeOptions<'_>,
    nest: &Nesting,
    ctl: &Interrupt<'_>,
) -> Result<Surface, BakeError> {
    // A vector source is rendered at the placement's scale, and the placement divided by it.
    if sm.stack_mode.is_none() && sm.warp.is_none() && photocraft_io::svg::is_svg(bytes) {
        let k = vector_scale(sm);
        let buf = photocraft_io::svg::rasterize(bytes, k as f32).map_err(|e| BakeError::Refused(format!("the SVG contents: {e}")))?;
        check(ctl)?;
        let src = buffer_to_surface(&buf, fmt);
        let map = match &sm.perspective {
            Some(p) => {
                let [h0, h1, h2, h3, h4, h5, h6, h7, h8] = *p;
                Map::Projective(Homography([h0 / k, h1 / k, h2, h3 / k, h4 / k, h5, h6 / k, h7 / k, h8]))
            }
            None => {
                let [a, b, c, d, e, f] = sm.transform.m;
                Map::Affine(Affine { m: [a / k, b / k, c / k, d / k, e, f] })
            }
        };
        return place_through(&src, buf.rect, &map, None, opts, nest, ctl);
    }
    let (src, rect) = contents(name, bytes, fmt, sm, opts, nest, ctl)?;
    let map = match &sm.perspective {
        Some(p) => Map::Projective(Homography(*p)),
        None => Map::Affine(sm.transform),
    };
    place_through(&src, rect, &map, sm.warp.as_ref().filter(|w| !w.is_identity()), opts, nest, ctl)
}

enum Map {
    Affine(Affine),
    Projective(Homography),
}

impl Map {
    /// `(x, y)` in document pixels, or `None` at or behind the horizon.
    fn apply(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        match self {
            Map::Affine(t) => {
                let [a, b, c, d, e, f] = t.m;
                Some((a * x + c * y + e, b * x + d * y + f))
            }
            Map::Projective(h) => {
                let [h0, h1, h2, h3, h4, h5, h6, h7, h8] = h.0;
                let w = h6 * x + h7 * y + h8;
                (w > 0.0).then(|| ((h0 * x + h1 * y + h2) / w, (h3 * x + h4 * y + h5) / w))
            }
        }
    }
}

/// `src` (its pixels in `rect`) through `warp` (source space) and `map`, exactly as
/// `place_source` / `place_source_projective` place it, after checking the result's bounds.
fn place_through(
    src: &Surface,
    rect: Rect,
    map: &Map,
    warp: Option<&photocraft_geom::warp::Warp>,
    opts: &BakeOptions<'_>,
    nest: &Nesting,
    ctl: &Interrupt<'_>,
) -> Result<Surface, BakeError> {
    if rect.is_empty() {
        return Ok(Surface::new(PixelFormat::new(src.format().mode, src.format().sample, true)));
    }
    // Where the result can lie: a warp's surface is inside its control hull (Bézier patches), and
    // both maps take that box's corners to a polygon holding its image (a projective map keeps
    // convex sets convex while every corner is in front of the horizon).
    let hull = match warp {
        Some(w) => {
            let b = w.bounds;
            if f64::from(rect.x0) < b[0] || f64::from(rect.y0) < b[1] || f64::from(rect.x1) > b[2] || f64::from(rect.y1) > b[3] {
                return Err(BakeError::Refused("a warp over part of the smart object's contents".into()));
            }
            w.to_mesh(1, 1).control_bounds()
        }
        None => [f64::from(rect.x0), f64::from(rect.y0), f64::from(rect.x1), f64::from(rect.y1)],
    };
    let mut bbox = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for (x, y) in [(hull[0], hull[1]), (hull[2], hull[1]), (hull[2], hull[3]), (hull[0], hull[3])] {
        let Some((px, py)) = map.apply(x, y) else {
            return Err(BakeError::Limit("the perspective places part of the contents at or behind the horizon".into()));
        };
        bbox = [bbox[0].min(px), bbox[1].min(py), bbox[2].max(px), bbox[3].max(py)];
    }
    let (w, h) = ((bbox[2] - bbox[0]).ceil() + 2.0, (bbox[3] - bbox[1]).ceil() + 2.0);
    if !(w.is_finite() && h.is_finite()) || w * h > opts.max_pixels as f64 || bbox.iter().any(|v| v.abs() > f64::from(i32::MAX / 2)) {
        return Err(BakeError::Limit(format!("the placed contents would cover {w:.0} × {h:.0} pixels; the limit is {} pixels", opts.max_pixels)));
    }
    nest.check_surface(
        Rect::new(bbox[0].floor() as i32 - 1, bbox[1].floor() as i32 - 1, bbox[2].ceil() as i32 + 1, bbox[3].ceil() as i32 + 1),
        src.format(),
        opts,
    )?;
    let budget = photocraft_algo::control::Budget { max_pixels: opts.max_pixels, max_bytes: opts.max_filter_bytes, max_work: opts.max_work };
    let refused = |e| match e {
        photocraft_algo::control::Refused::Cancelled => BakeError::Cancelled,
        photocraft_algo::control::Refused::Limit(m) => BakeError::Limit(m),
    };
    check(ctl)?;
    let placed = match (map, warp) {
        (Map::Affine(t), Some(w)) => {
            let [a, b, c, d, e, f] = t.m;
            let f = |x: f64, y: f64| {
                let (u, v) = w.map(x, y);
                (a * u + c * v + e, b * u + d * v + f)
            };
            Some(photocraft_algo::warp::warp_mesh_surface_controlled(src, rect, &f, Interp::Bicubic, &budget, ctl).map_err(refused)?)
        }
        (Map::Projective(hm), Some(w)) => {
            let f = |x: f64, y: f64| {
                let (u, v) = w.map(x, y);
                hm.apply(u, v)
            };
            Some(photocraft_algo::warp::warp_mesh_surface_controlled(src, rect, &f, Interp::Bicubic, &budget, ctl).map_err(refused)?)
        }
        (Map::Affine(t), None) => {
            let [a, b, c, d, e, f] = t.m;
            let near = |x: f64, y: f64| (x - y).abs() < 1e-9;
            if near(a, 1.0) && near(b, 0.0) && near(c, 0.0) && near(d, 1.0) && near(e, e.round()) && near(f, f.round()) {
                Some(photocraft_algo::resample::translate_surface(src, e.round() as i32, f.round() as i32))
            } else {
                photocraft_algo::transform::warp_surface_with(src, rect, &Homography([a, c, e, b, d, f, 0.0, 0.0, 1.0]), Interp::Bicubic, ctl)?
            }
        }
        (Map::Projective(hm), None) => photocraft_algo::transform::warp_surface_with(src, rect, hm, Interp::Bicubic, ctl)?,
    };
    placed.ok_or(BakeError::Cancelled)
}
