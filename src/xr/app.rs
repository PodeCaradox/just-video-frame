//! The OpenXR frame loop for the whole app: a browser panel, video playback
//! with a control bar, and controller input (point, click, drag).

use super::context::{VIEW_TYPE, XrContext};
use super::input::{Input, InputState, Ray};
use super::player::{Placement, PlayOptions, PlayStats, Playback, ViewOptions, eye_params};
use super::renderer::{QuadTarget, Renderer};
use crate::ui::canvas::Fonts;
use crate::ui::navigator::Navigator;
use crate::ui::{browser, captions, controls};
use crate::vr::Projection;
use anyhow::Context;
use openxr as xr;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering},
    },
    time::{Duration, Instant},
};

const CURSOR_PX: u32 = 64;
const CURSOR_SIZE: f32 = 0.035;

pub struct AppOptions {
    pub view: ViewOptions,
    pub play: PlayOptions,
    pub quit: Arc<AtomicBool>,
}

/// A flat UI panel floating in the LOCAL space.
#[derive(Clone, Copy)]
struct Panel {
    center: [f32; 3],
    /// Turn around +Y (radians), then tilt around the panel's X axis.
    yaw: f32,
    tilt: f32,
    size: [f32; 2],
    pixels: [u32; 2],
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn scale(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

impl Panel {
    fn orientation(&self) -> xr::Quaternionf {
        let (sy, cy) = (self.yaw / 2.0).sin_cos();
        let (sx, cx) = (self.tilt / 2.0).sin_cos();
        // q = q_yaw · q_tilt
        xr::Quaternionf {
            x: cy * sx,
            y: sy * cx,
            z: -sy * sx,
            w: cy * cx,
        }
    }

    /// Unit vectors: right, up, normal (towards the viewer).
    fn basis(&self) -> [[f32; 3]; 3] {
        let (sy, cy) = self.yaw.sin_cos();
        let (st, ct) = self.tilt.sin_cos();
        let right = [cy, 0.0, -sy];
        let up = [sy * st, ct, cy * st];
        let normal = [sy * ct, -st, cy * ct];
        [right, up, normal]
    }

    /// Canvas pixel hit by `ray`, if any.
    fn hit(&self, ray: &Ray) -> Option<(f32, f32)> {
        let [right, up, normal] = self.basis();
        let denom = dot(ray.direction, normal);
        if denom.abs() < 1e-4 {
            return None;
        }
        let to_center = [
            self.center[0] - ray.origin[0],
            self.center[1] - ray.origin[1],
            self.center[2] - ray.origin[2],
        ];
        let t = dot(to_center, normal) / denom;
        if t <= 0.0 {
            return None;
        }
        let p = add(ray.origin, scale(ray.direction, t));
        let local = [
            p[0] - self.center[0],
            p[1] - self.center[1],
            p[2] - self.center[2],
        ];
        let u = dot(local, right) / self.size[0] + 0.5;
        let v = 0.5 - dot(local, up) / self.size[1];
        ((0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v))
            .then_some((u * self.pixels[0] as f32, v * self.pixels[1] as f32))
    }

    /// World position of canvas pixel (x, y), `lift` metres towards the viewer.
    fn point(&self, x: f32, y: f32, lift: f32) -> [f32; 3] {
        let [right, up, normal] = self.basis();
        let u = (x / self.pixels[0] as f32 - 0.5) * self.size[0];
        let v = (0.5 - y / self.pixels[1] as f32) * self.size[1];
        add(
            add(add(self.center, scale(right, u)), scale(up, v)),
            scale(normal, lift),
        )
    }

    fn pose(&self) -> xr::Posef {
        xr::Posef {
            orientation: self.orientation(),
            position: xr::Vector3f {
                x: self.center[0],
                y: self.center[1],
                z: self.center[2],
            },
        }
    }
}

/// The browser 2.2 m ahead: far enough that pointing needs only small hand
/// movements, and scaled to look as big as 1.6 × 1.0 m at 1.5 m.
const BROWSER_PANEL: Panel = Panel {
    center: [0.0, -0.147, -2.2],
    yaw: 0.0,
    tilt: 0.0,
    size: [2.347, 1.467],
    pixels: [browser::WIDTH, browser::HEIGHT],
};

/// Preferred distance of the control bar; it looks as big as 1.2 m wide at 1 m.
const CONTROLS_DISTANCE: f32 = 1.8;

/// The control bar ahead of the head, below eye level, tilted towards it:
/// [`CONTROLS_DISTANCE`] away, but never beyond `max_distance` (in front of
/// the screen), and scaled with its distance so it always looks the same size.
fn controls_panel(head: &xr::Posef, max_distance: f32) -> Panel {
    let d = CONTROLS_DISTANCE.min(max_distance).max(0.5);
    let q = head.orientation;
    // Forward (-Z) of the head, flattened to the horizon.
    let forward = [
        -2.0 * (q.x * q.z + q.w * q.y),
        0.0,
        -(1.0 - 2.0 * (q.x * q.x + q.y * q.y)),
    ];
    let yaw = (-forward[0]).atan2(-forward[2]);
    let (sy, cy) = yaw.sin_cos();
    let p = head.position;
    Panel {
        center: [p.x - sy * d, p.y - 0.42 * d, p.z - cy * d],
        yaw,
        tilt: -0.45,
        size: [
            1.2 * d,
            1.2 * d * controls::HEIGHT as f32 / controls::WIDTH as f32,
        ],
        pixels: [controls::WIDTH, controls::HEIGHT],
    }
}

/// The dialog panel: above the control bar, in the same plane, as wide.
fn dialog_panel(bar: &Panel) -> Panel {
    let size = [
        bar.size[0],
        bar.size[0] * controls::DIALOG_HEIGHT as f32 / controls::DIALOG_WIDTH as f32,
    ];
    let [_, up, _] = bar.basis();
    let lift = bar.size[1] / 2.0 + bar.size[0] * 0.02 + size[1] / 2.0;
    Panel {
        center: [0, 1, 2].map(|i| bar.center[i] + up[i] * lift),
        size,
        pixels: [controls::DIALOG_WIDTH, controls::DIALOG_HEIGHT],
        ..*bar
    }
}

/// A white-ringed blue dot with premultiplied alpha.
fn cursor_image() -> Vec<u8> {
    let mut pixels = vec![0u8; (CURSOR_PX * CURSOR_PX * 4) as usize];
    let c = CURSOR_PX as f32 / 2.0;
    for y in 0..CURSOR_PX {
        for x in 0..CURSOR_PX {
            let d = ((x as f32 + 0.5 - c).powi(2) + (y as f32 + 0.5 - c).powi(2)).sqrt();
            let outer = (c - 2.0 - d).clamp(0.0, 1.0);
            let inner = (c - 10.0 - d).clamp(0.0, 1.0);
            let rgb = [
                255.0 * (1.0 - inner) + 79.0 * inner,
                255.0 * (1.0 - inner) + 140.0 * inner,
                255.0,
            ];
            let i = ((y * CURSOR_PX + x) * 4) as usize;
            for (k, channel) in rgb.iter().enumerate() {
                pixels[i + k] = (channel * outer) as u8;
            }
            pixels[i + 3] = (255.0 * outer) as u8;
        }
    }
    pixels
}

fn quad_layer<'a>(
    space: &'a xr::Space,
    target: &'a QuadTarget,
    pose: xr::Posef,
    size: [f32; 2],
    blend: bool,
) -> xr::CompositionLayerQuad<'a, xr::Vulkan> {
    let layer = xr::CompositionLayerQuad::new()
        .space(space)
        .eye_visibility(xr::EyeVisibility::BOTH)
        .sub_image(
            xr::SwapchainSubImage::new()
                .swapchain(&target.swapchain)
                .image_array_index(0)
                .image_rect(xr::Rect2Di {
                    offset: xr::Offset2Di { x: 0, y: 0 },
                    extent: xr::Extent2Di {
                        width: target.width as i32,
                        height: target.height as i32,
                    },
                }),
        )
        .pose(pose)
        .size(xr::Extent2Df {
            width: size[0],
            height: size[1],
        });
    if blend {
        layer.layer_flags(xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA)
    } else {
        layer
    }
}

enum Mode {
    Browser,
    Playing(Box<Playback>),
}

/// Trigger/A held on the video: a quick press toggles the menu, moving
/// while pressed drags the screen.
/// A select press on the browser panel: a click on release, unless it
/// turned into a drag (scrolling the list or its scrollbar).
struct BrowserPress {
    hand: usize,
    hit: browser::Hit,
    start_y: f32,
    start_scroll: f32,
    since: i64,
    dragging: bool,
    long_pressed: bool,
}

/// Holding select this long on a row selects it for editing.
const LONG_PRESS_NS: i64 = 600_000_000;

/// Canvas pixels the pointer must move before a press on the list scrolls it.
const LIST_DRAG_PX: f32 = 24.0;

/// A select press on the control bar, acted on at release (or long press).
struct ControlPress {
    hand: usize,
    hit: controls::Hit,
    since: i64,
    long: bool,
}

/// Remembers the format chosen for a file.
fn save_layout(
    key: Option<&String>,
    layout: &crate::vr::Layout,
    image: &crate::config::ImageAdjust,
) {
    let Some(key) = key else { return };
    let saved = crate::config::LayoutOverride {
        projection: layout.projection,
        stereo: layout.stereo,
        swap_eyes: layout.swap_eyes,
        image: *image,
    };
    if let Err(e) = crate::config::save_layout_override(key, Some(saved)) {
        eprintln!("Can't save the format: {e:#}");
    }
}

/// Remembers where the playing file was left (off the frame loop: the
/// write may be slow).
fn save_resume(key: Option<&String>, playback: &Playback) {
    let Some(key) = key.cloned() else { return };
    let point = crate::config::resume_point(playback.position(), playback.duration);
    let _ = std::thread::Builder::new()
        .name("save resume".into())
        .spawn(move || {
            if let Err(e) = crate::config::save_resume_position(&key, point) {
                eprintln!("Can't save where the video was left: {e:#}");
            }
        });
}

/// How often the place in the playing video is saved (for a crash or power-off).
const RESUME_SAVE_INTERVAL: Duration = Duration::from_secs(15);

/// Resuming starts this much before where the video was left.
const RESUME_REWIND: f64 = 3.0;

struct Press {
    hand: usize,
    since: i64,
    yaw: f32,
    pitch: f32,
    start: Placement,
    dragging: bool,
}

/// Aim change (radians) that turns a press into a drag.
const DRAG_THRESHOLD: f32 = 0.035;
/// Longest press (ns) that still counts as a click.
const CLICK_NS: i64 = 400_000_000;

/// Where `ray` meets the video (flat or curved screen, or a point 5 m out
/// for spherical video), for showing the cursor on it.
/// Flat screen size (metres) for a `width`-wide screen showing a video of
/// `video` pixels in `stereo` layout.
fn screen_size(
    video: (u32, u32),
    stereo: crate::vr::Stereo,
    width: f32,
    quarter_turns: u8,
) -> [f32; 2] {
    let (w, h) = video;
    let mut aspect = match stereo {
        crate::vr::Stereo::SideBySide => w as f32 / 2.0 / h.max(1) as f32,
        crate::vr::Stereo::TopBottom => w as f32 / (h as f32 / 2.0).max(1.0),
        crate::vr::Stereo::Mono => w as f32 / h.max(1) as f32,
    };
    if quarter_turns % 2 == 1 {
        aspect = 1.0 / aspect.max(0.01);
    }
    [width, width / aspect.max(0.1)]
}

/// Where subtitles go: over the bottom of a flat screen, just in front of it;
/// for VR180/360, 2 m ahead in the view's direction, below its centre.
fn caption_panel(
    placement: &Placement,
    projection: Projection,
    screen: [f32; 2],
    settings: crate::config::CaptionSettings,
) -> Panel {
    let flat = projection == Projection::Flat;
    // The layer grows with the chosen size; its bottom edge stays put.
    let quad_w = settings.scale
        * if flat {
            screen[0] * captions::SCREEN_SHARE
        } else {
            1.5
        };
    let quad_h = quad_w * captions::HEIGHT as f32 / captions::WIDTH as f32;
    let (y, depth) = if flat {
        // A curved screen bends towards us: stay in front of its nearest part.
        let d = placement.distance;
        let depth = if placement.curved {
            (d * d - quad_w * quad_w / 4.0).max(0.01).sqrt()
        } else {
            d
        };
        let bottom = -screen[1] / 2.0 + screen[1] * (0.04 + settings.raise);
        (bottom + quad_h / 2.0, depth - 0.02)
    } else {
        (-0.45 + 0.9 * settings.raise + quad_h / 2.0 - 0.14, 2.0)
    };
    let mut panel = Panel {
        center: [0.0; 3],
        yaw: placement.yaw,
        tilt: placement.pitch,
        size: [quad_w, quad_h],
        pixels: [captions::WIDTH, captions::HEIGHT],
    };
    let [_, up, normal] = panel.basis();
    panel.center = [0, 1, 2].map(|i| up[i] * y - normal[i] * depth);
    panel
}

/// Sample text shown while editing subtitle size and position.
const SAMPLE_CAPTION: &str = "This is how subtitles will look.\nLong lines wrap onto a second one.";

/// A 4×4 texture of one colour (premultiplied alpha), stretched into lines.
fn solid_image(rgb: [u8; 3], alpha: f32) -> Vec<u8> {
    let px = [
        (rgb[0] as f32 * alpha) as u8,
        (rgb[1] as f32 * alpha) as u8,
        (rgb[2] as f32 * alpha) as u8,
        (255.0 * alpha) as u8,
    ];
    px.repeat(16)
}

fn quat_mul(a: xr::Quaternionf, b: xr::Quaternionf) -> xr::Quaternionf {
    xr::Quaternionf {
        w: a.w * b.w - a.x * b.x - a.y * b.y - a.z * b.z,
        x: a.w * b.x + a.x * b.w + a.y * b.z - a.z * b.y,
        y: a.w * b.y - a.x * b.z + a.y * b.w + a.z * b.x,
        z: a.w * b.z + a.x * b.y - a.y * b.x + a.z * b.w,
    }
}

/// A line of the outline: its pose and size (metres).
type Line = (xr::Posef, [f32; 2]);

/// Lines `t` thick along the edges of a flat panel.
fn panel_outline(panel: &Panel, t: f32) -> Vec<Line> {
    let [right, up, _] = panel.basis();
    let [w, h] = panel.size;
    let at = |dx: f32, dy: f32| xr::Posef {
        orientation: panel.orientation(),
        position: {
            let p = add(add(panel.center, scale(right, dx)), scale(up, dy));
            xr::Vector3f {
                x: p[0],
                y: p[1],
                z: p[2],
            }
        },
    };
    vec![
        (at(0.0, h / 2.0), [w + t, t]),
        (at(0.0, -h / 2.0), [w + t, t]),
        (at(-w / 2.0, 0.0), [t, h + t]),
        (at(w / 2.0, 0.0), [t, h + t]),
    ]
}

/// Lines along the edges of the flat screen (curved: along its arcs).
fn screen_outline(placement: &Placement, screen: [f32; 2], t: f32) -> Vec<Line> {
    let base = Panel {
        center: [0.0; 3],
        yaw: placement.yaw,
        tilt: placement.pitch,
        size: screen,
        pixels: [1, 1],
    };
    let [right, up, normal] = base.basis();
    let r = placement.distance;
    if !placement.curved {
        let panel = Panel {
            center: scale(normal, -r),
            ..base
        };
        return panel_outline(&panel, t);
    }
    // A point on the curved screen `angle` to the right, `y` up, facing the viewer.
    let at = |angle: f32, y: f32| -> xr::Posef {
        let p = add(
            add(scale(right, r * angle.sin()), scale(up, y)),
            scale(normal, -r * angle.cos()),
        );
        let (s, c) = (-angle / 2.0).sin_cos();
        let turn = xr::Quaternionf {
            x: 0.0,
            y: s,
            z: 0.0,
            w: c,
        };
        xr::Posef {
            orientation: quat_mul(base.orientation(), turn),
            position: xr::Vector3f {
                x: p[0],
                y: p[1],
                z: p[2],
            },
        }
    };
    let [w, h] = screen;
    let half = w / 2.0 / r;
    let mut lines = vec![(at(-half, 0.0), [t, h + t]), (at(half, 0.0), [t, h + t])];
    const SEGMENTS: usize = 24;
    let step = 2.0 * half / SEGMENTS as f32;
    for i in 0..SEGMENTS {
        let angle = -half + (i as f32 + 0.5) * step;
        // A little longer than the chord, so the segments join up.
        let len = 2.0 * r * (step / 2.0).sin() + t / 2.0;
        for y in [h / 2.0, -h / 2.0] {
            lines.push((at(angle, y), [len, t]));
        }
    }
    lines
}

fn video_hit(
    ray: &Ray,
    placement: &Placement,
    projection: Projection,
    screen: [f32; 2],
) -> Option<[f32; 3]> {
    let r = placement.rotation();
    // Into the placement's frame (Rᵀ·v).
    let local = |v: [f32; 3]| [0, 1, 2].map(|i| (0..3).map(|k| r[i][k] * v[k]).sum::<f32>());
    let world = |v: [f32; 3]| [0, 1, 2].map(|i| (0..3).map(|k| r[k][i] * v[k]).sum::<f32>());
    let (o, d) = (local(ray.origin), local(ray.direction));
    let (w, h) = (screen[0], screen[1]);
    let p = match projection {
        Projection::Flat if placement.curved => {
            let radius = placement.distance;
            let a = d[0] * d[0] + d[2] * d[2];
            let b = 2.0 * (o[0] * d[0] + o[2] * d[2]);
            let c = o[0] * o[0] + o[2] * o[2] - radius * radius;
            let disc = b * b - 4.0 * a * c;
            if a < 1e-6 || disc < 0.0 {
                return None;
            }
            let t = (-b + disc.sqrt()) / (2.0 * a);
            let p = add(o, scale(d, t));
            let arc = p[0].atan2(-p[2]) * radius;
            (p[2] < 0.0 && arc.abs() <= w / 2.0 && p[1].abs() <= h / 2.0).then_some(p)?
        }
        Projection::Flat => {
            if d[2] > -1e-4 {
                return None;
            }
            let t = (-placement.distance - o[2]) / d[2];
            let p = add(o, scale(d, t));
            (p[0].abs() <= w / 2.0 && p[1].abs() <= h / 2.0).then_some(p)?
        }
        _ => add(o, scale(d, 5.0)),
    };
    Some(world(p))
}

/// Drops something that may block (decoder, network reader, audio) on a
/// background thread, so it can never freeze the frame loop.
pub fn drop_in_background<T: Send + 'static>(value: T) {
    let _ = std::thread::Builder::new()
        .name("teardown".into())
        .spawn(move || drop(value));
}

/// Frame-loop phases reported by the watchdog when the loop stalls.
const PHASES: [&str; 8] = [
    "events",
    "wait frame",
    "input",
    "stop playback",
    "browser",
    "playback",
    "render",
    "submit",
];

/// Logs where the frame loop is when it has not finished a frame for 3 s.
fn spawn_watchdog(heartbeat: Arc<AtomicI64>, phase: Arc<AtomicU8>, quit: Arc<AtomicBool>) {
    let started = Instant::now();
    let _ = std::thread::Builder::new()
        .name("watchdog".into())
        .spawn(move || {
            let mut reported = 0;
            while !quit.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                let stalled =
                    started.elapsed().as_millis() as i64 - heartbeat.load(Ordering::Relaxed);
                if stalled > 3000 && stalled / 5000 != reported {
                    reported = stalled / 5000;
                    let p = PHASES
                        .get(phase.load(Ordering::Relaxed) as usize)
                        .unwrap_or(&"?");
                    eprintln!(
                        "Watchdog: frame loop stalled for {:.1} s in phase \"{p}\"",
                        stalled as f64 / 1000.0
                    );
                } else if stalled <= 3000 {
                    reported = 0;
                }
            }
        });
}

fn aim_angles(ray: &Ray) -> (f32, f32) {
    let d = ray.direction;
    ((-d[0]).atan2(-d[2]), d[1].clamp(-1.0, 1.0).asin())
}

fn wrap_angle(a: f32) -> f32 {
    (a + std::f32::consts::PI).rem_euclid(std::f32::consts::TAU) - std::f32::consts::PI
}

/// Runs the app. With a navigator the browser is shown; `initial` starts
/// playing immediately (and the app exits when it ends if there is no browser).
pub fn run(
    mut navigator: Option<Navigator>,
    initial: Option<Playback>,
    options: AppOptions,
) -> anyhow::Result<PlayStats> {
    let mut ctx = XrContext::new()?;
    let mut renderer = Renderer::new(&ctx)?;
    let mut input = Input::new(&ctx)?;
    let heartbeat = Arc::new(AtomicI64::new(0));
    let phase = Arc::new(AtomicU8::new(0));
    let loop_started = Instant::now();
    spawn_watchdog(heartbeat.clone(), phase.clone(), options.quit.clone());
    let set_phase = |p: u8| phase.store(p, Ordering::Relaxed);
    let space = ctx
        .session
        .create_reference_space(xr::ReferenceSpaceType::LOCAL, xr::Posef::IDENTITY)
        .context("Create LOCAL reference space")?;
    let mut fonts = Fonts::load()?;
    let mut browser_target = match navigator {
        Some(_) => Some(renderer.create_quad(&ctx, browser::WIDTH, browser::HEIGHT)?),
        None => None,
    };
    let mut controls_target = renderer.create_quad(&ctx, controls::WIDTH, controls::HEIGHT)?;
    let mut captions_target = renderer.create_quad(&ctx, captions::WIDTH, captions::HEIGHT)?;
    // Outlines of the screen and the subtitle area while editing subtitles.
    let mut screen_line = renderer.create_quad(&ctx, 4, 4)?;
    renderer.upload_quad(&mut screen_line, &solid_image([0xff, 0xff, 0xff], 0.7))?;
    let mut caption_line = renderer.create_quad(&ctx, 4, 4)?;
    renderer.upload_quad(&mut caption_line, &solid_image([0xff, 0xcc, 0x00], 0.9))?;
    let mut outlines: Vec<(bool, Line)> = Vec::new();
    let mut dialog_target =
        renderer.create_quad(&ctx, controls::DIALOG_WIDTH, controls::DIALOG_HEIGHT)?;
    // The caption on screen, and where it goes this frame.
    let mut caption_drawn: Option<crate::subtitles::Caption> = None;
    let mut caption_at: Option<(xr::Posef, [f32; 2])> = None;
    // Editing subtitle size and position (from the track dialog).
    let mut caption_edit = false;
    let mut cursor = renderer.create_quad(&ctx, CURSOR_PX, CURSOR_PX)?;
    renderer.upload_quad(&mut cursor, &cursor_image())?;

    let mut mode = match initial {
        Some(playback) => Mode::Playing(Box::new(playback)),
        None => Mode::Browser,
    };
    let mut stats = PlayStats::default();
    let mut events = xr::EventDataBuffer::new();
    let mut running = false;
    let mut exit_requested = false;
    let mut active_hand = 1usize;
    let mut hovered: Option<browser::Hit> = None;
    // Where the playing file's chosen format is saved.
    let mut playing_key: Option<String> = None;
    let mut resume_saved_at = Instant::now();
    let mut screenshot = options.play.screenshot.clone();
    let mut placement = Placement::new(&options.view);
    let mut controls_panel_at: Option<Panel> = None;
    let mut controls_drawn: Option<(controls::State, controls::Hit)> = None;
    let mut press: Option<Press> = None;
    let mut browser_press: Option<BrowserPress> = None;
    // The control bar shows the page with every format.
    // The dialog open above the control bar, and what it last showed.
    let mut dialog: Option<controls::Dialog> = None;
    let mut dialog_drawn: Option<(controls::State, controls::Hit)> = None;
    // Subtitle size and position (the same for every video).
    let mut caption_settings = crate::config::caption_settings();
    // Jump lengths, volume etc.; reloaded when the Settings screen changes them.
    let mut prefs = crate::config::preferences();
    // Volume changed with the D-pad and not yet saved (saved once it settles).
    let mut volume_changed_at: Option<Instant> = None;
    let mut list_page = 0usize;
    let mut control_press: Option<ControlPress> = None;
    // Formats the format button steps through.
    let mut favourites: Vec<controls::Format> = crate::config::favourite_formats()
        .into_iter()
        .map(|f| (f.projection, f.stereo))
        .collect();
    // Previous (-1) or next (+1) video requested from the control bar.
    let mut switch_video: Option<isize> = None;

    'main: loop {
        heartbeat.store(loop_started.elapsed().as_millis() as i64, Ordering::Relaxed);
        set_phase(0);
        if options.quit.load(Ordering::Relaxed) && !exit_requested {
            exit_requested = true;
            if running {
                ctx.session.request_exit()?;
            } else {
                break;
            }
        }
        while let Some(event) = ctx.xr.poll_event(&mut events)? {
            match event {
                xr::Event::SessionStateChanged(e) => {
                    eprintln!("OpenXR session: {:?}", e.state());
                    match e.state() {
                        xr::SessionState::READY => {
                            ctx.session.begin(VIEW_TYPE)?;
                            running = true;
                        }
                        xr::SessionState::STOPPING => {
                            ctx.session.end()?;
                            running = false;
                        }
                        xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => break 'main,
                        _ => {}
                    }
                }
                xr::Event::InstanceLossPending(_) => break 'main,
                xr::Event::InteractionProfileChanged(_) => input.log_profiles(&ctx),
                _ => {}
            }
        }
        if !running {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        set_phase(1);
        let state = ctx.frame_waiter.wait()?;
        ctx.frame_stream.begin()?;
        stats.xr_frames += 1;
        if !state.should_render {
            ctx.frame_stream
                .end(state.predicted_display_time, ctx.blend_mode, &[])?;
            continue;
        }
        stats.rendered_xr_frames += 1;
        let now = state.predicted_display_time.as_nanos();
        let dt = state.predicted_display_period.as_nanos() as f32 / 1e9;
        set_phase(2);
        let mut buttons = input
            .poll(&ctx, &space, state.predicted_display_time)
            .unwrap_or_else(|e| {
                eprintln!("Input: {e:#}");
                InputState::default()
            });
        for (hand, pressed) in buttons.select.iter().enumerate() {
            if *pressed {
                active_hand = hand;
            }
        }
        let ray = buttons.rays[active_hand]
            .as_ref()
            .or(buttons.rays[1 - active_hand].as_ref());
        let (_, views) =
            ctx.session
                .locate_views(VIEW_TYPE, state.predicted_display_time, &space)?;

        // B while editing subtitles: back to the track dialog.
        if caption_edit && buttons.back {
            caption_edit = false;
            dialog = Some(controls::Dialog::Tracks);
            buttons.back = false;
            controls_drawn = None;
        }
        // Leaving playback: B, end of video, or --duration reached.
        let mut stop_playback = switch_video.is_some();
        if let Mode::Playing(playback) = &mut mode {
            let reached_end = options
                .play
                .duration
                .is_some_and(|d| playback.media_time(now).is_some_and(|t| t >= d));
            stop_playback |= buttons.back || playback.finished(now) || reached_end;
        }
        if stop_playback {
            set_phase(3);
            if volume_changed_at.take().is_some() {
                save_volume(prefs.volume);
            }
            if let Mode::Playing(playback) = std::mem::replace(&mut mode, Mode::Browser) {
                save_resume(playing_key.as_ref(), &playback);
                stats.displayed_frames += playback.stats.displayed_frames;
                stats.uploaded_frames += playback.stats.uploaded_frames;
                stats.skipped_frames += playback.stats.skipped_frames;
                stats.media_seconds = playback.stats.media_seconds;
                drop_in_background(playback);
            }
            // This B press ended playback; don't also go up a folder.
            buttons.back = false;
            controls_panel_at = None;
            press = None;
            match navigator.as_mut() {
                // Previous/next: the browser shows "Opening …" until it plays.
                Some(nav) if let Some(delta) = switch_video.take() => {
                    nav.open_adjacent(delta);
                    nav.redraw();
                }
                Some(nav) => nav.playback_ended(),
                None => options.quit.store(true, Ordering::Relaxed),
            }
        }

        let mut cursor_at: Option<(Panel, f32, f32)> = None;
        // A cursor on the video itself: position, aim yaw/pitch, distance from the eye.
        let mut video_cursor: Option<([f32; 3], f32, f32, f32)> = None;
        // Every frame submits a projection layer (the video, or a dark backdrop
        // in the browser): SteamVR only treats an app showing one as running,
        // and otherwise leaves its own menu in front ("start in background").
        let eyes_rendered;
        match &mut mode {
            Mode::Browser => {
                set_phase(4);
                let (Some(nav), Some(target)) = (navigator.as_mut(), browser_target.as_mut())
                else {
                    ctx.frame_stream
                        .end(state.predicted_display_time, ctx.blend_mode, &[])?;
                    continue;
                };
                if let Some(changed) = nav.take_preferences() {
                    prefs = changed;
                }
                if let Some(opened) = nav.poll() {
                    playing_key = Some(opened.key.clone());
                    eprintln!(
                        "Playing {} as {:?} / {:?}",
                        opened.name, opened.layout.projection, opened.layout.stereo
                    );
                    let start = opened
                        .resume
                        .filter(|_| prefs.resume)
                        .map_or(0.0, |t| (t - RESUME_REWIND).max(0.0));
                    let mut playback = Playback::start(
                        opened.decoder,
                        opened.layout,
                        start,
                        // 100 % by default: the headset's buttons set loudness.
                        prefs.volume as f32 / 100.0,
                    );
                    if start > 0.0 {
                        playback.notice(
                            format!("Continuing from {}", controls::format_time(start)),
                            Duration::from_secs(4),
                        );
                    } else if prefs.volume != 100 {
                        // A level set earlier (perhaps 0 %) shouldn't look like a fault.
                        playback.notice(format!("Volume {} %", prefs.volume), NOTICE * 2);
                    }
                    resume_saved_at = Instant::now();
                    playback.add_external_subtitles(opened.external_subtitles);
                    playback.image = opened.image;
                    renderer.set_adjust(&playback.image);
                    dialog = None;
                    caption_edit = false;
                    mode = Mode::Playing(Box::new(playback));
                    ctx.frame_stream
                        .end(state.predicted_display_time, ctx.blend_mode, &[])?;
                    continue;
                }
                let point = ray.and_then(|r| BROWSER_PANEL.hit(r));
                let hit = point.map_or(browser::Hit::Nothing, |(x, y)| {
                    browser::hit(nav.view(), &mut fonts, x, y)
                });
                if buttons.select[active_hand]
                    && browser_press.is_none()
                    && let Some((_, y)) = point
                {
                    browser_press = Some(BrowserPress {
                        hand: active_hand,
                        hit,
                        start_y: y,
                        start_scroll: nav.scroll(),
                        since: now,
                        dragging: false,
                        long_pressed: false,
                    });
                }
                if let Some(p) = &mut browser_press {
                    if buttons.select_held[p.hand] {
                        if let Some((_, y)) = point {
                            let on_list = matches!(
                                p.hit,
                                browser::Hit::Row(_)
                                    | browser::Hit::Lock(_)
                                    | browser::Hit::RowAction(..)
                            );
                            if p.hit == browser::Hit::ScrollBar {
                                nav.set_scroll(browser::scroll_at(nav.view(), y));
                            } else if on_list
                                && !p.long_pressed
                                && (p.dragging || (y - p.start_y).abs() > LIST_DRAG_PX)
                            {
                                // Drag the list like a touch screen.
                                p.dragging = true;
                                nav.set_scroll(
                                    p.start_scroll - browser::rows_for_drag(y - p.start_y),
                                );
                            }
                        }
                        if let browser::Hit::Row(i) = p.hit
                            && !p.dragging
                            && !p.long_pressed
                            && now - p.since > LONG_PRESS_NS
                        {
                            p.long_pressed = true;
                            nav.long_press(i);
                        }
                    } else {
                        if !p.dragging && !p.long_pressed {
                            nav.click(p.hit);
                        }
                        browser_press = None;
                    }
                }
                if buttons.back {
                    nav.back();
                }
                if buttons.scroll.abs() > 0.2 {
                    // Holding the grip scrolls four times faster.
                    let speed = if buttons.grip { 48.0 } else { 12.0 };
                    nav.scroll_by(-buttons.scroll * speed * dt);
                }
                if nav.take_dirty() || hovered != Some(hit) {
                    hovered = Some(hit);
                    let hover_point = match hit {
                        browser::Hit::Nothing => None,
                        _ => point,
                    };
                    let canvas = browser::render(nav.view(), &mut fonts, hover_point, false);
                    renderer.upload_quad(target, &canvas.pixels)?;
                }
                if let Some((x, y)) = point {
                    cursor_at = Some((BROWSER_PANEL, x, y));
                }
                set_phase(6);
                renderer.begin_frame(None)?;
                for (eye, _) in views.iter().enumerate() {
                    let target = &mut renderer.eyes[eye];
                    let index = target.swapchain.acquire_image()?;
                    target.swapchain.wait_image(xr::Duration::INFINITE)?;
                    renderer.draw_eye(eye, index, &Default::default(), false);
                }
                renderer.end_frame()?;
                for eye in renderer.eyes.iter_mut() {
                    eye.swapchain.release_image()?;
                }
                eyes_rendered = true;
            }
            Mode::Playing(playback) => {
                set_phase(5);
                if resume_saved_at.elapsed() >= RESUME_SAVE_INTERVAL {
                    resume_saved_at = Instant::now();
                    save_resume(playing_key.as_ref(), playback);
                }
                let curved_applies = playback.layout.projection == Projection::Flat;
                let ui_state = controls::State {
                    paused: playback.paused(),
                    position: playback.position().floor(),
                    duration: playback.duration,
                    has_previous: navigator.as_ref().is_some_and(|n| n.has_adjacent(-1)),
                    has_next: navigator.as_ref().is_some_and(|n| n.has_adjacent(1)),
                    curved: curved_applies.then_some(placement.curved),
                    format: (playback.layout.projection, playback.layout.stereo),
                    favourites: favourites.clone(),
                    swap_eyes: playback.layout.swap_eyes,
                    subtitle_tracks: playback.subtitle_labels(),
                    subtitle: playback.subtitle_index(),
                    audio_tracks: playback.audio_labels().to_vec(),
                    audio: playback.audio_index(),
                    list_page,
                    image: playback.image,
                    dialog,
                    caption_edit,
                };
                // The pointer on the dialog (above the bar) or on the bar.
                let dialog_at = controls_panel_at
                    .filter(|_| dialog.is_some())
                    .map(|bar| dialog_panel(&bar));
                let dialog_point =
                    dialog_at.and_then(|p| ray.and_then(|r| p.hit(r)).map(|pt| (p, pt)));
                let bar_point =
                    controls_panel_at.and_then(|p| ray.and_then(|r| p.hit(r)).map(|pt| (p, pt)));
                let control_point = dialog_point.or(bar_point);
                let dialog_hit = dialog_point.map_or(controls::Hit::Nothing, |(_, (x, y))| {
                    controls::dialog_hit(&ui_state, x, y)
                });
                let bar_hit = match (dialog_point, bar_point) {
                    (None, Some((_, (x, y)))) => controls::hit(&ui_state, x, y),
                    _ => controls::Hit::Nothing,
                };
                let control_hit = if dialog_point.is_some() {
                    dialog_hit
                } else {
                    bar_hit
                };

                // Controls act on release, so buttons with a long press can
                // tell the two apart; the seek bar acts at once.
                let mut clicked: Option<controls::Hit> = None;
                let mut long_pressed: Option<controls::Hit> = None;
                if let Some(cp) = &mut control_press {
                    if buttons.select_held[cp.hand] {
                        if !cp.long && cp.hit.has_long_press() && now - cp.since > LONG_PRESS_NS {
                            cp.long = true;
                            long_pressed = Some(cp.hit);
                        }
                    } else {
                        if !cp.long {
                            clicked = Some(cp.hit);
                        }
                        control_press = None;
                    }
                }
                if buttons.select[active_hand] && press.is_none() && control_press.is_none() {
                    if control_point.is_some() {
                        if let controls::Hit::Seek(_) = control_hit {
                            clicked = Some(control_hit);
                        } else {
                            control_press = Some(ControlPress {
                                hand: active_hand,
                                hit: control_hit,
                                since: now,
                                long: false,
                            });
                        }
                    } else if let Some(r) = buttons.rays[active_hand].as_ref() {
                        let (yaw, pitch) = aim_angles(r);
                        press = Some(Press {
                            hand: active_hand,
                            since: now,
                            yaw,
                            pitch,
                            start: placement,
                            dragging: false,
                        });
                    }
                }
                if let Some(p) = &mut press {
                    match (buttons.select_held[p.hand], buttons.rays[p.hand].as_ref()) {
                        (true, Some(r)) => {
                            let (yaw, pitch) = aim_angles(r);
                            let (dyaw, dpitch) = (wrap_angle(yaw - p.yaw), pitch - p.pitch);
                            if !p.dragging
                                && (dyaw.abs() > DRAG_THRESHOLD || dpitch.abs() > DRAG_THRESHOLD)
                            {
                                // Start from here, so the screen doesn't jump by the threshold.
                                p.dragging = true;
                                (p.yaw, p.pitch, p.start) = (yaw, pitch, placement);
                            } else if p.dragging {
                                placement.yaw = p.start.yaw + dyaw;
                                placement.pitch = (p.start.pitch + dpitch).clamp(-1.3, 1.3);
                                // The stick pushes the screen away or pulls it closer.
                                if buttons.scroll.abs() > 0.2 {
                                    placement.distance = (placement.distance
                                        + buttons.scroll * 2.0 * dt)
                                        .clamp(0.8, 12.0);
                                }
                            }
                        }
                        _ => {
                            // Released: a short press without dragging toggles the menu.
                            if !p.dragging && now - p.since < CLICK_NS {
                                controls_panel_at = match controls_panel_at {
                                    Some(_) => None,
                                    None => {
                                        // In front of a flat screen; VR180/360 surround us.
                                        let limit =
                                            if playback.layout.projection == Projection::Flat {
                                                placement.distance - 0.4
                                            } else {
                                                f32::INFINITY
                                            };
                                        views.first().map(|v| controls_panel(&v.pose, limit))
                                    }
                                };
                                dialog = None;
                                caption_edit = false;
                                controls_drawn = None;
                            }
                            press = None;
                        }
                    }
                }
                let set_format = |playback: &mut Playback, format: controls::Format| {
                    (playback.layout.projection, playback.layout.stereo) = format;
                    save_layout(playing_key.as_ref(), &playback.layout, &playback.image);
                };
                let mut image_changed = false;
                let mut captions_changed = false;
                match clicked {
                    Some(controls::Hit::PlayPause) => playback.toggle_pause(now),
                    Some(controls::Hit::Seek(f)) => playback.seek(f as f64 * playback.duration),
                    Some(controls::Hit::Previous) => switch_video = Some(-1),
                    Some(controls::Hit::Next) => switch_video = Some(1),
                    // CC: subtitles on/off (or, with none, straight to audio tracks).
                    Some(controls::Hit::Captions) => {
                        if playback.subtitle_labels().is_empty() {
                            dialog = Some(controls::Dialog::Tracks);
                        } else {
                            playback.toggle_subtitles();
                        }
                    }
                    Some(controls::Hit::Screen) => {
                        let current = (playback.layout.projection, playback.layout.stereo);
                        if let Some(next) = controls::next_favourite(current, &favourites) {
                            set_format(playback, next);
                        }
                    }
                    Some(controls::Hit::Image) => dialog = Some(controls::Dialog::Image),
                    Some(controls::Hit::AudioTrack(track)) => playback.set_audio_track(track),
                    Some(controls::Hit::SubtitleTrack(track)) => playback.set_subtitle(track),
                    Some(controls::Hit::MorePage) => {
                        list_page =
                            (list_page + 1) % controls::pages(playback.subtitle_labels().len() + 1);
                    }
                    Some(controls::Hit::CaptionSize(d)) => {
                        caption_settings.scale =
                            (caption_settings.scale * 1.12f32.powi(d as i32)).clamp(0.5, 2.5);
                        captions_changed = true;
                    }
                    Some(controls::Hit::CaptionMove(d)) => {
                        caption_settings.raise =
                            (caption_settings.raise + 0.03 * d as f32).clamp(-0.3, 0.8);
                        captions_changed = true;
                    }
                    Some(controls::Hit::CaptionEdit) => {
                        caption_edit = true;
                        dialog = None;
                    }
                    Some(controls::Hit::CaptionReset) => {
                        caption_settings = Default::default();
                        captions_changed = true;
                    }
                    Some(controls::Hit::CaptionDone) => {
                        caption_edit = false;
                        dialog = Some(controls::Dialog::Tracks);
                    }
                    Some(controls::Hit::Pick(i)) => set_format(playback, controls::FORMATS[i]),
                    Some(controls::Hit::Curved) => placement.curved = !placement.curved,
                    Some(controls::Hit::SwapEyes) => {
                        playback.layout.swap_eyes = !playback.layout.swap_eyes;
                        save_layout(playing_key.as_ref(), &playback.layout, &playback.image);
                    }
                    Some(controls::Hit::Brightness(d)) => {
                        playback.image.brightness = (playback.image.brightness
                            + controls::BRIGHTNESS_STEP * d as f32)
                            .clamp(-0.5, 0.5);
                        image_changed = true;
                    }
                    Some(controls::Hit::Contrast(d)) => {
                        playback.image.contrast = (playback.image.contrast
                            + controls::CONTRAST_STEP * d as f32)
                            .clamp(0.5, 2.0);
                        image_changed = true;
                    }
                    Some(controls::Hit::Saturation(d)) => {
                        playback.image.saturation = (playback.image.saturation
                            + controls::SATURATION_STEP * d as f32)
                            .clamp(0.0, 2.0);
                        image_changed = true;
                    }
                    Some(controls::Hit::Rotate(turns)) => {
                        playback.image.rotation = turns;
                        image_changed = true;
                    }
                    Some(controls::Hit::ResetImage) => {
                        playback.image = Default::default();
                        image_changed = true;
                    }
                    Some(controls::Hit::Close) => dialog = None,
                    Some(controls::Hit::Nothing) | None => {}
                }
                match long_pressed {
                    Some(controls::Hit::Captions) => {
                        dialog = Some(controls::Dialog::Tracks);
                        // Start on the page with the track shown.
                        list_page = playback.subtitle_index().map_or(0, |i| (i + 1) / 11);
                    }
                    Some(controls::Hit::Screen) => dialog = Some(controls::Dialog::Screen),
                    // Star or unstar a favourite.
                    Some(controls::Hit::Pick(i)) => {
                        let format = controls::FORMATS[i];
                        match favourites.iter().position(|f| *f == format) {
                            Some(at) if favourites.len() > 1 => {
                                favourites.remove(at);
                            }
                            Some(_) => {} // keep at least one
                            None => favourites.push(format),
                        }
                        let saved: Vec<crate::config::Format> = favourites
                            .iter()
                            .map(|&(projection, stereo)| crate::config::Format {
                                projection,
                                stereo,
                            })
                            .collect();
                        if let Err(e) = crate::config::save_favourite_formats(&saved) {
                            eprintln!("Can't save favourite formats: {e:#}");
                        }
                    }
                    _ => {}
                }
                if image_changed {
                    renderer.set_adjust(&playback.image);
                    save_layout(playing_key.as_ref(), &playback.layout, &playback.image);
                }
                if captions_changed {
                    caption_drawn = None;
                    if let Err(e) = crate::config::save_caption_settings(caption_settings) {
                        eprintln!("Can't save subtitle settings: {e:#}");
                    }
                }
                if clicked.is_some() || long_pressed.is_some() {
                    controls_drawn = None;
                    dialog_drawn = None;
                }
                let dragging = press.as_ref().is_some_and(|p| p.dragging);
                // Thumbstick click: back to the default size and place.
                // D-pad left/right (or a sideways stick flick): jump back/forward,
                // further with the grip held. Up/down: volume.
                if buttons.seek != 0 {
                    let jump = prefs.jump(buttons.seek, buttons.grip);
                    playback.seek(playback.position() + jump);
                    let text = jump_notice(jump, playback.position());
                    playback.notice(text, NOTICE);
                    controls_drawn = None;
                }
                if buttons.volume != 0 {
                    prefs.volume = prefs.stepped_volume(buttons.volume);
                    playback.set_volume(prefs.volume as f32 / 100.0);
                    let text = if playback.has_audio() {
                        format!("Volume {} %", prefs.volume)
                    } else {
                        format!("Volume {} % (this video has no sound)", prefs.volume)
                    };
                    playback.notice(text, NOTICE);
                    volume_changed_at = Some(Instant::now());
                }
                if volume_changed_at.is_some_and(|at| at.elapsed() > VOLUME_SAVE_DELAY) {
                    volume_changed_at = None;
                    save_volume(prefs.volume);
                }
                if buttons.reset {
                    placement = Placement {
                        curved: placement.curved,
                        ..Placement::new(&options.view)
                    };
                    press = None;
                }
                // The stick zooms (screen size / magnification), except while
                // dragging, when it moves the screen nearer or farther.
                if !dragging && buttons.scroll.abs() > 0.2 {
                    placement.zoom =
                        (placement.zoom * (buttons.scroll * 1.2 * dt).exp()).clamp(0.4, 3.0);
                }

                if controls_panel_at.is_some() {
                    if controls_drawn.as_ref() != Some(&(ui_state.clone(), bar_hit)) {
                        let canvas = controls::render(&ui_state, &mut fonts, bar_hit);
                        renderer.upload_quad(&mut controls_target, &canvas.pixels)?;
                        controls_drawn = Some((ui_state.clone(), bar_hit));
                    }
                    if dialog.is_some()
                        && dialog_drawn.as_ref() != Some(&(ui_state.clone(), dialog_hit))
                    {
                        let canvas = controls::render_dialog(&ui_state, &mut fonts, dialog_hit);
                        renderer.upload_quad(&mut dialog_target, &canvas.pixels)?;
                        dialog_drawn = Some((ui_state, dialog_hit));
                    }
                    if let Some((p, (x, y))) = control_point {
                        cursor_at = Some((p, x, y));
                    } else if let (Some(r), Some(head)) = (ray, views.first()) {
                        // Menu open: also show where the ray meets the video.
                        let screen = screen_size(
                            renderer.video_size(),
                            playback.layout.stereo,
                            options.view.screen_width * placement.zoom,
                            playback.image.rotation,
                        );
                        if let Some(point) =
                            video_hit(r, &placement, playback.layout.projection, screen)
                        {
                            let eye = head.pose.position;
                            let to_eye = [eye.x - point[0], eye.y - point[1], eye.z - point[2]];
                            let distance = dot(to_eye, to_eye).sqrt();
                            let (yaw, pitch) = aim_angles(r);
                            video_cursor = Some((point, yaw, pitch, distance));
                        }
                    }
                }

                // Subtitles: redrawn only when the text changes. While
                // editing, sample text shows their size and place.
                let caption = if caption_edit {
                    Some(crate::subtitles::Caption::text(SAMPLE_CAPTION))
                } else {
                    playback.caption()
                };
                if caption != caption_drawn {
                    if let Some(caption) = &caption {
                        let canvas = captions::render(caption, &mut fonts);
                        renderer.upload_quad(&mut captions_target, &canvas.pixels)?;
                    }
                    caption_drawn = caption.clone();
                }
                let screen = screen_size(
                    renderer.video_size(),
                    playback.layout.stereo,
                    options.view.screen_width * placement.zoom,
                    playback.image.rotation,
                );
                let flat = playback.layout.projection == Projection::Flat;
                let caption_area = caption_panel(
                    &placement,
                    playback.layout.projection,
                    screen,
                    caption_settings,
                );
                caption_at = caption
                    .is_some()
                    .then(|| (caption_area.pose(), caption_area.size));
                outlines.clear();
                if caption_edit {
                    // Thick enough to see at any distance.
                    let t = 0.004 * if flat { placement.distance } else { 2.0 };
                    if flat {
                        outlines.extend(
                            screen_outline(&placement, screen, t)
                                .into_iter()
                                .map(|l| (false, l)),
                        );
                    }
                    outlines.extend(
                        panel_outline(&caption_area, t)
                            .into_iter()
                            .map(|l| (true, l)),
                    );
                }
                set_phase(6);
                let upload = playback.advance(now);
                renderer.begin_frame(if upload { playback.current() } else { None })?;
                if upload {
                    playback.stats.uploaded_frames += 1;
                }
                let show = playback.current().is_some();
                if show {
                    playback.stats.displayed_frames += 1;
                }
                let tex = renderer.video_size();
                let mut indices = [0u32; 2];
                for (eye, view) in views.iter().enumerate() {
                    let target = &mut renderer.eyes[eye];
                    let index = target.swapchain.acquire_image()?;
                    target.swapchain.wait_image(xr::Duration::INFINITE)?;
                    indices[eye] = index;
                    let params = eye_params(
                        view,
                        eye,
                        &playback.layout,
                        tex,
                        &options.view,
                        &placement,
                        playback.image.rotation,
                    );
                    renderer.draw_eye(eye, index, &params, show);
                }
                let media_time = playback.media_time(now);
                let capture = match (&screenshot, media_time) {
                    (Some((_, at)), Some(t)) if t >= *at && show => screenshot.take(),
                    _ => None,
                };
                if capture.is_some() {
                    renderer.record_readback(0, indices[0])?;
                }
                renderer.end_frame()?;
                if let Some((path, _)) = capture {
                    renderer.take_screenshot(0, &path)?;
                    stats.screenshot = Some(path.display().to_string());
                }
                for eye in renderer.eyes.iter_mut() {
                    eye.swapchain.release_image()?;
                }
                eyes_rendered = true;
            }
        }

        // Quad layers to show this frame: (target, pose, size, blend).
        let mut quads: Vec<(&QuadTarget, xr::Posef, [f32; 2], bool)> = Vec::new();
        match &mode {
            Mode::Browser => {
                if let Some(target) = browser_target.as_ref().filter(|t| t.ready) {
                    quads.push((target, BROWSER_PANEL.pose(), BROWSER_PANEL.size, false));
                }
            }
            Mode::Playing(_) => {
                for (caption, (pose, size)) in &outlines {
                    let target = if *caption {
                        &caption_line
                    } else {
                        &screen_line
                    };
                    quads.push((target, *pose, *size, true));
                }
                if let Some((pose, size)) = caption_at.filter(|_| captions_target.ready) {
                    quads.push((&captions_target, pose, size, true));
                }
                if let Some(panel) = controls_panel_at.filter(|_| controls_target.ready) {
                    quads.push((&controls_target, panel.pose(), panel.size, false));
                    if dialog.is_some() && dialog_target.ready {
                        let above = dialog_panel(&panel);
                        quads.push((&dialog_target, above.pose(), above.size, false));
                    }
                }
            }
        }
        if let Some((panel, x, y)) = cursor_at {
            // Lifted a few millimetres so the cursor sits in front of the panel.
            let p = panel.point(x, y, 0.005);
            let pose = xr::Posef {
                orientation: panel.orientation(),
                position: xr::Vector3f {
                    x: p[0],
                    y: p[1],
                    z: p[2],
                },
            };
            quads.push((&cursor, pose, [CURSOR_SIZE, CURSOR_SIZE], true));
        }

        if let Some((p, yaw, pitch, distance)) = video_cursor {
            // Facing the viewer, and scaled to look the same size at any distance.
            let facing = Panel {
                center: p,
                yaw,
                tilt: pitch,
                size: [0.0; 2],
                pixels: [1, 1],
            };
            let pose = xr::Posef {
                orientation: facing.orientation(),
                position: xr::Vector3f {
                    x: p[0],
                    y: p[1],
                    z: p[2],
                },
            };
            let size = CURSOR_SIZE * distance;
            quads.push((&cursor, pose, [size, size], true));
        }
        set_phase(7);
        let projection_views: Vec<xr::CompositionLayerProjectionView<xr::Vulkan>> = if eyes_rendered
        {
            views
                .iter()
                .zip(&renderer.eyes)
                .map(|(view, target)| {
                    xr::CompositionLayerProjectionView::new()
                        .pose(view.pose)
                        .fov(view.fov)
                        .sub_image(
                            xr::SwapchainSubImage::new()
                                .swapchain(&target.swapchain)
                                .image_array_index(0)
                                .image_rect(xr::Rect2Di {
                                    offset: xr::Offset2Di { x: 0, y: 0 },
                                    extent: xr::Extent2Di {
                                        width: target.width as i32,
                                        height: target.height as i32,
                                    },
                                }),
                        )
                })
                .collect()
        } else {
            Vec::new()
        };
        let projection = xr::CompositionLayerProjection::new()
            .space(&space)
            .views(&projection_views);
        let quad_layers: Vec<xr::CompositionLayerQuad<xr::Vulkan>> = quads
            .iter()
            .map(|(target, pose, size, blend)| quad_layer(&space, target, *pose, *size, *blend))
            .collect();
        let mut layers: Vec<&xr::CompositionLayerBase<xr::Vulkan>> = Vec::new();
        if !projection_views.is_empty() {
            layers.push(&projection);
        }
        for quad in &quad_layers {
            layers.push(&**quad);
        }
        ctx.frame_stream
            .end(state.predicted_display_time, ctx.blend_mode, &layers)?;
    }
    options.quit.store(true, Ordering::Relaxed);
    if volume_changed_at.is_some() {
        save_volume(prefs.volume);
    }
    if let Mode::Playing(playback) = mode {
        // Written here, not in the background: the app is about to exit.
        let point = crate::config::resume_point(playback.position(), playback.duration);
        if let Some(key) = &playing_key
            && let Err(e) = crate::config::save_resume_position(key, point)
        {
            eprintln!("Can't save where the video was left: {e:#}");
        }
        stats.displayed_frames += playback.stats.displayed_frames;
        stats.uploaded_frames += playback.stats.uploaded_frames;
        stats.skipped_frames += playback.stats.skipped_frames;
        stats.media_seconds = playback.stats.media_seconds;
    }
    drop(renderer);
    Ok(stats)
}

/// How long a jump or volume notice stays up.
const NOTICE: Duration = Duration::from_millis(1500);
/// Volume is saved once the D-pad has left it alone this long.
const VOLUME_SAVE_DELAY: Duration = Duration::from_secs(1);

/// "+5 s · 12:34" after a jump of `jump` seconds to `position`.
fn jump_notice(jump: f64, position: f64) -> String {
    let sign = if jump < 0.0 { '−' } else { '+' };
    let length = crate::ui::settings::format_jump(jump.abs().round() as u32);
    format!("{sign}{length}  ·  {}", controls::format_time(position))
}

fn save_volume(volume: u32) {
    if let Err(e) = crate::config::update_preferences(|p| p.volume = volume) {
        eprintln!("Can't save the volume: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jump_notices() {
        assert_eq!(jump_notice(-5.0, 754.0), "−5 s  ·  12:34");
        assert_eq!(jump_notice(600.0, 3600.0), "+10 min  ·  1:00:00");
    }

    #[test]
    fn panel_hit_round_trips_through_point() {
        let panel = Panel {
            center: [0.3, -0.4, -1.0],
            yaw: 0.5,
            tilt: -0.45,
            size: [1.2, 1.2 * controls::HEIGHT as f32 / controls::WIDTH as f32],
            pixels: [1200, 260],
        };
        for (x, y) in [(600.0, 130.0), (100.0, 40.0), (1150.0, 250.0)] {
            let target = panel.point(x, y, 0.0);
            let len = dot(target, target).sqrt();
            let ray = Ray {
                origin: [0.0; 3],
                direction: scale(target, 1.0 / len),
            };
            let (hx, hy) = panel.hit(&ray).expect("hit");
            assert!(
                (hx - x).abs() < 2.0 && (hy - y).abs() < 2.0,
                "({x},{y}) -> ({hx},{hy})"
            );
        }
    }

    /// +Z (a quad's normal) turned by `q`.
    fn rotate_z(q: xr::Quaternionf) -> [f32; 3] {
        let v = [0.0f32, 0.0, 1.0];
        let t = [
            2.0 * (q.y * v[2] - q.z * v[1]),
            2.0 * (q.z * v[0] - q.x * v[2]),
            2.0 * (q.x * v[1] - q.y * v[0]),
        ];
        [
            v[0] + q.w * t[0] + (q.y * t[2] - q.z * t[1]),
            v[1] + q.w * t[1] + (q.z * t[0] - q.x * t[2]),
            v[2] + q.w * t[2] + (q.x * t[1] - q.y * t[0]),
        ]
    }

    #[test]
    fn curved_screen_outline_faces_the_viewer() {
        let placement = Placement {
            yaw: 0.4,
            pitch: 0.0,
            distance: 3.0,
            curved: true,
            zoom: 1.0,
        };
        let lines = screen_outline(&placement, [4.0, 2.25], 0.01);
        for (pose, _) in &lines {
            let p = [pose.position.x, pose.position.y, pose.position.z];
            let horizontal = (p[0] * p[0] + p[2] * p[2]).sqrt();
            assert!((horizontal - 3.0).abs() < 1e-3, "on the arc: {p:?}");
            // The normal points back at the viewer (the axis).
            let n = rotate_z(pose.orientation);
            let towards = [-p[0] / horizontal, 0.0, -p[2] / horizontal];
            assert!(dot(n, towards) > 0.999, "{n:?} vs {towards:?}");
        }
    }

    #[test]
    fn panel_orientation_matches_basis() {
        let panel = Panel {
            center: [0.0; 3],
            yaw: 0.7,
            tilt: -0.4,
            size: [1.0, 1.0],
            pixels: [1, 1],
        };
        let q = panel.orientation();
        // Rotate +Z (the quad's normal) by q and compare with the basis normal.
        let v = [0.0f32, 0.0, 1.0];
        let t = [
            2.0 * (q.y * v[2] - q.z * v[1]),
            2.0 * (q.z * v[0] - q.x * v[2]),
            2.0 * (q.x * v[1] - q.y * v[0]),
        ];
        let rotated = [
            v[0] + q.w * t[0] + (q.y * t[2] - q.z * t[1]),
            v[1] + q.w * t[1] + (q.z * t[0] - q.x * t[2]),
            v[2] + q.w * t[2] + (q.x * t[1] - q.y * t[0]),
        ];
        let normal = panel.basis()[2];
        for i in 0..3 {
            assert!(
                (rotated[i] - normal[i]).abs() < 1e-5,
                "{rotated:?} vs {normal:?}"
            );
        }
    }
}
