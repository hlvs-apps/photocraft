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
//! * **No caches.** Nothing is read from or written to the decoded-source or stage caches.
//! * **The filter switch is ignored.** The bake point says which filters run; a host keeps the
//!   stack's on/off state itself.
//!
//! [`render_below_filter`]: super::render_below_filter

use photocraft_algo::transform::Interp;
use photocraft_compose::{RenderError, RenderOptions};
use photocraft_raster::Interrupt;

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
    /// [`photocraft_algo::control::Budget::max_work`].
    pub max_work: u64,
}

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
    let original = place(&name, &bytes, fmt, sm, opts, ctl)?;
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
    // Stored records bypass command dispatch. Its tolerant replay drops malformed special
    // filters, which is not a successful bake.
    match f.command.as_str() {
        crate::distort_cmds::LIQUIFY => {
            crate::distort_cmds::liquify_params(&f.command, &f.params, extent)?;
        }
        crate::distort_cmds::PUPPET | crate::distort_cmds::PUPPET_SMART => {
            crate::distort_cmds::puppet_params(&f.command, &f.params)?;
        }
        crate::distort_cmds::PERSPECTIVE | crate::distort_cmds::PERSPECTIVE_SMART => {
            crate::distort_cmds::perspective_params(&f.command, &f.params)?;
        }
        crate::lens_cmds::LENS => {
            crate::lens_cmds::lens_params(&f.command, &f.params, extent, None)?;
        }
        crate::lens_cmds::WIDE => {
            crate::lens_cmds::wide_params(&f.command, &f.params, None)?;
        }
        crate::lens_cmds::RAW => {
            crate::lens_cmds::raw_params(&f.command, &f.params)?;
        }
        _ => {}
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

/// The contents in their own pixels: composited (or stacked) once, uncached.
fn contents(name: &str, bytes: &[u8], fmt: PixelFormat, sm: &SmartObject, opts: &BakeOptions<'_>, ctl: &Interrupt<'_>) -> Result<(Surface, Rect), BakeError> {
    let doc = (opts.decode)(name, bytes, ctl)?;
    check(ctl)?;
    let bounds = doc.bounds();
    let buf = match sm.stack_mode {
        None => photocraft_compose::flatten_with(&doc, &opts.render, ctl).map_err(render_error("compositing the smart object's contents"))?,
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
            if held > opts.max_stack_bytes {
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
    Ok((source_surface(&doc, &buf, fmt)?, bounds))
}

/// The source placed in document pixels, as `rendering::place` does it, within the budget and
/// cancellable.
fn place(name: &str, bytes: &[u8], fmt: PixelFormat, sm: &SmartObject, opts: &BakeOptions<'_>, ctl: &Interrupt<'_>) -> Result<Surface, BakeError> {
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
        return place_through(&src, buf.rect, &map, None, opts, ctl);
    }
    let (src, rect) = contents(name, bytes, fmt, sm, opts, ctl)?;
    let map = match &sm.perspective {
        Some(p) => Map::Projective(Homography(*p)),
        None => Map::Affine(sm.transform),
    };
    place_through(&src, rect, &map, sm.warp.as_ref().filter(|w| !w.is_identity()), opts, ctl)
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
    check(ctl)?;
    let placed = match (map, warp) {
        (Map::Affine(t), Some(w)) => {
            let [a, b, c, d, e, f] = t.m;
            let f = |x: f64, y: f64| {
                let (u, v) = w.map(x, y);
                (a * u + c * v + e, b * u + d * v + f)
            };
            photocraft_algo::warp::warp_mesh_surface_with(src, rect, &f, Interp::Bicubic, ctl)
        }
        (Map::Projective(hm), Some(w)) => {
            let f = |x: f64, y: f64| {
                let (u, v) = w.map(x, y);
                hm.apply(u, v)
            };
            photocraft_algo::warp::warp_mesh_surface_with(src, rect, &f, Interp::Bicubic, ctl)
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
