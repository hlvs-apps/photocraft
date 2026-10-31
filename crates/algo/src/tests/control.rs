//! The controlled kernels ([`crate::control`]): the desktop's pixels when nothing refuses, a stop
//! at whichever interrupt check says so, and hostile parameters refused before the work.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use photocraft_raster::Interrupt;

use super::*;
use crate::camera_raw::CameraRaw;
use crate::control::{Budget, Refused};
use crate::lens::LensCorrection;
use crate::puppet::{PuppetDensity, PuppetMode, PuppetPin, PuppetWarp};
use crate::tone::ShadowsHighlights;
use crate::transform::Interp;
use crate::wideangle::{Constraint, Orientation, WideAngle, WideModel};

const DEPTHS: [SampleType; 3] = [SampleType::U8, SampleType::U16, SampleType::F32];

/// An opaque L-shaped blob over a transparent field, the shape a puppet warp meshes.
fn blob(s: SampleType) -> Surface {
    let mut surf = Surface::new(fmt(s));
    surf.fill_rect(Rect::new(20, 10, 36, 70), &[0.8, 0.3, 0.1, 1.0]);
    surf.fill_rect(Rect::new(36, 56, 70, 70), &[0.2, 0.5, 0.9, 1.0]);
    surf
}

fn pin(src: [f64; 2], dst: [f64; 2]) -> PuppetPin {
    PuppetPin { src, dst, rotate: None, depth: 0 }
}

fn puppet() -> PuppetWarp {
    PuppetWarp {
        pins: vec![pin([28.0, 15.0], [28.0, 15.0]), pin([64.0, 63.0], [70.0, 44.0])],
        mode: PuppetMode::Normal,
        density: PuppetDensity::Normal,
        expansion: 2.0,
    }
}

fn bend(x: f64, y: f64) -> (f64, f64) {
    (x + 0.2 * y, y + 5.0 * (x / 10.0).sin())
}

fn lens() -> LensCorrection {
    LensCorrection { distortion: 30.0, red_cyan: 40.0, vignette_amount: -40.0, vertical: 20.0, angle: 3.0, ..Default::default() }
}

fn wide() -> WideAngle {
    WideAngle {
        model: WideModel::Fisheye,
        focal_length: 10.0,
        constraints: vec![
            Constraint { a: [4.0, 5.0], b: [36.0, 9.0], orientation: Orientation::Horizontal },
            Constraint { a: [8.0, 26.0], b: [33.0, 21.0], orientation: Orientation::Free },
        ],
        ..Default::default()
    }
}

/// Every spatial stage of Camera Raw, and the per-pixel ones around them.
fn raw() -> CameraRaw {
    CameraRaw {
        temperature: 20.0,
        exposure: 0.3,
        contrast: 25.0,
        highlights: -40.0,
        shadows: 50.0,
        whites: 20.0,
        blacks: -15.0,
        texture: 30.0,
        clarity: 40.0,
        dehaze: 35.0,
        vibrance: 20.0,
        saturation: 10.0,
        sharpen_amount: 60.0,
        sharpen_masking: 30.0,
        noise_luminance: 40.0,
        noise_color: 30.0,
        grain_amount: 20.0,
        vignette_amount: -30.0,
        ..Default::default()
    }
}

fn shadows() -> ShadowsHighlights {
    ShadowsHighlights { shadow_amount: 50.0, highlight_amount: 30.0, black_clip: 1.0, white_clip: 1.0, ..Default::default() }
}

fn rgba(s: &Surface, r: Rect) -> Vec<[f32; 4]> {
    let mut v = vec![[0.0f32; 4]; r.size().area() as usize];
    s.read_rgba_into(r, &mut v);
    v
}

/// A kernel under test: its controlled run with `budget` and `ctl`.
type Run<'a> = Box<dyn Fn(&Budget, &Interrupt) -> Result<Vec<f32>, Refused> + 'a>;

/// The six kernels over `r` at `s`'s depth, each with the desktop's own result to compare with.
fn kernels(s: SampleType, r: Rect) -> Vec<(&'static str, Vec<f32>, Run<'static>)> {
    let src = pattern(s, r);
    let shape = blob(s);
    let pixels = |o: &Surface| o.read_region(o.content_bounds());
    let px = rgba(&src, r);
    let (w, h) = (r.width() as usize, r.height() as usize);
    let float = s == SampleType::F32;
    let mut developed = px.clone();
    crate::camera_raw::develop(&mut developed, w, h, &raw(), float);
    let mut lifted = px.clone();
    crate::tone::shadows_highlights(&mut lifted, w, h, &shadows());
    let flat = |v: &[[f32; 4]]| v.iter().flatten().copied().collect::<Vec<f32>>();
    vec![
        ("puppet", pixels(&crate::puppet::puppet_warp(&shape, shape.content_bounds(), &puppet(), Interp::Bicubic).unwrap()), {
            let shape = shape.clone();
            Box::new(move |b, c| crate::puppet::puppet_warp_controlled(&shape, shape.content_bounds(), &puppet(), Interp::Bicubic, b, c).map(|o| pixels(&o)))
        }),
        ("mesh warp", pixels(&crate::warp::warp_mesh_surface(&src, r, &bend, Interp::Bicubic)), {
            let src = src.clone();
            Box::new(move |b, c| crate::warp::warp_mesh_surface_controlled(&src, r, &bend, Interp::Bicubic, b, c).map(|o| pixels(&o)))
        }),
        ("lens", pixels(&crate::lens::correct(&src, r, &lens())), {
            let src = src.clone();
            Box::new(move |b, c| crate::lens::correct_controlled(&src, r, &lens(), b, c).map(|o| pixels(&o)))
        }),
        ("wide angle", pixels(&crate::wideangle::apply(&src, r, &wide(), Interp::Bicubic)), {
            let src = src.clone();
            Box::new(move |b, c| crate::wideangle::apply_controlled(&src, r, &wide(), Interp::Bicubic, b, c).map(|o| pixels(&o)))
        }),
        ("camera raw", flat(&developed), {
            let px = px.clone();
            Box::new(move |_, c| {
                let mut v = px.clone();
                crate::camera_raw::develop_with(&mut v, w, h, &raw(), float, c)?;
                Ok(flat(&v))
            })
        }),
        ("shadows/highlights", flat(&lifted), {
            Box::new(move |_, c| {
                let mut v = px.clone();
                crate::tone::shadows_highlights_with(&mut v, w, h, &shadows(), c)?;
                Ok(flat(&v))
            })
        }),
    ]
}

#[test]
fn controlled_kernels_give_the_desktop_pixels() {
    for s in DEPTHS {
        for (name, want, run) in kernels(s, R) {
            let got = run(&Budget::UNLIMITED, &Interrupt::NONE).unwrap_or_else(|e| panic!("{name} {s:?}: {e}"));
            assert!(!want.is_empty(), "{name} {s:?}: the desktop made nothing");
            assert!(got == want, "{name} {s:?}: the controlled pixels differ");
        }
    }
}

#[test]
fn controlled_kernels_stop_at_whichever_check_says_so() {
    // Big enough for several tiles of each tiled kernel.
    for (name, _, run) in kernels(SampleType::U16, Rect::new(0, 0, 600, 400)) {
        // Every check counts; the n-th says stop. Whatever n, the run is Cancelled.
        let total = {
            let n = AtomicUsize::new(0);
            let count = || {
                n.fetch_add(1, Ordering::Relaxed);
                false
            };
            run(&Budget::UNLIMITED, &Interrupt::cancel_only(&count)).unwrap();
            n.load(Ordering::Relaxed)
        };
        assert!(total > 8, "{name}: checked {total} times");
        for stop in [1, 2, total / 3, total / 2, total - 1] {
            let n = AtomicUsize::new(0);
            let late = || n.fetch_add(1, Ordering::Relaxed) + 1 >= stop;
            assert_eq!(run(&Budget::UNLIMITED, &Interrupt::cancel_only(&late)), Err(Refused::Cancelled), "{name}: stopping at check {stop} of {total}");
        }
    }
}

/// `f` is a [`Refused::Limit`] mentioning `what`, and it came quickly (before the work).
fn refused(what: &str, f: impl FnOnce() -> Result<Surface, Refused>) {
    let started = Instant::now();
    refused_after_work(what, f);
    assert!(started.elapsed() < Duration::from_secs(2), "{what}: took {:?}", started.elapsed());
}

/// `f` is a [`Refused::Limit`] mentioning `what`, which it may know only after part of the work
/// (a solve that places the result).
fn refused_after_work(what: &str, f: impl FnOnce() -> Result<Surface, Refused>) {
    match f() {
        Err(Refused::Limit(m)) => assert!(m.contains(what), "refused for another reason: {m}"),
        Err(Refused::Cancelled) => panic!("{what}: cancelled, not refused"),
        Ok(_) => panic!("{what}: not refused"),
    }
}

const SMALL: Budget = Budget { max_pixels: 1 << 20, max_bytes: 1 << 28, max_work: 1 << 26 };

#[test]
fn a_pin_dragged_far_is_held_to_the_budget_not_the_desktop_cap() {
    let shape = blob(SampleType::U8);
    let mut w = puppet();
    // 2²⁰ pixels at most: a pin a few thousand pixels away stretches the mesh past it (well under
    // the desktop's 2³⁰).
    w.pins[1].dst = [4000.0, 3000.0];
    refused("the puppet warp", || crate::puppet::puppet_warp_controlled(&shape, shape.content_bounds(), &w, Interp::Bicubic, &SMALL, &Interrupt::NONE));
    assert!(crate::puppet::puppet_warp(&shape, shape.content_bounds(), &w, Interp::Bilinear).is_some());
}

#[test]
fn a_puppet_over_a_checkerboard_is_refused_before_meshing_it() {
    // A checkerboard over 1200²: an outline of a hundred thousand loops, which the desktop would
    // mesh (and solve) for minutes.
    // 2 × 2-pixel cells: the outline field's blocks are 2 pixels here, so every cell is a loop
    // (1-pixel cells would merge into one solid block).
    let mut s = Surface::new(fmt(SampleType::U8));
    let side = 1200;
    let mut v = vec![0.0f32; side * side * 4];
    for (i, p) in v.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        if (i % side / 2 + i / side / 2) % 2 == 0 {
            p.copy_from_slice(&[1.0, 1.0, 1.0, 1.0]);
        }
    }
    s.write_region(Rect::new(0, 0, side as i32, side as i32), &v);
    let w = PuppetWarp { pins: vec![pin([10.0, 10.0], [12.0, 10.0])], expansion: 0.0, ..puppet() };
    let budget = Budget { max_pixels: 1 << 24, max_bytes: 1 << 30, max_work: 1 << 32 };
    refused("outline", || crate::puppet::puppet_warp_controlled(&s, s.content_bounds(), &w, Interp::Bilinear, &budget, &Interrupt::NONE));
}

#[test]
fn many_pins_are_refused_before_binding_them() {
    let shape = blob(SampleType::U8);
    let w = PuppetWarp { pins: (0..200_000).map(|i| pin([28.0, 15.0 + f64::from(i % 50)], [28.0, 15.0])).collect(), ..puppet() };
    refused("pins", || crate::puppet::puppet_warp_controlled(&shape, shape.content_bounds(), &w, Interp::Bicubic, &SMALL, &Interrupt::NONE));
}

#[test]
fn a_mesh_warp_thrown_far_or_folded_is_refused() {
    let src = pattern(SampleType::U8, R);
    let far = |x: f64, y: f64| (x * 400.0, y * 400.0);
    refused("the warped surface", || crate::warp::warp_mesh_surface_controlled(&src, R, &far, Interp::Bilinear, &SMALL, &Interrupt::NONE));
    // Every cell folded onto the same 600² square: few pixels, but each tested again per cell.
    let fold = |x: f64, y: f64| ((x * 37.0) % 600.0, (y * 41.0) % 600.0);
    let budget = Budget { max_work: 1 << 22, ..SMALL };
    refused("triangles", || crate::warp::warp_mesh_surface_controlled(&src, R, &fold, Interp::Bilinear, &budget, &Interrupt::NONE));
    // A vertex at infinity: the desktop draws nothing, the controlled warp says why.
    let inf = |x: f64, y: f64| if x > 20.0 { (f64::INFINITY, y) } else { (x, y) };
    refused("infinity", || crate::warp::warp_mesh_surface_controlled(&src, R, &inf, Interp::Bilinear, &SMALL, &Interrupt::NONE));
}

#[test]
fn a_tile_reading_past_its_share_is_refused_not_left_empty() {
    // A 64× minification: the one destination tile reads the whole 1024² source (16 MiB of f32
    // RGBA), past what one worker's share leaves after the bookkeeping. One worker, so the share
    // is the same on every machine.
    let src = pattern(SampleType::U8, Rect::new(0, 0, 1024, 1024));
    let shrink = |x: f64, y: f64| (x / 64.0, y / 64.0);
    let budget = Budget { max_pixels: 1 << 24, max_bytes: 17 << 20, max_work: 1 << 30 };
    let pool = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    pool.install(|| {
        refused("bookkeeping", || {
            crate::warp::warp_mesh_surface_controlled(&src, Rect::new(0, 0, 1024, 1024), &shrink, Interp::Bilinear, &budget, &Interrupt::NONE)
        });
        let window = Budget { max_bytes: 48 << 20, ..budget };
        refused("source window", || {
            crate::warp::warp_mesh_surface_controlled(&src, Rect::new(0, 0, 1024, 1024), &shrink, Interp::Bilinear, &window, &Interrupt::NONE)
        });
    });
    // With room for the window it draws.
    let roomy = Budget { max_bytes: 64 << 20, ..budget };
    pool.install(|| {
        assert!(crate::warp::warp_mesh_surface_controlled(&src, Rect::new(0, 0, 1024, 1024), &shrink, Interp::Bilinear, &roomy, &Interrupt::NONE).is_ok())
    });
}

#[test]
fn a_lens_correction_over_its_budget_is_refused() {
    let src = pattern(SampleType::U8, R);
    let tiny = Budget { max_pixels: 100, ..SMALL };
    refused("the corrected image", || crate::lens::correct_controlled(&src, R, &lens(), &tiny, &Interrupt::NONE));
    let no_room = Budget { max_bytes: 1 << 10, ..SMALL };
    refused("lens correction", || crate::lens::correct_controlled(&src, R, &lens(), &no_room, &Interrupt::NONE));
}

#[test]
fn a_wide_angle_scaled_far_or_overconstrained_is_refused() {
    let src = pattern(SampleType::U8, R);
    let big = WideAngle { scale: 10_000.0, ..wide() };
    // The scale clamps at 10×: 100× the frame's pixels. The solver's mesh is the same whatever
    // the frame (48 cells across), so its work is not this small frame's, and the warped bounds
    // are known once it is solved (refused before they are drawn).
    let budget = Budget { max_pixels: 10_000, max_work: 1 << 32, ..SMALL };
    refused_after_work("pixels", || crate::wideangle::apply_controlled(&src, R, &big, Interp::Bilinear, &budget, &Interrupt::NONE));
    let many = WideAngle { constraints: vec![Constraint { a: [4.0, 5.0], b: [36.0, 9.0], orientation: Orientation::Free }; 100_000], ..wide() };
    refused("constraints", || crate::wideangle::apply_controlled(&src, R, &many, Interp::Bilinear, &SMALL, &Interrupt::NONE));
}

#[test]
fn geometry_refuses_memory_before_building_its_grid_or_solver() {
    let src = blob(SampleType::U8);
    let bounds = Rect::new(0, 0, 80, 80);
    let budget = Budget { max_bytes: 1024, ..Budget::UNLIMITED };
    let calls = AtomicUsize::new(0);
    let map = |x, y| {
        calls.fetch_add(1, Ordering::Relaxed);
        (x, y)
    };
    let out = crate::warp::warp_mesh_surface_controlled(&src, bounds, &map, Interp::Bicubic, &budget, &Interrupt::NONE);
    assert!(matches!(out, Err(Refused::Limit(m)) if m.contains("warp grid")));
    // The fixed map-length probes are allowed, but no allocated grid is mapped.
    assert!(calls.load(Ordering::Relaxed) < 1000);
    let out = crate::wideangle::apply_controlled(&src, bounds, &wide(), Interp::Bicubic, &budget, &Interrupt::NONE);
    assert!(matches!(out, Err(Refused::Limit(m)) if m.contains("wide-angle solver")));
}
