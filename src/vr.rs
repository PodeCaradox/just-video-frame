//! VR layout detection. Metadata wins; otherwise common filename conventions
//! (DeoVR / HereSphere / Skybox style tags such as `_180_LR`, `_360_TB`).
//! The result is a suggestion: the player keeps a per-file manual override.

use crate::media::VideoInfo;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Projection {
    Flat,
    Equirect180,
    Equirect360,
    /// Fisheye VR180 needs lens parameters; never rendered as equirectangular.
    Fisheye180,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stereo {
    Mono,
    SideBySide,
    TopBottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Evidence {
    Metadata,
    Filename,
    /// Guess from frame shape alone; the weakest evidence.
    Aspect,
    Default,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Layout {
    pub projection: Projection,
    pub stereo: Stereo,
    /// Right eye stored first (e.g. `_RL`, or stereo3d "inverted").
    pub swap_eyes: bool,
    pub projection_from: Evidence,
    pub stereo_from: Evidence,
}

/// Width / height of the picture one eye sees on a flat screen.
///
/// 3D films usually come half-packed: both eyes squeezed into one ordinary
/// frame (1920×1080 half-SBS holds two 960×1080 eyes), each meant to be
/// stretched back to the frame's shape. Full packing doubles the frame
/// instead (3840×1080). Containers don't record which, so guess from the
/// shape: a full-packed eye narrower than 1.25:1 or wider than 3:1 is
/// implausible for a film, so treat such frames as half-packed.
pub fn eye_aspect(width: u32, height: u32, layout: &Layout) -> f32 {
    let (w, h) = (width as f32, height.max(1) as f32);
    let flat = layout.projection == Projection::Flat;
    match layout.stereo {
        Stereo::SideBySide if flat && w / 2.0 / h < 1.25 => w / h,
        Stereo::SideBySide => w / 2.0 / h,
        Stereo::TopBottom if flat && w / (h / 2.0) > 3.0 => w / h,
        Stereo::TopBottom => w / (h / 2.0),
        Stereo::Mono => w / h,
    }
}

fn tokens(name: &str) -> Vec<String> {
    let stem = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = stem.rsplit_once('.').map_or(stem, |(s, _)| s);
    stem.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn projection_from_name(tokens: &[String]) -> Option<Projection> {
    const FISHEYE: &[&str] = &[
        "fisheye",
        "fisheye180",
        "fisheye190",
        "fisheye200",
        "f180",
        "mkx200",
        "mkx220",
        "vrca220",
        "rf52",
    ];
    if tokens.iter().any(|t| FISHEYE.contains(&t.as_str())) {
        return Some(Projection::Fisheye180);
    }
    for t in tokens {
        match t.as_str() {
            "180" | "180x180" | "vr180" | "180sbs" | "180lr" | "180tb" | "dome" => {
                return Some(Projection::Equirect180);
            }
            "360" | "vr360" | "360tb" | "360lr" | "360sbs" | "sphere" | "equirect" => {
                return Some(Projection::Equirect360);
            }
            _ => {}
        }
    }
    None
}

fn stereo_from_name(tokens: &[String]) -> Option<(Stereo, bool)> {
    for t in tokens {
        let t = t.as_str();
        let t = t
            .strip_prefix("180")
            .or_else(|| t.strip_prefix("360"))
            .unwrap_or(t);
        match t {
            "lr" | "sbs" | "3dh" | "hsbs" | "fsbs" | "halfsbs" | "fullsbs" => {
                return Some((Stereo::SideBySide, false));
            }
            "rl" => return Some((Stereo::SideBySide, true)),
            "tb" | "ou" | "3dv" | "htab" | "tab" | "overunder" | "hou" | "fou" | "halfou"
            | "fullou" => {
                return Some((Stereo::TopBottom, false));
            }
            "bt" => return Some((Stereo::TopBottom, true)),
            "mono" | "2d" => return Some((Stereo::Mono, false)),
            _ => {}
        }
    }
    None
}

fn projection_from_metadata(video: &VideoInfo) -> Option<Projection> {
    match video.projection.as_deref()? {
        "equirectangular" | "tiled equirectangular" => {
            let degrees = video.horizontal_degrees.unwrap_or(360.0);
            Some(if degrees <= 270.0 {
                Projection::Equirect180
            } else {
                Projection::Equirect360
            })
        }
        "half equirectangular" => Some(Projection::Equirect180),
        "fisheye" => Some(Projection::Fisheye180),
        "rectilinear" => Some(Projection::Flat),
        // Cubemaps and parametric immersive video are unsupported; fall back to names.
        _ => None,
    }
}

fn stereo_from_metadata(video: &VideoInfo) -> Option<(Stereo, bool)> {
    let stereo = match video.stereo_mode.as_deref()? {
        "2D" => Stereo::Mono,
        "side by side" => Stereo::SideBySide,
        "top and bottom" => Stereo::TopBottom,
        _ => return None,
    };
    Some((stereo, video.stereo_inverted))
}

impl Layout {
    /// A short name for file lists, e.g. "VR180 3D"; `None` for a plain flat video.
    pub fn short_label(&self) -> Option<String> {
        let shape = match self.projection {
            Projection::Flat if self.stereo == Stereo::Mono => return None,
            Projection::Flat => "",
            Projection::Equirect180 => "VR180",
            Projection::Equirect360 => "VR360",
            Projection::Fisheye180 => "Fisheye",
        };
        let depth = if self.stereo == Stereo::Mono {
            ""
        } else {
            "3D"
        };
        Some([shape, depth].join(" ").trim().to_string())
    }
}

pub fn detect(name: &str, video: Option<&VideoInfo>) -> Layout {
    let tokens = tokens(name);
    let (projection, projection_from) = video
        .and_then(projection_from_metadata)
        .map(|p| (p, Evidence::Metadata))
        .or_else(|| projection_from_name(&tokens).map(|p| (p, Evidence::Filename)))
        .unwrap_or((Projection::Flat, Evidence::Default));
    let ((stereo, swap_eyes), stereo_from) = video
        .and_then(stereo_from_metadata)
        .map(|s| (s, Evidence::Metadata))
        .or_else(|| stereo_from_name(&tokens).map(|s| (s, Evidence::Filename)))
        .unwrap_or(((Stereo::Mono, false), Evidence::Default));
    // Untagged 2:1 frames of 5.7K and wider are almost always VR180 side-by-side
    // (two square eyes). Smaller 2:1 frames are often mono 360, so leave them flat.
    if projection_from == Evidence::Default
        && stereo_from == Evidence::Default
        && let Some(v) = video
        && v.width >= 5760
        && v.width == 2 * v.height
    {
        return Layout {
            projection: Projection::Equirect180,
            stereo: Stereo::SideBySide,
            swap_eyes: false,
            projection_from: Evidence::Aspect,
            stereo_from: Evidence::Aspect,
        };
    }
    Layout {
        projection,
        stereo,
        swap_eyes,
        projection_from,
        stereo_from,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn by_name(name: &str) -> (Projection, Stereo, bool) {
        let l = detect(name, None);
        (l.projection, l.stereo, l.swap_eyes)
    }

    #[test]
    fn filename_conventions() {
        use Projection::*;
        use Stereo::*;
        assert_eq!(by_name("Trip_180_LR.mp4"), (Equirect180, SideBySide, false));
        assert_eq!(by_name("dive-360_TB.mkv"), (Equirect360, TopBottom, false));
        assert_eq!(
            by_name("Concert 180x180_3dh.mp4"),
            (Equirect180, SideBySide, false)
        );
        assert_eq!(by_name("clip_MKX200.mp4"), (Fisheye180, Mono, false));
        assert_eq!(
            by_name("scene_FISHEYE190_LR.mp4"),
            (Fisheye180, SideBySide, false)
        );
        assert_eq!(by_name("walk_360.mp4"), (Equirect360, Mono, false));
        assert_eq!(by_name("odd_180_RL.mp4"), (Equirect180, SideBySide, true));
        assert_eq!(by_name("movie.2024.2160p.mkv"), (Flat, Mono, false));
        assert_eq!(by_name("film_sbs.mkv"), (Flat, SideBySide, false));
        assert_eq!(by_name("dir/360/flat_video.mp4"), (Flat, Mono, false));
    }

    #[test]
    fn list_labels() {
        let label = |name| detect(name, None).short_label();
        assert_eq!(label("movie.2024.2160p.mkv"), None);
        assert_eq!(label("film_sbs.mkv").as_deref(), Some("3D"));
        assert_eq!(label("film_TB.mkv").as_deref(), Some("3D"));
        assert_eq!(label("Trip_180_LR.mp4").as_deref(), Some("VR180 3D"));
        assert_eq!(label("dive-360_TB.mkv").as_deref(), Some("VR360 3D"));
        assert_eq!(label("walk_360.mp4").as_deref(), Some("VR360"));
        assert_eq!(
            label("scene_FISHEYE190_LR.mp4").as_deref(),
            Some("Fisheye 3D")
        );
    }

    fn video(projection: Option<&str>, degrees: Option<f64>, stereo: Option<&str>) -> VideoInfo {
        VideoInfo {
            codec: "hevc".into(),
            profile: None,
            pixel_format: None,
            width: 8192,
            height: 4096,
            bit_depth: 10,
            fps: 60.0,
            stereo_mode: stereo.map(Into::into),
            stereo_inverted: false,
            projection: projection.map(Into::into),
            horizontal_degrees: degrees,
        }
    }

    #[test]
    fn untagged_wide_frames_guess_vr180_sbs() {
        let v = video(None, None, None);
        let l = detect("untagged_8k.mp4", Some(&v));
        assert_eq!(
            (l.projection, l.stereo),
            (Projection::Equirect180, Stereo::SideBySide)
        );
        assert_eq!(l.projection_from, Evidence::Aspect);
        let small = VideoInfo {
            width: 1280,
            height: 640,
            ..v.clone()
        };
        assert_eq!(
            detect("h265.mkv", Some(&small)).projection,
            Projection::Flat
        );
        // Any real tag wins over the shape guess.
        assert_eq!(detect("clip_360.mp4", Some(&v)).stereo, Stereo::Mono);
    }

    #[test]
    fn half_packed_3d_is_stretched_back() {
        let flat = |stereo| Layout {
            projection: Projection::Flat,
            stereo,
            swap_eyes: false,
            projection_from: Evidence::Filename,
            stereo_from: Evidence::Filename,
        };
        let sbs = flat(Stereo::SideBySide);
        let tb = flat(Stereo::TopBottom);
        let close = |a: f32, b: f32| (a - b).abs() < 0.01;
        // Half: 16:9 frame, and cropped 2.39:1 scope.
        assert!(close(eye_aspect(1920, 1080, &sbs), 16.0 / 9.0));
        assert!(close(eye_aspect(1920, 804, &sbs), 1920.0 / 804.0));
        assert!(close(eye_aspect(1920, 1080, &tb), 16.0 / 9.0));
        assert!(close(eye_aspect(1920, 804, &tb), 1920.0 / 804.0));
        // Full: doubled frames keep each eye's own shape, 4:3 included.
        assert!(close(eye_aspect(3840, 1080, &sbs), 16.0 / 9.0));
        assert!(close(eye_aspect(2880, 1080, &sbs), 4.0 / 3.0));
        assert!(close(eye_aspect(1920, 2160, &tb), 16.0 / 9.0));
        assert!(close(eye_aspect(1920, 1606, &tb), 1920.0 / 803.0));
        // Spherical video is never stretched (VR180 SBS has square eyes).
        let vr180 = Layout {
            projection: Projection::Equirect180,
            ..sbs
        };
        assert!(close(eye_aspect(5760, 2880, &vr180), 1.0));
        assert!(close(
            eye_aspect(1920, 1080, &flat(Stereo::Mono)),
            16.0 / 9.0
        ));
    }

    #[test]
    fn ripped_3d_films() {
        use Stereo::*;
        // Names and frame shapes of 3D Blu-ray rips. "VR" in names like these
        // only says the copy is meant for a headset: the film is flat.
        let film = |name: &str, width: u32, height: u32| {
            let v = VideoInfo {
                width,
                height,
                bit_depth: 8,
                fps: 24000.0 / 1001.0,
                ..video(None, None, None)
            };
            let l = detect(name, Some(&v));
            assert_eq!(
                (l.projection, l.stereo, l.swap_eyes),
                (Projection::Flat, SideBySide, false),
                "{name}"
            );
            eye_aspect(width, height, &l)
        };
        let close = |a: f32, b: f32| (a - b).abs() < 0.01;
        // Full SBS cropped to the film: two 2.4:1 eyes.
        assert!(close(
            film("VR-SBS_3840x_A_Film_4k_hevc.mkv", 3840, 800),
            2.4
        ));
        // Full SBS with the film letterboxed in two 16:9 eyes.
        assert!(close(
            film("VR-SBS_3840x_A_Film_4k_hevc.mkv", 3840, 1080),
            16.0 / 9.0
        ));
        // Half SBS: squeezed 960x1080 eyes, stretched back to 16:9.
        assert!(close(
            film(
                "A.Film.2014.3D.BluRay.Half-SBS.x.DTS-HD.MA.7.1_1080p_hevc.mkv",
                1920,
                1080
            ),
            16.0 / 9.0
        ));
        // Other spellings of the packing.
        let stereo = |name| detect(name, None).stereo;
        assert_eq!(stereo("A.Film.3D.HalfSBS.mkv"), SideBySide);
        assert_eq!(stereo("A.Film.3D.FullSBS.mkv"), SideBySide);
        assert_eq!(stereo("A.Film.3D.HOU.mkv"), TopBottom);
        assert_eq!(stereo("A.Film.3D.H-OU.mkv"), TopBottom);
        assert_eq!(stereo("A.Film.3D.FOU.mkv"), TopBottom);
        assert_eq!(stereo("A.Film.3D.HalfOU.mkv"), TopBottom);
    }

    #[test]
    fn metadata_beats_filename() {
        let v = video(Some("equirectangular"), Some(180.0), Some("side by side"));
        let l = detect("thing_360_TB.mp4", Some(&v));
        assert_eq!(l.projection, Projection::Equirect180);
        assert_eq!(l.stereo, Stereo::SideBySide);
        assert_eq!(l.projection_from, Evidence::Metadata);
        let v = video(Some("equirectangular"), Some(360.0), None);
        let l = detect("thing_TB.mp4", Some(&v));
        assert_eq!(l.projection, Projection::Equirect360);
        assert_eq!(
            (l.stereo, l.stereo_from),
            (Stereo::TopBottom, Evidence::Filename)
        );
    }
}
