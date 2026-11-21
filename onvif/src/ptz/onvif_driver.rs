use super::{MoveState, PtzDriver, PtzError, PtzPosition, PtzState, PtzVelocity};
use crate::soap::client::{AuthType, Client, ClientBuilder, Credentials};
use async_trait::async_trait;
use schema::onvif as tt;
use std::time::Duration;
use url::Url;

const NS_PTZ: &str = "http://www.onvif.org/ver20/ptz/wsdl";
/// Wrong but seen in the wild (and in some simulators).
const NS_PTZ_V10: &str = "http://www.onvif.org/ver10/ptz/wsdl";
const NS_MEDIA: &str = "http://www.onvif.org/ver10/media/wsdl";

/// Connection options for [`OnvifPtz::connect`].
#[derive(Clone)]
pub struct OnvifPtzOptions {
    pub credentials: Option<Credentials>,
    pub auth_type: AuthType,
    /// Needed for devices that only accept WS-UsernameToken over plain HTTP.
    pub allow_username_token_over_http: bool,
    /// Use this media profile; otherwise the first profile with a PTZ configuration.
    pub profile_token: Option<String>,
    pub timeout: Duration,
}

impl Default for OnvifPtzOptions {
    fn default() -> Self {
        Self {
            credentials: None,
            auth_type: AuthType::Any,
            allow_username_token_over_http: false,
            profile_token: None,
            timeout: Duration::from_secs(5),
        }
    }
}

/// [`PtzDriver`] over the ONVIF PTZ service (ver20).
pub struct OnvifPtz {
    ptz: Client,
    profile_token: String,
}

fn client(uri: &Url, o: &OnvifPtzOptions) -> Client {
    ClientBuilder::new(uri)
        .credentials(o.credentials.clone())
        .auth_type(o.auth_type.clone())
        .allow_username_token_over_http(o.allow_username_token_over_http)
        .timeout(o.timeout)
        .build()
}

fn token(s: &str) -> tt::ReferenceToken {
    tt::ReferenceToken(s.to_string())
}

fn generic_speed(speed: Option<f64>) -> Option<tt::Ptzspeed> {
    speed.map(|s| {
        let s = s.clamp(0.0, 1.0);
        tt::Ptzspeed {
            pan_tilt: Some(tt::Vector2D {
                x: s,
                y: s,
                space: None,
            }),
            zoom: Some(tt::Vector1D { x: s, space: None }),
        }
    })
}

fn move_state(s: &Option<tt::MoveStatus>) -> MoveState {
    match s {
        Some(tt::MoveStatus::Idle) => MoveState::Idle,
        Some(tt::MoveStatus::Moving) => MoveState::Moving,
        _ => MoveState::Unknown,
    }
}

impl OnvifPtz {
    /// Wrap an existing PTZ-service client.
    pub fn new(ptz: Client, profile_token: impl Into<String>) -> Self {
        Self {
            ptz,
            profile_token: profile_token.into(),
        }
    }

    pub fn profile_token(&self) -> &str {
        &self.profile_token
    }

    /// Discover the PTZ and media services from the device-management URI
    /// (e.g. `http://10.0.0.5/onvif/device_service`) and pick a PTZ profile.
    pub async fn connect(devicemgmt_uri: &Url, opts: OnvifPtzOptions) -> Result<Self, PtzError> {
        let dm = client(devicemgmt_uri, &opts);
        let services = schema::devicemgmt::get_services(&dm, &Default::default())
            .await?
            .service;

        let base = devicemgmt_uri.origin();
        let find = |ns: &str| -> Result<Option<Url>, PtzError> {
            match services.iter().find(|s| s.namespace == ns) {
                None => Ok(None),
                Some(s) => {
                    let u = Url::parse(&s.x_addr).map_err(|e| PtzError::Other(e.to_string()))?;
                    // Don't send credentials to a host the operator didn't configure.
                    if u.origin() != base {
                        return Err(PtzError::Other(format!(
                            "service {} is not on the configured device {}",
                            u, devicemgmt_uri
                        )));
                    }
                    Ok(Some(u))
                }
            }
        };
        let ptz_uri = match find(NS_PTZ)? {
            Some(u) => u,
            None => find(NS_PTZ_V10)?.ok_or(PtzError::NoPtzService)?,
        };

        let profile_token = match opts.profile_token.clone() {
            Some(t) => t,
            None => {
                let media_uri = find(NS_MEDIA)?.ok_or(PtzError::NoMediaService)?;
                let media = client(&media_uri, &opts);
                schema::media::get_profiles(&media, &Default::default())
                    .await?
                    .profiles
                    .into_iter()
                    .find(|p| p.ptz_configuration.is_some())
                    .map(|p| p.token.0)
                    .ok_or(PtzError::NoPtzProfile)?
            }
        };

        Ok(Self {
            ptz: client(&ptz_uri, &opts),
            profile_token,
        })
    }
}

#[async_trait]
impl PtzDriver for OnvifPtz {
    async fn move_velocity(&self, v: PtzVelocity) -> Result<(), PtzError> {
        let req = schema::ptz::ContinuousMove {
            profile_token: token(&self.profile_token),
            velocity: tt::Ptzspeed {
                pan_tilt: v.pan_tilt.map(|(x, y)| tt::Vector2D {
                    x: x.clamp(-1.0, 1.0),
                    y: y.clamp(-1.0, 1.0),
                    space: None,
                }),
                zoom: v.zoom.map(|z| tt::Vector1D {
                    x: z.clamp(-1.0, 1.0),
                    space: None,
                }),
            },
            timeout: None,
        };
        schema::ptz::continuous_move(&self.ptz, &req).await?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), PtzError> {
        let req = schema::ptz::Stop {
            profile_token: token(&self.profile_token),
            pan_tilt: Some(true),
            zoom: Some(true),
        };
        schema::ptz::stop(&self.ptz, &req).await?;
        Ok(())
    }

    async fn absolute(&self, t: PtzPosition, speed: Option<f64>) -> Result<(), PtzError> {
        let req = schema::ptz::AbsoluteMove {
            profile_token: token(&self.profile_token),
            position: tt::Ptzvector {
                pan_tilt: Some(tt::Vector2D {
                    x: t.pan.clamp(-1.0, 1.0),
                    y: t.tilt.clamp(-1.0, 1.0),
                    space: None,
                }),
                zoom: Some(tt::Vector1D {
                    x: t.zoom.clamp(0.0, 1.0),
                    space: None,
                }),
            },
            speed: generic_speed(speed),
        };
        schema::ptz::absolute_move(&self.ptz, &req).await?;
        Ok(())
    }

    async fn relative(&self, d: PtzPosition, speed: Option<f64>) -> Result<(), PtzError> {
        let req = schema::ptz::RelativeMove {
            profile_token: token(&self.profile_token),
            translation: tt::Ptzvector {
                pan_tilt: Some(tt::Vector2D {
                    x: d.pan,
                    y: d.tilt,
                    space: None,
                }),
                zoom: Some(tt::Vector1D {
                    x: d.zoom,
                    space: None,
                }),
            },
            speed: generic_speed(speed),
        };
        schema::ptz::relative_move(&self.ptz, &req).await?;
        Ok(())
    }

    async fn goto_preset(&self, preset_token: &str) -> Result<(), PtzError> {
        let req = schema::ptz::GotoPreset {
            profile_token: token(&self.profile_token),
            preset_token: token(preset_token),
            speed: None,
        };
        schema::ptz::goto_preset(&self.ptz, &req).await?;
        Ok(())
    }

    async fn status(&self) -> Result<PtzState, PtzError> {
        let req = schema::ptz::GetStatus {
            profile_token: token(&self.profile_token),
        };
        let s = schema::ptz::get_status(&self.ptz, &req).await?.ptz_status;
        Ok(state_from(&s))
    }
}

fn state_from(s: &tt::Ptzstatus) -> PtzState {
    let position = s.position.as_ref().and_then(|p| {
        let pt = p.pan_tilt.as_ref()?;
        Some(PtzPosition {
            pan: pt.x,
            tilt: pt.y,
            zoom: p.zoom.as_ref().map(|z| z.x).unwrap_or(0.0),
        })
    });
    let (pan_tilt, zoom) = match &s.move_status {
        Some(m) => (move_state(&m.pan_tilt), move_state(&m.zoom)),
        None => (MoveState::Unknown, MoveState::Unknown),
    };
    PtzState {
        position,
        pan_tilt,
        zoom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_with_position() {
        // Shape returned by Hikvision / Dahua PTZ domes.
        let xml = r#"<tptz:GetStatusResponse xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
  <tptz:PTZStatus>
    <tt:Position>
      <tt:PanTilt x="0.25" y="-0.5" space="http://www.onvif.org/ver10/tptz/PanTiltSpaces/PositionGenericSpace"/>
      <tt:Zoom x="0.125" space="http://www.onvif.org/ver10/tptz/ZoomSpaces/PositionGenericSpace"/>
    </tt:Position>
    <tt:MoveStatus><tt:PanTilt>MOVING</tt:PanTilt><tt:Zoom>IDLE</tt:Zoom></tt:MoveStatus>
    <tt:UtcTime>2026-11-21T14:18:29Z</tt:UtcTime>
  </tptz:PTZStatus>
</tptz:GetStatusResponse>"#;
        let r: schema::ptz::GetStatusResponse = yaserde::de::from_str(xml).unwrap();
        let st = state_from(&r.ptz_status);
        assert_eq!(
            st.position,
            Some(PtzPosition {
                pan: 0.25,
                tilt: -0.5,
                zoom: 0.125
            })
        );
        assert_eq!(st.pan_tilt, MoveState::Moving);
        assert_eq!(st.zoom, MoveState::Idle);
        assert!(st.is_moving());
    }

    #[test]
    fn parses_status_without_position() {
        let xml = r#"<tptz:GetStatusResponse xmlns:tptz="http://www.onvif.org/ver20/ptz/wsdl" xmlns:tt="http://www.onvif.org/ver10/schema">
  <tptz:PTZStatus>
    <tt:MoveStatus><tt:PanTilt>IDLE</tt:PanTilt><tt:Zoom>IDLE</tt:Zoom></tt:MoveStatus>
    <tt:UtcTime>2026-11-21T14:18:29Z</tt:UtcTime>
  </tptz:PTZStatus>
</tptz:GetStatusResponse>"#;
        let r: schema::ptz::GetStatusResponse = yaserde::de::from_str(xml).unwrap();
        let st = state_from(&r.ptz_status);
        assert_eq!(st.position, None);
        assert!(!st.is_moving());
    }
}
