//! Protocol-neutral PTZ control.
//!
//! [`PtzDriver`] is the trait an analytics pipeline (e.g. VisionGuard360) talks to;
//! [`OnvifPtz`] implements it over the ONVIF PTZ service. Other back-ends
//! (Hikvision ISAPI, Pelco-D over RS-485) can implement the same trait.
//!
//! All positions and speeds use the ONVIF *generic* spaces:
//! pan/tilt in `[-1, 1]`, zoom in `[0, 1]`, speeds in `[-1, 1]` / `[0, 1]`.
//!
//! [`slew`] turns a detection bounding box into an absolute PTZ target
//! ("slew-to-cue"), and [`SlewToCue`] wraps that with deadband, rate limiting
//! and an operator hold.

mod onvif_driver;
pub mod slew;

pub use onvif_driver::{OnvifPtz, OnvifPtzOptions};
pub use slew::{BBox, CueConfig, LensModel, SlewToCue};

use async_trait::async_trait;

/// Normalised PTZ position (ONVIF generic space).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PtzPosition {
    /// `[-1, 1]`, positive = right.
    pub pan: f64,
    /// `[-1, 1]`, positive = up.
    pub tilt: f64,
    /// `[0, 1]`, 0 = widest.
    pub zoom: f64,
}

/// Normalised velocity for continuous moves. `None` leaves that axis untouched.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PtzVelocity {
    pub pan_tilt: Option<(f64, f64)>,
    pub zoom: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveState {
    Idle,
    Moving,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PtzState {
    /// `None` if the camera does not report its position.
    pub position: Option<PtzPosition>,
    pub pan_tilt: MoveState,
    pub zoom: MoveState,
}

impl PtzState {
    pub fn is_moving(&self) -> bool {
        self.pan_tilt == MoveState::Moving || self.zoom == MoveState::Moving
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PtzError {
    #[error("transport: {0}")]
    Transport(#[from] schema::transport::Error),
    #[error("device has no PTZ service")]
    NoPtzService,
    #[error("device has no media service")]
    NoMediaService,
    #[error("no media profile with a PTZ configuration")]
    NoPtzProfile,
    #[error("camera did not report a PTZ position")]
    NoPosition,
    #[error("{0}")]
    Other(String),
}

#[async_trait]
pub trait PtzDriver: Send + Sync {
    /// Start moving at `velocity`; keeps going until [`PtzDriver::stop`].
    async fn move_velocity(&self, velocity: PtzVelocity) -> Result<(), PtzError>;
    /// Stop pan/tilt and zoom.
    async fn stop(&self) -> Result<(), PtzError>;
    /// Move to an absolute position. `speed` in `(0, 1]`, `None` = camera default.
    async fn absolute(&self, target: PtzPosition, speed: Option<f64>) -> Result<(), PtzError>;
    /// Move by an offset in generic space.
    async fn relative(&self, delta: PtzPosition, speed: Option<f64>) -> Result<(), PtzError>;
    async fn goto_preset(&self, preset_token: &str) -> Result<(), PtzError>;
    async fn status(&self) -> Result<PtzState, PtzError>;
}
