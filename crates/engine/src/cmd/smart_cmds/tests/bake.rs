use std::sync::atomic::{AtomicUsize, Ordering};

use photocraft_geom::warp::{Warp, WarpStyle};

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
