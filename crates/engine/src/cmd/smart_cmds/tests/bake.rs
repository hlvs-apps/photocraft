use std::sync::atomic::{AtomicUsize, Ordering};

use photocraft_geom::warp::{Warp, WarpStyle};
use photocraft_text::TextEngine;

use super::*;

/// A host decoder with no budget of its own: the engine's.
fn decode(name: &str, bytes: &[u8], _: &photocraft_raster::Interrupt<'_>) -> std::result::Result<Document, BakeError> {
    decode_source(name, bytes).map_err(BakeError::from)
}

fn options<'a>(decode: &'a BakeDecoder<'a>) -> BakeOptions<'a> {
    BakeOptions {
        decode,
        render: photocraft_compose::RenderOptions { max_pixels: 1 << 26, max_layers: 1000, max_effect_bytes: 1 << 30, max_mask_bytes: 1 << 30 },
        max_pixels: 1 << 26,
        max_stack_bytes: 1 << 30,
        max_filter_bytes: 1 << 30,
        max_work: 1 << 34,
        max_depth: 8,
        text: None,
    }
}

fn bake(doc: &Document, sm: &SmartObject, upto: usize) -> std::result::Result<Baked, BakeError> {
    bake_below_filter(doc, sm, upto, &options(&decode), &photocraft_raster::Interrupt::NONE)
}

/// The two surfaces hold the same pixels everywhere either has any.
fn assert_same(a: &Surface, b: &Surface, what: &str) {
    let r = a.content_bounds().union(&b.content_bounds());
    assert_eq!(a.content_bounds(), b.content_bounds(), "{what}: content bounds");
    assert!(a.read_region(r) == b.read_region(r), "{what}: pixels differ");
}

/// The active smart object, edited in place.
fn edit_smart(s: &mut Session, f: impl FnOnce(&mut SmartObject)) {
    s.edit("test", |doc, active| {
        if let LayerContent::Smart(sm) = &mut doc.layer_mut(active.unwrap()).unwrap().content {
            f(sm);
        }
        Ok(())
    })
    .unwrap();
}

/// A painted layer as a smart object with three filters (the middle one blended at half opacity).
fn filtered(depth: u64) -> Session {
    let mut s = session(depth);
    paint(&mut s);
    s.execute("filter.convertForSmartFilters", json!({})).unwrap();
    s.execute("filter.stylize.findEdges", json!({})).unwrap();
    s.execute("filter.noise.addNoise", json!({"amount": 20, "distribution": "gaussian", "seed": 5})).unwrap();
    s.execute("filter.blur.gaussianBlur", json!({"radius": 2})).unwrap();
    s.execute("layer.smartFilter.blendingOptions", json!({"index": 1, "blend": "multiply", "opacity": 0.5})).unwrap();
    s
}

fn assert_bakes_like_the_editor(s: &Session, what: &str) {
    let doc = &s.active().unwrap().doc;
    let sm = active_smart(s);
    let original = render_below_filter(doc, &sm, 0).unwrap().unwrap();
    for upto in 0..=sm.smart_filters.len() {
        let baked = bake(doc, &sm, upto).unwrap();
        assert_same(&baked.original, &original, &format!("{what}: original"));
        assert_same(&baked.filtered, &render_below_filter(doc, &sm, upto).unwrap().unwrap(), &format!("{what}: below filter {upto}"));
    }
}

#[test]
fn a_bake_gives_the_editors_pixels_below_every_filter() {
    for depth in [8, 16] {
        let mut s = filtered(depth);
        assert_bakes_like_the_editor(&s, &format!("{depth}-bit"));
        // Hidden filters are counted by the bake point and skipped.
        s.execute("layer.smartFilter.setVisible", json!({"index": 1, "visible": false})).unwrap();
        assert_bakes_like_the_editor(&s, &format!("{depth}-bit, one hidden"));
    }
}

#[test]
fn a_bake_follows_the_transform_the_warp_and_the_perspective() {
    let mut s = filtered(8);
    edit_smart(&mut s, |sm| sm.transform = Affine { m: [1.3, 0.2, -0.1, 0.9, 5.5, 3.25] });
    assert_bakes_like_the_editor(&s, "affine");
    let doc = s.active().unwrap().doc.clone();
    let source = source_box(&doc, &active_smart(&s)).unwrap();
    edit_smart(&mut s, |sm| sm.warp = Some(Warp::preset(WarpStyle::Arc, 35.0, source)));
    assert_bakes_like_the_editor(&s, "warp");
    edit_smart(&mut s, |sm| {
        sm.warp = None;
        sm.perspective = Some([1.1, 0.05, 4.0, 0.02, 0.95, 2.0, 0.0006, 0.0004, 1.0]);
    });
    assert_bakes_like_the_editor(&s, "perspective");
}

#[test]
fn a_bake_renders_svg_and_stacked_contents_like_the_editor() {
    let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="14"><rect x="2" y="2" width="12" height="9" fill="#c03" stroke="#06f" stroke-width="2"/></svg>"##;
    let mut s = session(8);
    s.edit("place", |doc, active| {
        let mut sm = SmartObject::new(
            SmartSource::Embedded { file_name: "a.svg".into(), bytes: Arc::new(svg.to_vec()) },
            Affine { m: [1.7, 0.0, 0.0, 1.7, 6.0, 4.0] },
            None,
        );
        sm.smart_filters.push(SmartFilter {
            command: "filter.stylize.findEdges".into(),
            params: json!({}),
            blend: BlendMode::Normal,
            opacity: 1.0,
            visible: true,
        });
        sm.smart_filters.push(SmartFilter {
            command: "filter.blur.gaussianBlur".into(),
            params: json!({"radius": 1.5}),
            blend: BlendMode::Normal,
            opacity: 1.0,
            visible: true,
        });
        *active = Some(doc.insert_above(*active, Layer::new("svg", LayerContent::Smart(sm))));
        Ok(())
    })
    .unwrap();
    assert_bakes_like_the_editor(&s, "svg");

    // Three layers stacked by their median.
    let mut inner = Document::new("stack", Size::new(24, 18), photocraft_color::ColorMode::Rgb, photocraft_color::SampleType::U8);
    for (i, c) in [[1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0], [0.2, 0.2, 1.0, 0.6]].iter().enumerate() {
        let mut surf = Surface::new(inner.pixel_format());
        surf.fill_rect(Rect::new(i as i32 * 3, i as i32 * 2, 16 + i as i32 * 2, 12), c);
        inner.layers.push(Layer::new(format!("l{i}"), LayerContent::Raster(surf)));
    }
    let bytes = encode_source(&inner).unwrap();
    let mut s = session(8);
    s.edit("place", |doc, active| {
        let mut sm = SmartObject::new(
            SmartSource::Embedded { file_name: "stack.pcraft".into(), bytes: Arc::new(bytes) },
            Affine { m: [1.0, 0.0, 0.0, 1.0, 8.0, 6.0] },
            None,
        );
        sm.stack_mode = Some(photocraft_doc::StackMode::Median);
        sm.smart_filters.push(SmartFilter {
            command: "filter.stylize.findEdges".into(),
            params: json!({}),
            blend: BlendMode::Normal,
            opacity: 1.0,
            visible: true,
        });
        *active = Some(doc.insert_above(*active, Layer::new("stack", LayerContent::Smart(sm))));
        Ok(())
    })
    .unwrap();
    assert_bakes_like_the_editor(&s, "stack mode");
}

#[test]
fn a_bake_ignores_the_stack_switch_and_leaves_no_cache_behind() {
    let mut s = filtered(8);
    edit_smart(&mut s, |sm| sm.filters_enabled = false);
    let doc = &s.active().unwrap().doc;
    let sm = active_smart(&s);
    // Fresh contents, so nothing an earlier test rendered can be in the caches.
    let mut fresh = sm.clone();
    let SmartSource::Embedded { bytes, .. } = &sm.source else { panic!() };
    let mut contents = decode_source("x.pcraft", bytes).unwrap();
    contents.name = format!("fresh {:p}", &contents);
    let fresh_bytes = encode_source(&contents).unwrap();
    fresh.source = SmartSource::Embedded { file_name: "x.pcraft".into(), bytes: Arc::new(fresh_bytes.clone()) };
    let baked = bake(doc, &fresh, 3).unwrap();
    assert_eq!(source_decode_count(&fresh_bytes, doc.pixel_format()), 0, "the bake decoded it without caching it");
    let mut on = fresh.clone();
    on.filters_enabled = true;
    assert_same(&baked.filtered, &render_below_filter(doc, &on, 3).unwrap().unwrap(), "switched off");
    assert_eq!(source_decode_count(&fresh_bytes, doc.pixel_format()), 1, "the editor's render caches it");
}

#[test]
fn a_bake_reads_nothing_outside_the_document() {
    let s = filtered(8);
    let doc = &s.active().unwrap().doc;
    let sm = active_smart(&s);
    // A linked file the document does not hold.
    let mut linked = sm.clone();
    linked.source = SmartSource::Linked { path: "/etc/hostname".into() };
    assert!(matches!(bake(doc, &linked, 1), Err(BakeError::Refused(m)) if m.contains("not in the document")));
    // Filters that would read a file or run a plug-in.
    for (command, params) in [
        ("filter.distort.displace", json!({"mapPath": "/etc/hostname", "horizontal": 10, "vertical": 10})),
        ("image.adjustments.colorLookup", json!({"file": "/etc/hostname"})),
        ("plugin.run", json!({"id": "anything"})),
    ] {
        let mut f = sm.clone();
        f.smart_filters.insert(0, SmartFilter { command: command.into(), params, blend: BlendMode::Normal, opacity: 1.0, visible: true });
        let err = bake(doc, &f, 1).unwrap_err();
        assert!(matches!(&err, BakeError::Refused(_)), "{command}: {err}");
        // Above the bake point it is not run, so it is not refused either.
        assert!(bake(doc, &f, 0).is_ok(), "{command}");
    }
    // A filter photocraft does not implement (kept from a PSD).
    let mut f = sm.clone();
    f.smart_filters.insert(
        0,
        SmartFilter { command: UNSUPPORTED_FILTER.into(), params: json!({"name": "Oil Paint"}), blend: BlendMode::Normal, opacity: 1.0, visible: true },
    );
    assert!(matches!(bake(doc, &f, 1), Err(BakeError::Refused(m)) if m.contains("Oil Paint")));
}

#[test]
fn placed_contents_are_found_in_the_psd_placed_layer_data() {
    let mut inner = Document::with_background(
        "in",
        Size::new(10, 8),
        photocraft_color::ColorMode::Rgb,
        photocraft_color::SampleType::U8,
        photocraft_color::Color::rgba(0.0, 1.0, 0.0, 1.0),
    );
    inner.layers[0].surface_mut().unwrap().fill_rect(Rect::new(0, 0, 5, 8), &[1.0, 0.0, 0.0, 1.0]);
    let png = photocraft_io::export(&inner, "png", &Default::default()).unwrap().bytes;
    let lnk2 = photocraft_io::linked::encode_linked_file(&photocraft_io::linked::LinkedFile { uuid: "uuid-1".into(), file_name: "in.png".into(), bytes: png });
    let mut doc = Document::new("d", Size::new(W as u32, H as u32), photocraft_color::ColorMode::Rgb, photocraft_color::SampleType::U8);
    doc.metadata.psd_global_blocks.push((*b"8BIM", *b"lnk2", Arc::new(lnk2)));
    let mut sm = SmartObject::new(SmartSource::Linked { path: "uuid-1".into() }, Affine { m: [2.0, 0.0, 0.0, 2.0, 20.0, 10.0] }, None);
    sm.smart_filters.push(SmartFilter { command: "filter.stylize.findEdges".into(), params: json!({}), blend: BlendMode::Normal, opacity: 1.0, visible: true });
    let baked = bake(&doc, &sm, 1).unwrap();
    assert_eq!(baked.original.rgba(23, 15), [1.0, 0.0, 0.0, 1.0]);
    assert_same(&baked.filtered, &render_below_filter(&doc, &sm, 1).unwrap().unwrap(), "lnk2");
}

#[test]
fn a_bake_stops_when_interrupted() {
    let s = filtered(8);
    let doc = &s.active().unwrap().doc;
    let sm = active_smart(&s);
    let at_once = || true;
    assert!(matches!(bake_below_filter(doc, &sm, 3, &options(&decode), &photocraft_raster::Interrupt::cancel_only(&at_once)), Err(BakeError::Cancelled)));
    // Part-way: every check counts, the n-th says stop. Whatever n, it is Cancelled or finished.
    let total = {
        let n = AtomicUsize::new(0);
        let count = || {
            n.fetch_add(1, Ordering::Relaxed);
            false
        };
        bake_below_filter(doc, &sm, 3, &options(&decode), &photocraft_raster::Interrupt::cancel_only(&count)).unwrap();
        n.load(Ordering::Relaxed)
    };
    assert!(total > 8, "checked {total} times");
    for stop in [1, 2, total / 2, total - 1] {
        let n = AtomicUsize::new(0);
        let late = || n.fetch_add(1, Ordering::Relaxed) + 1 >= stop;
        let out = bake_below_filter(doc, &sm, 3, &options(&decode), &photocraft_raster::Interrupt::cancel_only(&late));
        assert!(matches!(out, Err(BakeError::Cancelled)), "stopping at check {stop} of {total}");
    }
}

#[test]
fn budgets_refuse_before_the_work() {
    let mut s = filtered(8);
    let small = |max_pixels: u64| BakeOptions { max_pixels, ..options(&decode) };
    {
        let doc = &s.active().unwrap().doc;
        let sm = active_smart(&s);
        // The canvas alone is 64 × 48.
        let err = bake_below_filter(doc, &sm, 3, &small(1000), &photocraft_raster::Interrupt::NONE).unwrap_err();
        assert!(matches!(&err, BakeError::Limit(_)), "{err}");
        let tight = BakeOptions { render: photocraft_compose::RenderOptions { max_pixels: 100, ..options(&decode).render }, ..options(&decode) };
        assert!(matches!(bake_below_filter(doc, &sm, 3, &tight, &photocraft_raster::Interrupt::NONE), Err(BakeError::Limit(_))));
    }
    // A huge placement is refused at once, before anything is allocated.
    edit_smart(&mut s, |sm| sm.transform = Affine { m: [1e6, 0.0, 0.0, 1e6, 0.0, 0.0] });
    let doc = &s.active().unwrap().doc;
    let started = std::time::Instant::now();
    assert!(matches!(bake(doc, &active_smart(&s), 3), Err(BakeError::Limit(_))));
    assert!(started.elapsed().as_secs() < 5);
    // A perspective that reaches the horizon.
    let mut sm = active_smart(&s);
    sm.transform = Affine::IDENTITY;
    sm.perspective = Some([1.0, 0.0, 0.0, 0.0, 1.0, 0.0, -0.05, 0.0, 1.0]);
    assert!(matches!(bake(doc, &sm, 1), Err(BakeError::Limit(m)) if m.contains("horizon")));
}

#[test]
fn a_bake_point_past_the_stack_is_refused() {
    let s = filtered(8);
    let doc = &s.active().unwrap().doc;
    assert!(matches!(bake(doc, &active_smart(&s), 4), Err(BakeError::Engine(EngineError::BadParams { .. }))));
}

#[test]
fn a_bake_refuses_registered_commands_that_are_not_filters() {
    let s = filtered(8);
    let doc = &s.active().unwrap().doc;
    let mut sm = active_smart(&s);
    sm.smart_filters[0].command = "layer.new.frame".into();
    assert!(crate::commands::find("layer.new.frame").is_some());
    assert!(matches!(bake(doc, &sm, 1), Err(BakeError::Refused(_))));
}

#[test]
fn a_bake_refuses_invalid_special_filter_parameters() {
    let s = filtered(8);
    let doc = &s.active().unwrap().doc;
    let mut sm = active_smart(&s);
    sm.smart_filters[0].command = "filter.liquify".into();
    sm.smart_filters[0].params = json!({"meshSize": 0});
    assert!(bake(doc, &sm, 1).is_err());
}

#[test]
fn a_bake_budgets_filter_windows_before_running_them() {
    let s = filtered(8);
    let doc = &s.active().unwrap().doc;
    let opts = BakeOptions { max_filter_bytes: 1, ..options(&decode) };
    assert!(matches!(bake_below_filter(doc, &active_smart(&s), 1, &opts, &photocraft_raster::Interrupt::NONE), Err(BakeError::Limit(_))));
}

#[test]
fn a_liquify_bake_stops_inside_a_stroke() {
    let s = filtered(8);
    let doc = &s.active().unwrap().doc;
    let mut sm = active_smart(&s);
    sm.smart_filters[0].command = "filter.liquify".into();
    sm.smart_filters[0].params = json!({"strokes": [{"size": 1, "points": [[0, 20], [60, 20], [0, 20], [60, 20]]}]});
    let count = AtomicUsize::new(0);
    let stop = || count.fetch_add(1, Ordering::Relaxed) >= 100;
    let result = bake_below_filter(doc, &sm, 1, &options(&decode), &photocraft_raster::Interrupt::cancel_only(&stop));
    assert!(matches!(result, Err(BakeError::Cancelled)));
    // The same stroke completes when not interrupted.
    assert!(bake(doc, &sm, 1).is_ok());
}

/// A smart filter of each special kernel ([`crate::filters::apply_filter_to_surface_controlled`])
/// with parameters it takes, over the painted block at (-6, 5)-(40, 38).
fn special_filters() -> Vec<(&'static str, Value)> {
    vec![
        (crate::distort_cmds::PUPPET_SMART, json!({"pins": [{"src": [5, 10], "dst": [5, 10]}, {"src": [35, 33], "dst": [45, 25]}]})),
        (
            crate::distort_cmds::PERSPECTIVE_SMART,
            json!({"planes": [{"src": [[0, 5], [40, 5], [40, 38], [0, 38]], "dst": [[2, 8], [44, 3], [40, 41], [0, 36]]}]}),
        ),
        (crate::lens_cmds::LENS, json!({"distortion": 30, "vertical": 20, "redCyan": 30, "vignetteAmount": -30, "angle": 2})),
        (crate::lens_cmds::WIDE, json!({"model": "fisheye", "focalLength": 10, "constraints": [{"a": [5, 10], "b": [38, 14], "orientation": "horizontal"}]})),
        (
            crate::lens_cmds::RAW,
            json!({"clarity": 40, "dehaze": 30, "shadows": 40, "highlights": -30, "noiseLuminance": 30, "sharpenAmount": 50, "whites": 10}),
        ),
        ("image.adjustments.shadowsHighlights", json!({"shadowAmount": 50, "highlightAmount": 30})),
        ("image.adjustments.brightnessContrast", json!({"brightness": 30, "contrast": 20})),
    ]
}

fn with_filter(sm: &SmartObject, command: &str, params: &Value) -> SmartObject {
    let mut sm = sm.clone();
    sm.smart_filters = vec![SmartFilter { command: command.into(), params: params.clone(), blend: BlendMode::Normal, opacity: 1.0, visible: true }];
    sm
}

#[test]
fn special_filters_bake_like_the_editor() {
    for depth in [8, 16, 32] {
        let s = filtered(depth);
        let doc = &s.active().unwrap().doc;
        for (command, params) in special_filters() {
            let sm = with_filter(&active_smart(&s), command, &params);
            let baked = bake(doc, &sm, 1).unwrap_or_else(|e| panic!("{command} at {depth}: {e}"));
            let editor = render_below_filter(doc, &sm, 1).unwrap().unwrap();
            assert!(!baked.filtered.content_bounds().is_empty(), "{command} at {depth}: nothing baked");
            assert_same(&baked.filtered, &editor, &format!("{command} at {depth}"));
        }
    }
}

#[test]
fn special_filters_stop_inside_the_kernel() {
    let s = filtered(16);
    let doc = &s.active().unwrap().doc;
    for (command, params) in special_filters() {
        let sm = with_filter(&active_smart(&s), command, &params);
        let total = {
            let n = AtomicUsize::new(0);
            let count = || {
                n.fetch_add(1, Ordering::Relaxed);
                false
            };
            bake_below_filter(doc, &sm, 1, &options(&decode), &photocraft_raster::Interrupt::cancel_only(&count)).unwrap();
            n.load(Ordering::Relaxed)
        };
        // The bake's own stages check a handful of times; the kernel adds its rows, tiles or steps.
        assert!(total > 12, "{command}: checked {total} times");
        for stop in [total / 2, total - 3] {
            let n = AtomicUsize::new(0);
            let late = || n.fetch_add(1, Ordering::Relaxed) + 1 >= stop;
            let out = bake_below_filter(doc, &sm, 1, &options(&decode), &photocraft_raster::Interrupt::cancel_only(&late));
            assert!(matches!(out, Err(BakeError::Cancelled)), "{command}: stopping at check {stop} of {total}");
        }
    }
}

#[test]
fn hostile_special_filters_are_refused_before_the_work() {
    let s = filtered(8);
    let doc = &s.active().unwrap().doc;
    let base = active_smart(&s);
    // `quick`: refused before the work (not after a solve that places the result).
    let refusal = |command: &str, params: Value, opts: &BakeOptions<'_>, what: &str, quick: bool| {
        let started = std::time::Instant::now();
        let out = bake_below_filter(doc, &with_filter(&base, command, &params), 1, opts, &photocraft_raster::Interrupt::NONE);
        match out {
            Err(BakeError::Limit(m)) => assert!(m.contains(what), "{command}: refused for another reason: {m}"),
            other => panic!("{command}: not a limit: {other:?}"),
        }
        assert!(!quick || started.elapsed().as_secs() < 2, "{command}: took {:?}", started.elapsed());
    };
    let refused = |command: &str, params: Value, opts: &BakeOptions<'_>, what: &str| refusal(command, params, opts, what, true);
    let opts = options(&decode);
    // A pin dragged a million pixels: the desktop leaves the layer as it is; a bake says why.
    refused(crate::distort_cmds::PUPPET_SMART, json!({"pins": [{"src": [5, 10], "dst": [5, 10]}, {"src": [35, 33], "dst": [1e6, 1e6]}]}), &opts, "puppet warp");
    // Pins enough to make binding them the work.
    let pins: Vec<Value> = (0..20_000).map(|i| json!({"src": [5 + i % 30, 10 + i % 20], "dst": [6 + i % 30, 10 + i % 20]})).collect();
    let work = BakeOptions { max_work: 1 << 24, ..options(&decode) };
    refused(crate::distort_cmds::PUPPET_SMART, json!({ "pins": pins }), &work, "pins");
    // More planes than the controlled warp takes, counted before they are parsed.
    let plane = json!({"src": [[0, 5], [40, 5], [40, 38], [0, 38]], "dst": [[2, 8], [44, 3], [40, 41], [0, 36]]});
    refused(crate::distort_cmds::PERSPECTIVE_SMART, json!({ "planes": vec![plane; crate::distort_cmds::MAX_CONTROLLED_PLANES + 1] }), &opts, "planes");
    // A perspective warp thrown far off: its grid is placed, then refused, before it is drawn.
    let far = json!({"planes": [{"src": [[0, 5], [40, 5], [40, 38], [0, 38]], "dst": [[0, 5], [90000, 5], [90000, 70000], [0, 38]]}]});
    refused(crate::distort_cmds::PERSPECTIVE_SMART, far, &opts, "warped surface");
    // Adaptive Wide Angle at 150 % (the most its params take) over a budget that holds the
    // canvas (3072 pixels) but not the scaled image.
    let few = BakeOptions { max_pixels: 5_000, ..options(&decode) };
    // Its warped bounds are known once its mesh is solved.
    refusal(crate::lens_cmds::WIDE, json!({"model": "fisheye", "focalLength": 10, "scale": 150}), &few, "pixels", false);
    // Lens Correction, Camera Raw and Shadows/Highlights past the memory left to them.
    let tight = BakeOptions { max_filter_bytes: 300_000, ..options(&decode) };
    refused(crate::lens_cmds::LENS, json!({"distortion": 30}), &tight, "lens correction");
    refused(crate::lens_cmds::RAW, json!({"clarity": 40}), &tight, "Camera Raw");
    refused("image.adjustments.shadowsHighlights", json!({"shadowAmount": 50}), &tight, "Shadows/Highlights");
}

// ---------- what the contents cache ----------

/// What a layer of [`layered_contents`] stores as its rendering.
#[derive(Clone, Copy, PartialEq)]
enum Cached {
    /// Drawn as the editor draws it.
    Fresh,
    /// A red block where the layer draws nothing (the contents were edited since).
    Stale,
    /// Nothing (a document built without renderings).
    Missing,
}

fn red_block(fmt: PixelFormat) -> Surface {
    let mut s = Surface::new(fmt);
    s.fill_rect(Rect::new(0, 30, 10, 40), &[1.0, 0.0, 0.0, 1.0]);
    s
}

fn title(font: &str, text: &str) -> photocraft_doc::TextLayer {
    photocraft_doc::TextLayer {
        text: text.into(),
        font_family: font.into(),
        size_pt: 18.0,
        color: photocraft_color::Color::BLACK,
        transform: Affine::translate(4.0, 22.0),
        ..Default::default()
    }
}

/// A smart object over a small blue square, with an Invert filter.
fn inner_smart(fmt_depth: SampleType) -> SmartObject {
    let mut square = Document::new("square", Size::new(8, 8), ColorMode::Rgb, fmt_depth);
    let mut px = Surface::new(square.pixel_format());
    px.fill_rect(Rect::new(0, 0, 8, 8), &[0.1, 0.2, 0.9, 1.0]);
    square.layers.push(Layer::new("square", LayerContent::Raster(px)));
    let mut sm = SmartObject::new(
        SmartSource::Embedded { file_name: "square.pcraft".into(), bytes: Arc::new(encode_source(&square).unwrap()) },
        Affine::translate(40.0, 25.0),
        None,
    );
    sm.smart_filters =
        vec![SmartFilter { command: "image.adjustments.invert".into(), params: json!({}), blend: BlendMode::Normal, opacity: 1.0, visible: true }];
    sm
}

/// Contents of 60 × 40 with a fill, a text layer, a shape and a smart object (bottom to top),
/// each storing `cached` as its rendering.
fn layered_contents(cached: Cached, depth: SampleType) -> Document {
    let mut doc = Document::new("contents", Size::new(60, 40), ColorMode::Rgb, depth);
    let (fmt, canvas, dpi) = (doc.pixel_format(), doc.bounds(), doc.resolution_dpi);
    let stale = || (cached == Cached::Stale).then(|| red_block(fmt));
    let fill = photocraft_doc::Fill::Solid(photocraft_color::Color::rgba(0.9, 0.9, 0.8, 0.5));
    let mut backdrop = Layer::new("Backdrop", LayerContent::Fill(fill.clone()));
    backdrop.fill_cache = stale().map(|surface| photocraft_doc::FillCache { fill, surface });
    let mut text = title("Inter", "Hi");
    text.cache = match cached {
        Cached::Fresh => {
            TextEngine::new().render_layer(&mut text, dpi, fmt);
            text.cache
        }
        _ => stale(),
    };
    let mut shape = photocraft_doc::ShapeLayer {
        path: photocraft_vector::shapes::rect(30.0, 4.0, 20.0, 12.0),
        fill: Some(photocraft_doc::Fill::Solid(photocraft_color::Color::rgb(0.0, 0.6, 0.2))),
        ..Default::default()
    };
    shape.cache = match cached {
        Cached::Fresh => Some(photocraft_vector::render_shape(&shape, fmt, canvas)),
        _ => stale(),
    };
    doc.layers = vec![backdrop, Layer::new("Title", LayerContent::Text(text)), Layer::new("Badge", LayerContent::Shape(shape))];
    let mut sm = inner_smart(depth);
    sm.cache = match cached {
        Cached::Fresh => super::render(&doc, &sm).unwrap(),
        _ => stale(),
    };
    doc.layers.push(Layer::new("Inset", LayerContent::Smart(sm)));
    doc
}

/// A page holding `contents` as one smart object, with Find Edges on it.
fn page_with_contents(contents: &Document) -> (Document, SmartObject) {
    let mut page = Document::new("page", Size::new(80, 60), contents.mode, contents.depth);
    let mut sm = SmartObject::new(
        SmartSource::Embedded { file_name: "contents.pcraft".into(), bytes: Arc::new(encode_source(contents).unwrap()) },
        Affine::translate(6.0, 7.0),
        None,
    );
    sm.smart_filters =
        vec![SmartFilter { command: "filter.stylize.findEdges".into(), params: json!({}), blend: BlendMode::Normal, opacity: 1.0, visible: true }];
    page.layers.push(Layer::new("Contents", LayerContent::Smart(sm.clone())));
    (page, sm)
}

#[test]
fn contents_are_drawn_from_what_they_are_not_from_their_caches() {
    for depth in [SampleType::U8, SampleType::U16, SampleType::F32] {
        // The editor's picture of the contents, drawn with their renderings fresh.
        let (page, sm) = page_with_contents(&layered_contents(Cached::Fresh, depth));
        let editor = render_below_filter(&page, &sm, 0).unwrap().unwrap();
        let filtered = render_below_filter(&page, &sm, 1).unwrap().unwrap();
        assert!(!editor.content_bounds().is_empty());
        for cached in [Cached::Fresh, Cached::Stale, Cached::Missing] {
            let (page, sm) = page_with_contents(&layered_contents(cached, depth));
            let baked = bake(&page, &sm, 1).unwrap();
            let what = format!(
                "{depth:?}, caches {}",
                match cached {
                    Cached::Fresh => "fresh",
                    Cached::Stale => "stale",
                    Cached::Missing => "missing",
                }
            );
            assert_same(&baked.original, &editor, &what);
            assert_same(&baked.filtered, &filtered, &what);
        }
        // The stale renderings alone, composited as they are, would differ.
        let (page, sm) = page_with_contents(&layered_contents(Cached::Stale, depth));
        assert!(render_below_filter(&page, &sm, 0).unwrap().unwrap().read_region(editor.content_bounds()) != editor.read_region(editor.content_bounds()));
    }
}

#[test]
fn contents_the_bake_cannot_draw_are_refused_naming_the_layer() {
    let refused = |contents: &Document, opts: &BakeOptions<'_>, want: &[&str]| {
        let (page, sm) = page_with_contents(contents);
        match bake_below_filter(&page, &sm, 1, opts, &photocraft_raster::Interrupt::NONE) {
            Err(BakeError::Refused(m) | BakeError::Limit(m)) => assert!(want.iter().all(|w| m.contains(w)), "{m}"),
            other => panic!("not refused: {other:?}"),
        }
    };
    let opts = options(&decode);
    let with = |l: Layer| {
        let mut doc = layered_contents(Cached::Missing, SampleType::U8);
        doc.layers.push(l);
        doc
    };
    // A font the bake's text engine lacks: a substitute would not be the layer's text.
    refused(&with(Layer::new("Headline", LayerContent::Text(title("No Such Sans", "Hello")))), &opts, &["Headline", "No Such Sans"]);
    // Characters none of its fonts has.
    refused(&with(Layer::new("Caption", LayerContent::Text(title("Inter", "日本語")))), &opts, &["Caption", "none of this server's fonts"]);
    // Text too long to lay out as one step.
    refused(&with(Layer::new("Essay", LayerContent::Text(title("Inter", &"a".repeat(MAX_TEXT_CHARS + 1))))), &opts, &["Essay", "characters"]);
    // A nested smart object with a filter photocraft does not implement.
    let mut odd = inner_smart(SampleType::U8);
    odd.smart_filters[0].command = "psd.unsupportedFilter".into();
    odd.smart_filters[0].params = json!({"name": "Oil Slick"});
    refused(&with(Layer::new("Odd", LayerContent::Smart(odd))), &opts, &["Odd", "Oil Slick"]);
    // A nested smart object whose linked file is not in the contents.
    let lost = SmartObject::new(SmartSource::Linked { path: "/elsewhere/photo.psd".into() }, Affine::IDENTITY, Some(red_block(PixelFormat::RGBA8)));
    refused(&with(Layer::new("Lost", LayerContent::Smart(lost))), &opts, &["Lost", "not in the contents"]);
    // Nested in a group, named by its path.
    refused(&with(Layer::group("Header", vec![Layer::new("Kicker", LayerContent::Text(title("No Such Sans", "x")))])), &opts, &["\"Header\" › \"Kicker\""]);
    // Nesting past the depth: the contents are level 1 and their inset level 2.
    let shallow = BakeOptions { max_depth: 1, ..options(&decode) };
    refused(&layered_contents(Cached::Missing, SampleType::U8), &shallow, &["Inset", "deep"]);
    let few = BakeOptions { render: photocraft_compose::RenderOptions { max_layers: 0, ..options(&decode).render }, ..options(&decode) };
    refused(&layered_contents(Cached::Missing, SampleType::U8), &few, &["Inset", "smart objects"]);
}

#[test]
fn a_host_text_engine_is_used_when_given() {
    // The host's engine knows a family the bundled fonts lack (here: Inter registered again under
    // its own name is enough to show the engine is the host's; a missing family is refused above).
    let engine = std::sync::Mutex::new(TextEngine::new());
    let opts = BakeOptions { text: Some(&engine), ..options(&decode) };
    let (page, sm) = page_with_contents(&layered_contents(Cached::Stale, SampleType::U8));
    let baked = bake_below_filter(&page, &sm, 1, &opts, &photocraft_raster::Interrupt::NONE).unwrap();
    let (fresh, fsm) = page_with_contents(&layered_contents(Cached::Fresh, SampleType::U8));
    assert_same(&baked.original, &render_below_filter(&fresh, &fsm, 0).unwrap().unwrap(), "host engine");
}

#[test]
fn a_nested_bake_stops_when_interrupted() {
    let (page, sm) = page_with_contents(&layered_contents(Cached::Stale, SampleType::U8));
    let total = {
        let n = AtomicUsize::new(0);
        let count = || {
            n.fetch_add(1, Ordering::Relaxed);
            false
        };
        bake_below_filter(&page, &sm, 1, &options(&decode), &photocraft_raster::Interrupt::cancel_only(&count)).unwrap();
        n.load(Ordering::Relaxed)
    };
    for stop in [1, 3, total / 4, total / 2, total - 1] {
        let n = AtomicUsize::new(0);
        let late = || n.fetch_add(1, Ordering::Relaxed) + 1 >= stop;
        let out = bake_below_filter(&page, &sm, 1, &options(&decode), &photocraft_raster::Interrupt::cancel_only(&late));
        assert!(matches!(out, Err(BakeError::Cancelled)), "stopping at check {stop} of {total}");
    }
}

#[test]
fn mesh_placement_obeys_the_solver_budget_without_a_filter() {
    let mut s = filtered(8);
    let doc = s.active().unwrap().doc.clone();
    let source = source_box(&doc, &active_smart(&s)).unwrap();
    edit_smart(&mut s, |sm| sm.warp = Some(Warp::preset(WarpStyle::Arc, 35.0, source)));
    let opts = BakeOptions { max_work: 1, ..options(&decode) };
    let out = bake_below_filter(&s.active().unwrap().doc, &active_smart(&s), 0, &opts, &photocraft_raster::Interrupt::NONE);
    assert!(matches!(out, Err(BakeError::Limit(m)) if m.contains("warp")), "placement must use the controlled kernel");
}

#[test]
fn regenerated_caches_share_one_retained_budget() {
    let mut doc = Document::new("shapes", Size::new(256, 256), ColorMode::Rgb, SampleType::U8);
    for i in 0..8 {
        doc.layers.push(Layer::new(
            format!("shape {i}"),
            LayerContent::Shape(photocraft_doc::ShapeLayer {
                path: photocraft_vector::shapes::rect(0.0, 0.0, 256.0, 256.0),
                fill: Some(photocraft_doc::Fill::Solid(photocraft_color::Color::BLACK)),
                ..Default::default()
            }),
        ));
    }
    let (page, sm) = page_with_contents(&doc);
    let opts = BakeOptions { max_stack_bytes: 4 * 256 * 256 * 4, ..options(&decode) };
    let out = bake_below_filter(&page, &sm, 0, &opts, &photocraft_raster::Interrupt::NONE);
    assert!(matches!(out, Err(BakeError::Limit(m)) if m.contains("retained surface")));
    assert!(bake_below_filter(&page, &sm, 0, &options(&decode), &photocraft_raster::Interrupt::NONE).is_ok());
}

#[test]
fn invisible_contents_do_not_need_fonts_or_linked_sources() {
    let mut doc = layered_contents(Cached::Missing, SampleType::U8);
    let (base, sm) = page_with_contents(&doc);
    let expected = bake(&base, &sm, 0).unwrap();
    let mut hidden = Layer::group("hidden", vec![Layer::new("missing font", LayerContent::Text(title("Not Installed", "Hello")))]);
    hidden.visible = false;
    doc.layers.push(hidden);
    let (page, sm) = page_with_contents(&doc);
    assert_same(&bake(&page, &sm, 0).unwrap().original, &expected.original, "hidden group");
}

#[test]
fn nested_text_cap_is_one_short_layout_unit() {
    let mut doc = Document::new("text", Size::new(64, 64), ColorMode::Rgb, SampleType::U8);
    doc.layers.push(Layer::new("essay", LayerContent::Text(title("Inter", &"x".repeat(16_385)))));
    let (page, sm) = page_with_contents(&doc);
    let out = bake(&page, &sm, 0);
    assert!(matches!(out, Err(BakeError::Limit(m)) if m.contains("characters")));
}

#[test]
fn text_metadata_cannot_bypass_the_character_cap() {
    let mut text = title("Inter", "x");
    text.runs = vec![photocraft_doc::text::TextRun { len: 0, style: Default::default() }; 16_385];
    let mut doc = Document::new("text", Size::new(64, 64), ColorMode::Rgb, SampleType::U8);
    doc.layers.push(Layer::new("runs", LayerContent::Text(text)));
    let (page, sm) = page_with_contents(&doc);
    let out = bake(&page, &sm, 0);
    assert!(matches!(out, Err(BakeError::Limit(m)) if m.contains("style runs")));
}

#[test]
fn plane_count_is_refused_before_parsing_the_planes() {
    let mut s = filtered(8);
    edit_smart(&mut s, |sm| {
        sm.smart_filters = vec![SmartFilter {
            command: crate::distort_cmds::PERSPECTIVE_SMART.into(),
            params: json!({"planes": vec![Value::Null; crate::distort_cmds::MAX_CONTROLLED_PLANES + 1]}),
            blend: BlendMode::Normal,
            opacity: 1.0,
            visible: true,
        }];
    });
    let out = bake(&s.active().unwrap().doc, &active_smart(&s), 1);
    assert!(matches!(out, Err(BakeError::Limit(m)) if m.contains("planes")));
}

fn refresh(doc: &mut Document, which: Renderings) -> std::result::Result<Vec<String>, BakeError> {
    refresh_renderings(doc, which, &options(&decode), &photocraft_raster::Interrupt::NONE)
}

/// The rendering a layer stores: its cache, or its fill cache.
fn rendering(l: &Layer) -> Option<&Surface> {
    match &l.content {
        LayerContent::Fill(_) => l.fill_cache.as_ref().map(|c| &c.surface),
        _ => l.surface(),
    }
}

#[test]
fn a_refresh_draws_every_rendering_again_hidden_layers_included() {
    for depth in [SampleType::U8, SampleType::U16, SampleType::F32] {
        let fresh = layered_contents(Cached::Fresh, depth);
        for cached in [Cached::Fresh, Cached::Stale, Cached::Missing] {
            let mut doc = layered_contents(cached, depth);
            // A hidden layer is drawn too: the editor shows it once it is turned on.
            doc.layers[1].visible = false;
            let kept = refresh(&mut doc, Renderings::ALL).unwrap();
            assert!(kept.is_empty(), "{kept:?}");
            for (got, want) in doc.layers.iter().zip(&fresh.layers) {
                let what = format!("{depth:?}, layer {}", got.name);
                match (&got.content, rendering(want)) {
                    // The solid backdrop is drawn from its colour, so it keeps no rendering at all.
                    (LayerContent::Fill(_), _) => assert!(got.fill_cache.is_none(), "{what}"),
                    (_, Some(want)) => assert_same(rendering(got).expect("drawn"), want, &what),
                    (_, None) => panic!("{what}: the fresh document has no rendering"),
                }
            }
            // The document draws as the editor draws it.
            let mut editor = fresh.clone();
            editor.layers[1].visible = false;
            let (a, b) = (photocraft_compose::flatten(&doc), photocraft_compose::flatten(&editor));
            assert!(a.px == b.px, "{depth:?}: the refreshed document draws differently");
        }
    }
}

#[test]
fn a_refresh_draws_only_the_kinds_asked_for() {
    let mut doc = layered_contents(Cached::Stale, SampleType::U8);
    refresh(&mut doc, Renderings { shapes: true, fills: true, ..Renderings::default() }).unwrap();
    let red = red_block(doc.pixel_format());
    let fresh = layered_contents(Cached::Fresh, SampleType::U8);
    // Text and the smart object keep the stale renderings; the shape is drawn again.
    assert_same(rendering(&doc.layers[1]).unwrap(), &red, "text");
    assert_same(rendering(&doc.layers[3]).unwrap(), &red, "smart object");
    assert_same(rendering(&doc.layers[2]).unwrap(), rendering(&fresh.layers[2]).unwrap(), "shape");
}

#[test]
fn a_layer_the_refresh_cannot_draw_keeps_its_rendering_and_is_named() {
    let mut doc = layered_contents(Cached::Stale, SampleType::U8);
    let fmt = doc.pixel_format();
    let mut headline = title("No Such Sans", "Hello");
    headline.cache = Some(red_block(fmt));
    doc.layers.push(Layer::group("Header", vec![Layer::new("Headline", LayerContent::Text(headline))]));
    let lost = SmartObject::new(SmartSource::Linked { path: "/elsewhere/photo.psd".into() }, Affine::IDENTITY, Some(red_block(fmt)));
    doc.layers.push(Layer::new("Lost", LayerContent::Smart(lost)));
    let kept = refresh(&mut doc, Renderings::ALL).unwrap();
    assert_eq!(kept.len(), 2, "{kept:?}");
    assert!(kept[0].contains("\"Header\" › \"Headline\"") && kept[0].contains("No Such Sans"), "{}", kept[0]);
    assert!(kept[1].contains("\"Lost\"") && kept[1].contains("not in the contents"), "{}", kept[1]);
    let red = red_block(fmt);
    assert_same(rendering(&doc.layers[4].children().unwrap()[0]).unwrap(), &red, "the headline keeps its rendering");
    assert_same(rendering(&doc.layers[5]).unwrap(), &red, "the lost smart object keeps its rendering");
    // The rest was drawn again.
    let fresh = layered_contents(Cached::Fresh, SampleType::U8);
    assert_same(rendering(&doc.layers[1]).unwrap(), rendering(&fresh.layers[1]).unwrap(), "text");
}

#[test]
fn a_gradient_fill_gets_the_pixels_the_compositor_draws_for_it() {
    for depth in [SampleType::U8, SampleType::U16] {
        let mut doc = Document::new("fills", Size::new(600, 300), ColorMode::Rgb, depth);
        let gradient = photocraft_doc::Fill::gradient(
            vec![(0.0, photocraft_color::Color::rgb(0.9, 0.1, 0.1)), (1.0, photocraft_color::Color::rgb(0.1, 0.1, 0.9))],
            30.0,
            1.0,
            photocraft_doc::GradientStyle::Linear,
            false,
        );
        doc.layers.push(Layer::new("Wash", LayerContent::Fill(gradient.clone())));
        let live = photocraft_compose::flatten(&doc);
        refresh(&mut doc, Renderings { fills: true, ..Renderings::default() }).unwrap();
        let cache = doc.layers[0].fill_cache.as_ref().expect("a fill cache");
        assert_eq!(cache.fill, gradient);
        assert_eq!(cache.surface.content_bounds(), doc.bounds(), "{depth:?}");
        // The cache is pixels of the document's depth; the live gradient is float.
        let level = if depth == SampleType::U8 { 1.0 / 255.0 } else { 1.0 / 65535.0 };
        let got = photocraft_compose::flatten(&doc);
        let worst = got.px.iter().zip(&live.px).flat_map(|(a, b)| a.iter().zip(b).map(|(x, y)| (x - y).abs())).fold(0f32, f32::max);
        assert!(worst <= level, "{depth:?}: the cached fill is {worst} away from the live one");
    }
}

#[test]
fn budgets_and_the_interrupt_end_a_refresh() {
    let mut doc = layered_contents(Cached::Missing, SampleType::U8);
    let tight = BakeOptions { max_stack_bytes: 1, ..options(&decode) };
    match refresh_renderings(&mut doc, Renderings::ALL, &tight, &photocraft_raster::Interrupt::NONE) {
        Err(BakeError::Limit(m)) => assert!(m.contains("limit") || m.contains("budget"), "{m}"),
        other => panic!("not refused: {other:?}"),
    }
    let mut doc = layered_contents(Cached::Missing, SampleType::U8);
    let stop = || true;
    assert!(matches!(
        refresh_renderings(&mut doc, Renderings::ALL, &options(&decode), &photocraft_raster::Interrupt::cancel_only(&stop)),
        Err(BakeError::Cancelled)
    ));
}
