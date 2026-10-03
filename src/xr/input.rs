//! Controller input: an aim ray per hand (smoothed), select (trigger or A),
//! back (B), scroll (thumbstick; grip held = faster), jump (D-pad left/right
//! or a sideways stick flick), volume (D-pad up/down). Bindings are suggested
//! for the Steam Frame controller, Index controllers and the generic simple
//! profile; which one the runtime picked is logged.

use super::context::XrContext;
use openxr as xr;

pub struct Ray {
    pub origin: [f32; 3],
    pub direction: [f32; 3],
}

#[derive(Default)]
pub struct InputState {
    /// Aim rays per hand (left, right), when tracked.
    pub rays: [Option<Ray>; 2],
    /// Select pressed this frame, per hand.
    pub select: [bool; 2],
    /// Select held down, per hand (dragging).
    pub select_held: [bool; 2],
    pub back: bool,
    /// Thumbstick pressed in (reset view) this frame.
    pub reset: bool,
    /// Grip (lower trigger) held on either hand: a modifier, e.g. fast scroll.
    pub grip: bool,
    /// Jump this frame: -1 back, +1 forward (D-pad left/right, repeating
    /// while held, or a sideways thumbstick flick).
    pub seek: i32,
    /// Volume step this frame: -1 down, +1 up (D-pad down/up, repeating).
    pub volume: i32,
    /// Thumbstick vertical deflection (-1..1, up positive), strongest hand.
    pub scroll: f32,
}

pub struct Input {
    set: xr::ActionSet,
    aim: xr::Action<xr::Posef>,
    select: xr::Action<bool>,
    back: xr::Action<bool>,
    reset: xr::Action<bool>,
    grip: xr::Action<bool>,
    seek_back: xr::Action<bool>,
    seek_forward: xr::Action<bool>,
    volume_up: xr::Action<bool>,
    volume_down: xr::Action<bool>,
    scroll: xr::Action<xr::Vector2f>,
    hands: [xr::Path; 2],
    /// D-pad left, right, up, down: press-and-hold repeat.
    repeats: [Repeat; 4],
    /// Interaction profile last logged per hand (None: not yet).
    profiles: [Option<xr::Path>; 2],
    spaces: [xr::Space; 2],
    /// Per hand: a sideways stick flick must return to centre before the next.
    flick_ready: [bool; 2],
    /// Per hand: origin and direction filters.
    filters: [[OneEuro; 2]; 2],
}

/// Hold-to-repeat for a button: fires on press, then after [`REPEAT_DELAY`]
/// every [`REPEAT_EVERY`] while still held.
#[derive(Clone, Copy, Default)]
struct Repeat {
    /// Display time (ns) of the next repeat while held.
    next: Option<i64>,
}

const REPEAT_DELAY: i64 = 500_000_000;
const REPEAT_EVERY: i64 = 250_000_000;

impl Repeat {
    fn update(&mut self, held: bool, now: i64) -> bool {
        match self.next {
            _ if !held => {
                self.next = None;
                false
            }
            None => {
                self.next = Some(now + REPEAT_DELAY);
                true
            }
            Some(at) if now >= at => {
                self.next = Some(now + REPEAT_EVERY);
                true
            }
            Some(_) => false,
        }
    }
}

/// One Euro filter (Casiez et al.): heavy smoothing while still, little lag
/// when moving fast. Filters a 3-vector.
#[derive(Clone, Copy)]
struct OneEuro {
    min_cutoff: f32,
    beta: f32,
    value: Option<[f32; 3]>,
    speed: [f32; 3],
    time: i64,
}

impl OneEuro {
    const fn new(min_cutoff: f32, beta: f32) -> Self {
        Self {
            min_cutoff,
            beta,
            value: None,
            speed: [0.0; 3],
            time: 0,
        }
    }

    fn alpha(cutoff: f32, dt: f32) -> f32 {
        let tau = 1.0 / (2.0 * std::f32::consts::PI * cutoff);
        1.0 / (1.0 + tau / dt)
    }

    fn filter(&mut self, x: [f32; 3], time: i64) -> [f32; 3] {
        let Some(prev) = self.value else {
            self.value = Some(x);
            self.time = time;
            return x;
        };
        let dt = ((time - self.time) as f32 / 1e9).clamp(1e-4, 0.1);
        self.time = time;
        let a_d = Self::alpha(1.0, dt);
        let mut out = [0.0; 3];
        for i in 0..3 {
            let raw_speed = (x[i] - prev[i]) / dt;
            self.speed[i] += a_d * (raw_speed - self.speed[i]);
            let cutoff = self.min_cutoff + self.beta * self.speed[i].abs();
            out[i] = prev[i] + Self::alpha(cutoff, dt) * (x[i] - prev[i]);
        }
        self.value = Some(out);
        out
    }

    fn reset(&mut self) {
        self.value = None;
    }
}

fn rotate(q: xr::Quaternionf, v: [f32; 3]) -> [f32; 3] {
    // v' = v + 2w(q×v) + 2 q×(q×v)
    let (qx, qy, qz, w) = (q.x, q.y, q.z, q.w);
    let t = [
        2.0 * (qy * v[2] - qz * v[1]),
        2.0 * (qz * v[0] - qx * v[2]),
        2.0 * (qx * v[1] - qy * v[0]),
    ];
    [
        v[0] + w * t[0] + (qy * t[2] - qz * t[1]),
        v[1] + w * t[1] + (qz * t[0] - qx * t[2]),
        v[2] + w * t[2] + (qx * t[1] - qy * t[0]),
    ]
}

impl Input {
    pub fn new(ctx: &XrContext) -> anyhow::Result<Self> {
        let xr_ = &ctx.xr;
        let hands = [
            xr_.string_to_path("/user/hand/left")?,
            xr_.string_to_path("/user/hand/right")?,
        ];
        let set = xr_.create_action_set("player", "Player", 0)?;
        let aim = set.create_action::<xr::Posef>("aim", "Pointer", &hands)?;
        let select = set.create_action::<bool>("select", "Select", &hands)?;
        let back = set.create_action::<bool>("back", "Back", &hands)?;
        let reset = set.create_action::<bool>("reset", "Reset view", &hands)?;
        let grip = set.create_action::<bool>("grip", "Modifier", &hands)?;
        // Names are kept (SteamVR stores rebindings by them); the jump
        // length is a setting, so the labels don't give one.
        let seek_back = set.create_action::<bool>("seek_back", "Jump back", &hands)?;
        let seek_forward = set.create_action::<bool>("seek_forward", "Jump forward", &hands)?;
        let volume_up = set.create_action::<bool>("volume_up", "Volume up", &hands)?;
        let volume_down = set.create_action::<bool>("volume_down", "Volume down", &hands)?;
        let scroll = set.create_action::<xr::Vector2f>("scroll", "Scroll", &hands)?;

        let binding = |action: &str, path: xr::Path| match action {
            "aim" => xr::Binding::new(&aim, path),
            "select" => xr::Binding::new(&select, path),
            "back" => xr::Binding::new(&back, path),
            "reset" => xr::Binding::new(&reset, path),
            "grip" => xr::Binding::new(&grip, path),
            "seek_back" => xr::Binding::new(&seek_back, path),
            "seek_forward" => xr::Binding::new(&seek_forward, path),
            "volume_up" => xr::Binding::new(&volume_up, path),
            "volume_down" => xr::Binding::new(&volume_down, path),
            _ => xr::Binding::new(&scroll, path),
        };
        // Suggests every binding the runtime accepts for `profile` (each is
        // checked on its own first, since one bad path rejects the whole set).
        // An input is for both hands unless prefixed "left:" or "right:".
        // Nothing is suggested unless all of `required` were accepted, so a
        // profile we only half know can't replace a working one.
        let suggest =
            |profile: &str, wanted: &[(&str, &str)], required: &[&str]| -> anyhow::Result<usize> {
                let profile_path = xr_.string_to_path(profile)?;
                let mut accepted = Vec::new();
                let mut rejected = Vec::new();
                for hand in ["left", "right"] {
                    for (action, input) in wanted {
                        let input = match input.split_once(':') {
                            Some((only, rest)) if only == hand => rest,
                            Some(_) => continue,
                            None => input,
                        };
                        let path =
                            xr_.string_to_path(&format!("/user/hand/{hand}/input/{input}"))?;
                        if xr_
                            .suggest_interaction_profile_bindings(
                                profile_path,
                                &[binding(action, path)],
                            )
                            .is_ok()
                        {
                            accepted.push((*action, path));
                        } else {
                            rejected.push(format!("{hand}:{input}"));
                        }
                    }
                }
                if let Some(missing) = required
                    .iter()
                    .find(|r| !accepted.iter().any(|(a, _)| a == *r))
                {
                    anyhow::bail!("no binding for {missing}");
                }
                // Which inputs a profile lacks tells us what to bind instead.
                if !rejected.is_empty() {
                    eprintln!("Input: {profile} rejected {}", rejected.join(", "));
                }
                let list: Vec<_> = accepted.iter().map(|(a, p)| binding(a, *p)).collect();
                if !list.is_empty() {
                    xr_.suggest_interaction_profile_bindings(profile_path, &list)?;
                }
                Ok(list.len())
            };
        let required = ["aim", "select", "back"];
        // Steam Frame controllers: A/B on the right, a D-pad on the left
        // (both hands are tried; paths a controller lacks are rejected).
        let frame = [
            ("aim", "aim/pose"),
            ("select", "trigger/click"),
            ("select", "trigger/value"),
            ("select", "right:a/click"),
            ("back", "right:b/click"),
            ("scroll", "thumbstick"),
            ("reset", "thumbstick/click"),
            ("grip", "squeeze/click"),
            ("grip", "squeeze/value"),
            ("grip", "grip/click"),
            ("seek_back", "dpad_left/click"),
            ("seek_forward", "dpad_right/click"),
            ("volume_up", "dpad_up/click"),
            ("volume_down", "dpad_down/click"),
        ];
        for profile in [
            "/interaction_profiles/valve/frame_controller",
            "/interaction_profiles/valve/frame_controller_valve",
        ] {
            match suggest(profile, &frame, &required) {
                Ok(n) => eprintln!("Input: {profile}: {n} bindings"),
                Err(e) => eprintln!("Input: {profile} not used: {e}"),
            }
        }
        // Index bindings, which SteamVR maps Frame controllers onto when the
        // native profile isn't available. The Frame's left D-pad arrives as
        // the left A (down) and B (left/right/up), so A and B count only on
        // the right: the D-pad must not stop the video. Left/right/up can't be
        // told apart there, so this profile has no D-pad jumps or volume:
        // only the stick flick jumps.
        let index = [
            ("aim", "aim/pose"),
            ("select", "trigger/click"),
            ("select", "trigger/value"),
            ("select", "right:a/click"),
            ("back", "right:b/click"),
            ("scroll", "thumbstick"),
            ("reset", "thumbstick/click"),
            ("grip", "squeeze/click"),
            ("grip", "squeeze/value"),
        ];
        match suggest(
            "/interaction_profiles/valve/index_controller",
            &index,
            &required,
        ) {
            Ok(n) => eprintln!("Input: index_controller: {n} bindings"),
            Err(e) => eprintln!("Input: index_controller not used: {e}"),
        }
        suggest(
            "/interaction_profiles/khr/simple_controller",
            &[
                ("aim", "aim/pose"),
                ("select", "select/click"),
                ("back", "menu/click"),
            ],
            &[],
        )?;
        ctx.session.attach_action_sets(&[&set])?;
        let spaces = [
            aim.create_space(&ctx.session, hands[0], xr::Posef::IDENTITY)?,
            aim.create_space(&ctx.session, hands[1], xr::Posef::IDENTITY)?,
        ];
        Ok(Self {
            set,
            aim,
            select,
            back,
            reset,
            grip,
            seek_back,
            seek_forward,
            volume_up,
            volume_down,
            scroll,
            hands,
            repeats: Default::default(),
            profiles: [None; 2],
            spaces,
            flick_ready: [true; 2],
            // Direction: unit vector (rad/s-ish speeds); origin: metres.
            filters: [[OneEuro::new(3.0, 6.0), OneEuro::new(1.5, 0.6)]; 2],
        })
    }

    pub fn poll(
        &mut self,
        ctx: &XrContext,
        space: &xr::Space,
        time: xr::Time,
    ) -> anyhow::Result<InputState> {
        ctx.session.sync_actions(&[(&self.set).into()])?;
        // Until both hands have a profile (later changes arrive as events).
        if self
            .profiles
            .iter()
            .any(|p| p.is_none_or(|p| p == xr::Path::NULL))
        {
            self.log_profiles(ctx);
        }
        let mut state = InputState::default();
        let now = time.as_nanos();
        let mut dpad = [false; 4];
        let pressed = |action: &xr::Action<bool>, hand: xr::Path| -> anyhow::Result<bool> {
            let s = action.state(&ctx.session, hand)?;
            Ok(s.is_active && s.current_state && s.changed_since_last_sync)
        };
        for (i, &hand) in self.hands.iter().enumerate() {
            state.select[i] = pressed(&self.select, hand)?;
            let held = self.select.state(&ctx.session, hand)?;
            state.select_held[i] = held.is_active && held.current_state;
            state.back |= pressed(&self.back, hand)?;
            state.reset |= pressed(&self.reset, hand)?;
            let grip = self.grip.state(&ctx.session, hand)?;
            state.grip |= grip.is_active && grip.current_state;
            let dpad_actions = [
                &self.seek_back,
                &self.seek_forward,
                &self.volume_up,
                &self.volume_down,
            ];
            for (held, action) in dpad.iter_mut().zip(dpad_actions) {
                let s = action.state(&ctx.session, hand)?;
                *held |= s.is_active && s.current_state;
            }
            let stick = self.scroll.state(&ctx.session, hand)?;
            if stick.is_active {
                let (x, y) = (stick.current_state.x, stick.current_state.y);
                if y.abs() > state.scroll.abs() {
                    state.scroll = y;
                }
                // A clearly sideways flick seeks; it must return to centre first.
                if x.abs() < 0.3 {
                    self.flick_ready[i] = true;
                } else if self.flick_ready[i] && x.abs() > 0.8 && x.abs() > 2.0 * y.abs() {
                    self.flick_ready[i] = false;
                    state.seek = if x > 0.0 { 1 } else { -1 };
                }
            }
            if self.aim.is_active(&ctx.session, hand)? {
                let location = self.spaces[i].locate(space, time)?;
                let flags = location.location_flags;
                if flags.contains(xr::SpaceLocationFlags::POSITION_VALID)
                    && flags.contains(xr::SpaceLocationFlags::ORIENTATION_VALID)
                {
                    let p = location.pose.position;
                    let t = time.as_nanos();
                    let origin = self.filters[i][0].filter([p.x, p.y, p.z], t);
                    let d = self.filters[i][1]
                        .filter(rotate(location.pose.orientation, [0.0, 0.0, -1.0]), t);
                    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-6);
                    state.rays[i] = Some(Ray {
                        origin,
                        direction: [d[0] / len, d[1] / len, d[2] / len],
                    });
                } else {
                    self.filters[i][0].reset();
                    self.filters[i][1].reset();
                }
            }
        }
        let fired: Vec<bool> = (self.repeats.iter_mut().zip(dpad))
            .map(|(r, held)| r.update(held, now))
            .collect();
        if fired[0] {
            state.seek = -1;
        }
        if fired[1] {
            state.seek = 1;
        }
        state.volume = fired[2] as i32 - fired[3] as i32;
        Ok(state)
    }

    /// Logs the interaction profile the runtime chose for each hand (at
    /// first, then whenever it changes): it decides which buttons work.
    pub fn log_profiles(&mut self, ctx: &XrContext) {
        for (i, hand) in ["left", "right"].into_iter().enumerate() {
            // NULL: none chosen yet (no controller seen).
            let profile = match ctx.session.current_interaction_profile(self.hands[i]) {
                Ok(p) => p,
                Err(e) => {
                    if self.profiles[i].is_none() {
                        eprintln!("Input: {hand} hand profile unknown: {e}");
                        self.profiles[i] = Some(xr::Path::NULL);
                    }
                    continue;
                }
            };
            if self.profiles[i] == Some(profile) {
                continue;
            }
            let name = if profile == xr::Path::NULL {
                "no profile yet".into()
            } else {
                ctx.xr
                    .path_to_string(profile)
                    .unwrap_or_else(|e| format!("? ({e})"))
            };
            eprintln!("Input: {hand} hand uses {name}");
            self.profiles[i] = Some(profile);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_euro_smooths_jitter_but_follows_motion() {
        let mut f = OneEuro::new(3.0, 6.0);
        let step = 11_111_111; // 90 Hz
        // Holding still with ±0.004 noise (~0.25° on a unit vector).
        let mut max_dev = 0.0f32;
        for i in 0..200 {
            let noise = if i % 2 == 0 { 0.004 } else { -0.004 };
            let out = f.filter([noise, 0.0, -1.0], i * step);
            if i > 20 {
                max_dev = max_dev.max(out[0].abs());
            }
        }
        assert!(max_dev < 0.001, "jitter left: {max_dev}");
        // A fast turn is followed within a few frames.
        let mut out = [0.0; 3];
        for i in 200..210 {
            out = f.filter([0.5, 0.0, -0.8], i * step);
        }
        assert!((out[0] - 0.5).abs() < 0.05, "lagging: {out:?}");
    }

    #[test]
    fn held_buttons_repeat_after_a_delay() {
        let ms = 1_000_000;
        let mut r = Repeat::default();
        let fired: Vec<i64> = (0..100)
            .map(|i| i * 11 * ms)
            .filter(|&t| r.update(t < 1000 * ms, t))
            .collect();
        // Press, then from 0.5 s every ~0.25 s until released at 1 s.
        assert_eq!(fired.len(), 3, "{fired:?}");
        assert_eq!(fired[0], 0);
        assert!((500 * ms..520 * ms).contains(&fired[1]));
        assert!((750 * ms..780 * ms).contains(&fired[2]));
        assert!(!r.update(false, 1100 * ms));
        assert!(r.update(true, 1200 * ms), "a new press fires at once");
    }

    #[test]
    fn rotation_turns_forward_vector() {
        // 90° yaw to the left (about +Y) maps -Z forward to -X.
        let half = std::f32::consts::FRAC_PI_4;
        let q = xr::Quaternionf {
            x: 0.0,
            y: half.sin(),
            z: 0.0,
            w: half.cos(),
        };
        let v = rotate(q, [0.0, 0.0, -1.0]);
        assert!(
            (v[0] + 1.0).abs() < 1e-5 && v[1].abs() < 1e-5 && v[2].abs() < 1e-5,
            "{v:?}"
        );
    }
}
