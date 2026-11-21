//! Slew-to-cue: turn a detection bounding box into one absolute PTZ move.
//!
//! The camera is modelled as an ideal pinhole on a pan-then-tilt gimbal. Given
//! the current pan/tilt/zoom, the box centre is converted to a ray, rotated by
//! the current tilt, and turned back into absolute pan/tilt angles. That is
//! exact for an ideal gimbal at any tilt, unlike the usual "pixels × degrees
//! per pixel" shortcut, which drifts badly when the camera looks steeply down.
//!
//! Issuing a single `AbsoluteMove` per cue (instead of streaming
//! `ContinuousMove` from detections) avoids overshoot when glass-to-glass
//! latency is around a second.

use super::{PtzDriver, PtzError, PtzPosition};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Detection box in pixels, top-left origin, y down.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BBox {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Per-camera calibration mapping the ONVIF generic space to degrees.
#[derive(Debug, Clone, PartialEq)]
pub struct LensModel {
    /// Horizontal field of view at zoom = 0 (widest), degrees.
    pub hfov_wide_deg: f64,
    /// Horizontal field of view at zoom = 1 (tele), degrees.
    pub hfov_tele_deg: f64,
    /// Degrees at generic pan = -1 and +1.
    pub pan_range_deg: (f64, f64),
    /// Degrees at generic tilt = -1 and +1 (0° = horizon, positive = up).
    pub tilt_range_deg: (f64, f64),
    /// Pan wraps around (endless 360° rotation).
    pub pan_continuous: bool,
    /// Set for ceiling-mounted / image-flipped cameras where generic +tilt points down.
    pub invert_tilt: bool,
}

impl LensModel {
    /// Typical 30× outdoor PTZ dome: 60°…2.1° HFOV, endless pan, -90°…+20° tilt.
    pub fn typical_30x() -> Self {
        Self {
            hfov_wide_deg: 60.0,
            hfov_tele_deg: 2.1,
            pan_range_deg: (-180.0, 180.0),
            tilt_range_deg: (-90.0, 20.0),
            pan_continuous: true,
            invert_tilt: false,
        }
    }

    /// `tan(hfov/2)` at generic zoom `z`. Focal length is interpolated
    /// geometrically between wide and tele, which matches real zoom lenses far
    /// better than linear FOV interpolation. Calibrate per model if it matters.
    pub fn half_tan_h(&self, z: f64) -> f64 {
        let tw = (self.hfov_wide_deg.to_radians() / 2.0).tan();
        let tt = (self.hfov_tele_deg.to_radians() / 2.0).tan();
        tw * (tt / tw).powf(z.clamp(0.0, 1.0))
    }

    /// Inverse of [`LensModel::half_tan_h`]: zoom giving `tan(hfov/2) = t`, clamped to `[0, 1]`.
    pub fn zoom_for_half_tan_h(&self, t: f64) -> f64 {
        let tw = (self.hfov_wide_deg.to_radians() / 2.0).tan();
        let tt = (self.hfov_tele_deg.to_radians() / 2.0).tan();
        if t.is_nan() || t <= 0.0 || (tt - tw).abs() < f64::EPSILON {
            return 0.0;
        }
        ((t / tw).ln() / (tt / tw).ln()).clamp(0.0, 1.0)
    }

    fn sign(&self) -> f64 {
        if self.invert_tilt {
            -1.0
        } else {
            1.0
        }
    }

    pub fn pan_to_deg(&self, p: f64) -> f64 {
        lerp(self.pan_range_deg, p)
    }
    pub fn tilt_to_deg(&self, t: f64) -> f64 {
        lerp(self.tilt_range_deg, self.sign() * t)
    }
    pub fn deg_to_pan(&self, d: f64) -> f64 {
        let p = unlerp(self.pan_range_deg, d);
        if self.pan_continuous {
            wrap_unit(p)
        } else {
            p.clamp(-1.0, 1.0)
        }
    }
    pub fn deg_to_tilt(&self, d: f64) -> f64 {
        (self.sign() * unlerp(self.tilt_range_deg, d)).clamp(-1.0, 1.0)
    }
}

fn lerp((lo, hi): (f64, f64), g: f64) -> f64 {
    lo + (g + 1.0) * 0.5 * (hi - lo)
}
fn unlerp((lo, hi): (f64, f64), d: f64) -> f64 {
    (d - lo) / (hi - lo) * 2.0 - 1.0
}
/// Wrap into `[-1, 1)`.
pub fn wrap_unit(p: f64) -> f64 {
    (p + 1.0).rem_euclid(2.0) - 1.0
}

/// Tuning for [`target_for_bbox`] and [`SlewToCue`].
#[derive(Debug, Clone, PartialEq)]
pub struct CueConfig {
    /// Ignore cues whose centre is within this fraction of the frame from the
    /// centre (e.g. 0.05 = ±5 %), unless a zoom change is requested.
    pub deadband: f64,
    /// Zoom so the box fills this fraction of the frame (the tighter of width
    /// and height). `None` keeps the current zoom.
    pub target_fill: Option<f64>,
    /// Never zoom past this generic value.
    pub max_zoom: f64,
    /// Ignore zoom changes smaller than this (avoids hunting).
    pub zoom_deadband: f64,
    /// Generic move speed `(0, 1]`; `None` = camera default.
    pub speed: Option<f64>,
    /// Minimum time between issued moves.
    pub min_interval: Duration,
    /// Don't issue a new move while the camera reports it is still moving.
    pub skip_while_moving: bool,
}

impl Default for CueConfig {
    fn default() -> Self {
        Self {
            deadband: 0.05,
            target_fill: None,
            max_zoom: 1.0,
            zoom_deadband: 0.02,
            speed: Some(0.8),
            min_interval: Duration::from_millis(1500),
            skip_while_moving: true,
        }
    }
}

/// Absolute PTZ target that centres `bbox`, or `None` if it is already inside
/// the deadband and no zoom change is needed.
pub fn target_for_bbox(
    lens: &LensModel,
    cfg: &CueConfig,
    current: PtzPosition,
    bbox: BBox,
    frame_w: f64,
    frame_h: f64,
) -> Option<PtzPosition> {
    if !(frame_w > 0.0 && frame_h > 0.0) {
        return None;
    }
    let aspect = frame_w / frame_h;
    // Box centre relative to frame centre, in [-0.5, 0.5]; y up.
    let cx = (bbox.x + bbox.w / 2.0) / frame_w - 0.5;
    let cy = 0.5 - (bbox.y + bbox.h / 2.0) / frame_h;

    let th = lens.half_tan_h(current.zoom);
    let tv = th / aspect;

    // Zoom target first, so the deadband check can see whether zoom changes.
    let zoom = match cfg.target_fill {
        Some(fill) if fill > 0.0 => {
            let fw = (bbox.w / frame_w).abs();
            let fh = (bbox.h / frame_h).abs();
            // Required tan(hfov/2) so the box occupies `fill` of the frame.
            let need = (fw * th).max(fh * tv * aspect) / fill;
            let z = lens.zoom_for_half_tan_h(need).min(cfg.max_zoom);
            if (z - current.zoom).abs() < cfg.zoom_deadband {
                current.zoom
            } else {
                z
            }
        }
        _ => current.zoom,
    };

    if cx.abs() <= cfg.deadband && cy.abs() <= cfg.deadband && zoom == current.zoom {
        return None;
    }

    // Ray in camera frame: x right, y up, z forward.
    let (u, v) = (2.0 * cx * th, 2.0 * cy * tv);
    let phi = lens.tilt_to_deg(current.tilt).to_radians();
    let (s, c) = phi.sin_cos();
    let (x, y, z) = (u, v * c + s, -v * s + c);

    let pan_deg = lens.pan_to_deg(current.pan) + x.atan2(z).to_degrees();
    let tilt_deg = y.atan2((x * x + z * z).sqrt()).to_degrees();

    Some(PtzPosition {
        pan: lens.deg_to_pan(pan_deg),
        tilt: lens.deg_to_tilt(tilt_deg),
        zoom,
    })
}

/// What [`SlewToCue::cue`] did.
#[derive(Debug, Clone, PartialEq)]
pub enum CueOutcome {
    Moved(PtzPosition),
    /// An operator has control.
    Held,
    /// Within `min_interval` of the previous move.
    RateLimited,
    /// Camera still executing the previous move.
    CameraMoving,
    /// Target already centred (and no zoom change).
    InDeadband,
}

struct CueState {
    last_move: Option<Instant>,
    hold_until: Option<Instant>,
}

/// Rate-limited, operator-aware slew-to-cue on top of any [`PtzDriver`].
pub struct SlewToCue<D: PtzDriver> {
    driver: D,
    pub lens: LensModel,
    pub cfg: CueConfig,
    state: Mutex<CueState>,
}

impl<D: PtzDriver> SlewToCue<D> {
    pub fn new(driver: D, lens: LensModel, cfg: CueConfig) -> Self {
        Self {
            driver,
            lens,
            cfg,
            state: Mutex::new(CueState {
                last_move: None,
                hold_until: None,
            }),
        }
    }

    pub fn driver(&self) -> &D {
        &self.driver
    }

    /// Suppress automatic moves for `d` (call on every operator joystick/preset action).
    pub fn operator_hold(&self, d: Duration) {
        self.state.lock().unwrap().hold_until = Some(Instant::now() + d);
    }

    pub fn release_hold(&self) {
        self.state.lock().unwrap().hold_until = None;
    }

    /// Centre (and optionally zoom onto) a detection.
    pub async fn cue(
        &self,
        bbox: BBox,
        frame_w: f64,
        frame_h: f64,
    ) -> Result<CueOutcome, PtzError> {
        {
            let st = self.state.lock().unwrap();
            let now = Instant::now();
            if st.hold_until.is_some_and(|t| now < t) {
                return Ok(CueOutcome::Held);
            }
            if st
                .last_move
                .is_some_and(|t| now.duration_since(t) < self.cfg.min_interval)
            {
                return Ok(CueOutcome::RateLimited);
            }
        }

        let status = self.driver.status().await?;
        if self.cfg.skip_while_moving && status.is_moving() {
            return Ok(CueOutcome::CameraMoving);
        }
        let current = status.position.ok_or(PtzError::NoPosition)?;

        let Some(target) = target_for_bbox(&self.lens, &self.cfg, current, bbox, frame_w, frame_h)
        else {
            return Ok(CueOutcome::InDeadband);
        };

        self.driver.absolute(target, self.cfg.speed).await?;
        self.state.lock().unwrap().last_move = Some(Instant::now());
        Ok(CueOutcome::Moved(target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptz::{MoveState, PtzState, PtzVelocity};
    use async_trait::async_trait;

    const W: f64 = 1920.0;
    const H: f64 = 1080.0;

    fn lens() -> LensModel {
        LensModel::typical_30x()
    }
    fn cfg() -> CueConfig {
        CueConfig {
            min_interval: Duration::ZERO,
            ..Default::default()
        }
    }
    /// Box of size `s` centred at pixel (cx, cy).
    fn bx(cx: f64, cy: f64, s: f64) -> BBox {
        BBox {
            x: cx - s / 2.0,
            y: cy - s / 2.0,
            w: s,
            h: s,
        }
    }
    /// Pan/tilt (deg) of the horizon-level camera at tilt 0 deg.
    fn level() -> PtzPosition {
        let l = lens();
        PtzPosition {
            pan: 0.0,
            tilt: l.deg_to_tilt(0.0),
            zoom: 0.0,
        }
    }
    fn close(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn centred_box_is_deadband() {
        assert_eq!(
            target_for_bbox(&lens(), &cfg(), level(), bx(W / 2.0, H / 2.0, 50.0), W, H),
            None
        );
    }

    #[test]
    fn right_edge_pans_half_hfov_at_level() {
        let l = lens();
        let t = target_for_bbox(&l, &cfg(), level(), bx(W, H / 2.0, 2.0), W, H).unwrap();
        assert!(
            close(l.pan_to_deg(t.pan), 30.0, 1e-6),
            "{}",
            l.pan_to_deg(t.pan)
        );
        assert!(close(l.tilt_to_deg(t.tilt), 0.0, 1e-6));
        assert_eq!(t.zoom, 0.0);
    }

    #[test]
    fn top_edge_tilts_up_half_vfov() {
        let l = lens();
        let t = target_for_bbox(&l, &cfg(), level(), bx(W / 2.0, 0.0, 2.0), W, H).unwrap();
        let half_v = (l.half_tan_h(0.0) / (W / H)).atan().to_degrees();
        assert!(close(l.tilt_to_deg(t.tilt), half_v, 1e-6));
        assert!(close(l.pan_to_deg(t.pan), 0.0, 1e-6));
    }

    #[test]
    fn steep_tilt_needs_more_pan_than_naive() {
        // Looking 60 deg down, a box at the right edge is further round in pan
        // than half the HFOV.
        let l = lens();
        let cur = PtzPosition {
            pan: 0.0,
            tilt: l.deg_to_tilt(-60.0),
            zoom: 0.0,
        };
        let t = target_for_bbox(&l, &cfg(), cur, bx(W, H / 2.0, 2.0), W, H).unwrap();
        assert!(l.pan_to_deg(t.pan) > 45.0, "{}", l.pan_to_deg(t.pan));
    }

    #[test]
    fn pan_wraps_across_180() {
        let l = lens();
        let cur = PtzPosition {
            pan: l.deg_to_pan(170.0),
            ..level()
        };
        let t = target_for_bbox(&l, &cfg(), cur, bx(W, H / 2.0, 2.0), W, H).unwrap();
        assert!(
            close(l.pan_to_deg(t.pan), -160.0, 1e-6),
            "{}",
            l.pan_to_deg(t.pan)
        );
    }

    #[test]
    fn non_continuous_pan_clamps() {
        let l = LensModel {
            pan_continuous: false,
            pan_range_deg: (-170.0, 170.0),
            ..lens()
        };
        let cur = PtzPosition {
            pan: 1.0,
            ..level()
        };
        let t = target_for_bbox(&l, &cfg(), cur, bx(W, H / 2.0, 2.0), W, H).unwrap();
        assert_eq!(t.pan, 1.0);
    }

    #[test]
    fn tilt_clamps_at_limit() {
        let l = lens();
        let cur = PtzPosition {
            tilt: 1.0,
            ..level()
        };
        let t = target_for_bbox(&l, &cfg(), cur, bx(W / 2.0, 0.0, 2.0), W, H).unwrap();
        assert_eq!(t.tilt, 1.0);
    }

    #[test]
    fn zoom_round_trip() {
        let l = lens();
        for z in [0.0, 0.25, 0.5, 0.9, 1.0] {
            assert!(close(l.zoom_for_half_tan_h(l.half_tan_h(z)), z, 1e-9));
        }
    }

    #[test]
    fn zoom_to_fill() {
        // Centred wide box: 10% of frame width, 5% of height; fill 50% => tan(hfov/2) shrinks 5x.
        let l = lens();
        let c = CueConfig {
            target_fill: Some(0.5),
            ..cfg()
        };
        let wide = BBox {
            x: 0.45 * W,
            y: 0.475 * H,
            w: 0.1 * W,
            h: 0.05 * H,
        };
        let t = target_for_bbox(&l, &c, level(), wide, W, H).unwrap();
        let want = l.zoom_for_half_tan_h(l.half_tan_h(0.0) / 5.0);
        assert!(close(t.zoom, want, 1e-9), "{} vs {}", t.zoom, want);
        let capped = CueConfig { max_zoom: 0.1, ..c };
        let t = target_for_bbox(&l, &capped, level(), wide, W, H).unwrap();
        assert_eq!(t.zoom, 0.1);
        // Tall box: height is the binding constraint.
        let tall = BBox {
            x: 0.49 * W,
            y: 0.4 * H,
            w: 0.02 * W,
            h: 0.2 * H,
        };
        let t = target_for_bbox(&l, &c, level(), tall, W, H).unwrap();
        let want = l.zoom_for_half_tan_h(l.half_tan_h(0.0) * 0.2 / 0.5);
        assert!(close(t.zoom, want, 1e-9), "{} vs {}", t.zoom, want);
    }

    #[test]
    fn invert_tilt_flips_sign() {
        let l = LensModel {
            invert_tilt: true,
            ..lens()
        };
        let cur = PtzPosition {
            pan: 0.0,
            tilt: l.deg_to_tilt(0.0),
            zoom: 0.0,
        };
        let t = target_for_bbox(&l, &cfg(), cur, bx(W / 2.0, 0.0, 2.0), W, H).unwrap();
        assert!(l.tilt_to_deg(t.tilt) > 0.0);
        assert!(
            t.tilt < cur.tilt,
            "generic tilt should decrease when inverted"
        );
    }

    #[test]
    fn outputs_always_in_generic_range() {
        let l = lens();
        let c = CueConfig {
            target_fill: Some(0.3),
            deadband: 0.0,
            ..cfg()
        };
        for &p in &[-1.0, -0.3, 0.0, 0.7, 1.0] {
            for &ti in &[-1.0, 0.0, 1.0] {
                for &z in &[0.0, 0.5, 1.0] {
                    for &(x, y) in &[(0.0, 0.0), (W, H), (W, 0.0), (0.0, H), (300.0, 900.0)] {
                        let cur = PtzPosition {
                            pan: p,
                            tilt: ti,
                            zoom: z,
                        };
                        if let Some(t) = target_for_bbox(&l, &c, cur, bx(x, y, 40.0), W, H) {
                            assert!((-1.0..=1.0).contains(&t.pan), "{:?}", t);
                            assert!((-1.0..=1.0).contains(&t.tilt), "{:?}", t);
                            assert!((0.0..=1.0).contains(&t.zoom), "{:?}", t);
                        }
                    }
                }
            }
        }
    }

    struct Mock {
        pos: Mutex<PtzPosition>,
        moving: bool,
        moves: Mutex<u32>,
    }

    #[async_trait]
    impl PtzDriver for Mock {
        async fn move_velocity(&self, _: PtzVelocity) -> Result<(), PtzError> {
            Ok(())
        }
        async fn stop(&self) -> Result<(), PtzError> {
            Ok(())
        }
        async fn absolute(&self, t: PtzPosition, _: Option<f64>) -> Result<(), PtzError> {
            *self.pos.lock().unwrap() = t;
            *self.moves.lock().unwrap() += 1;
            Ok(())
        }
        async fn relative(&self, _: PtzPosition, _: Option<f64>) -> Result<(), PtzError> {
            Ok(())
        }
        async fn goto_preset(&self, _: &str) -> Result<(), PtzError> {
            Ok(())
        }
        async fn status(&self) -> Result<PtzState, PtzError> {
            let m = if self.moving {
                MoveState::Moving
            } else {
                MoveState::Idle
            };
            Ok(PtzState {
                position: Some(*self.pos.lock().unwrap()),
                pan_tilt: m,
                zoom: MoveState::Idle,
            })
        }
    }

    fn mock(moving: bool) -> Mock {
        Mock {
            pos: Mutex::new(level()),
            moving,
            moves: Mutex::new(0),
        }
    }

    #[tokio::test]
    async fn controller_moves_then_rate_limits() {
        let s = SlewToCue::new(
            mock(false),
            lens(),
            CueConfig {
                min_interval: Duration::from_secs(60),
                ..Default::default()
            },
        );
        let b = bx(W * 0.9, H / 2.0, 40.0);
        assert!(matches!(
            s.cue(b, W, H).await.unwrap(),
            CueOutcome::Moved(_)
        ));
        assert_eq!(s.cue(b, W, H).await.unwrap(), CueOutcome::RateLimited);
        assert_eq!(*s.driver().moves.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn controller_respects_hold_and_motion() {
        let s = SlewToCue::new(mock(false), lens(), cfg());
        s.operator_hold(Duration::from_secs(60));
        assert_eq!(
            s.cue(bx(W, H / 2.0, 4.0), W, H).await.unwrap(),
            CueOutcome::Held
        );
        s.release_hold();
        assert!(matches!(
            s.cue(bx(W, H / 2.0, 4.0), W, H).await.unwrap(),
            CueOutcome::Moved(_)
        ));

        let s = SlewToCue::new(mock(true), lens(), cfg());
        assert_eq!(
            s.cue(bx(W, H / 2.0, 4.0), W, H).await.unwrap(),
            CueOutcome::CameraMoving
        );
    }

    #[tokio::test]
    async fn repeated_cue_converges() {
        // After one move, the same world point is centred: re-projecting the box
        // that would now sit at frame centre lands in the deadband.
        let s = SlewToCue::new(mock(false), lens(), cfg());
        assert!(matches!(
            s.cue(bx(W * 0.8, H * 0.3, 20.0), W, H).await.unwrap(),
            CueOutcome::Moved(_)
        ));
        assert_eq!(
            s.cue(bx(W / 2.0, H / 2.0, 20.0), W, H).await.unwrap(),
            CueOutcome::InDeadband
        );
    }
}
