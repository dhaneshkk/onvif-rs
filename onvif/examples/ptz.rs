//! PTZ control and slew-to-cue from the command line.
//!
//!   cargo run --example ptz -- --uri http://10.0.0.5/onvif/device_service \
//!       --username admin --password secret status
//!   ... move --pan 0.3 --secs 1          # continuous move, then stop
//!   ... abs --pan 0.1 --tilt 0.2 --zoom 0
//!   ... preset 1
//!   ... cue 1600 300 80 120 --frame-w 1920 --frame-h 1080 --fill 0.3

use onvif::ptz::{
    BBox, CueConfig, LensModel, OnvifPtz, OnvifPtzOptions, PtzDriver, PtzPosition, PtzVelocity,
    SlewToCue,
};
use onvif::soap::client::{AuthType, Credentials};
use std::time::Duration;
use structopt::StructOpt;
use url::Url;

#[derive(StructOpt)]
struct Args {
    /// Device-management URI.
    #[structopt(long)]
    uri: Url,
    #[structopt(long)]
    username: Option<String>,
    #[structopt(long)]
    password: Option<String>,
    /// Media profile token (default: first profile with PTZ).
    #[structopt(long)]
    profile: Option<String>,
    /// Allow WS-UsernameToken over plain HTTP (some devices/simulators need it).
    #[structopt(long)]
    allow_username_token_over_http: bool,
    /// any | digest | usernametoken
    #[structopt(long, default_value = "any")]
    auth_type: String,
    #[structopt(subcommand)]
    cmd: Cmd,
}

#[derive(StructOpt)]
enum Cmd {
    Status,
    Stop,
    /// Continuous move for `secs`, then stop.
    Move {
        #[structopt(long, default_value = "0")]
        pan: f64,
        #[structopt(long, default_value = "0")]
        tilt: f64,
        #[structopt(long, default_value = "0")]
        zoom: f64,
        #[structopt(long, default_value = "1")]
        secs: f64,
    },
    Abs {
        #[structopt(long)]
        pan: f64,
        #[structopt(long)]
        tilt: f64,
        #[structopt(long, default_value = "0")]
        zoom: f64,
    },
    Preset {
        token: String,
    },
    /// Centre a detection box (pixels) using a typical 30x lens model.
    Cue {
        x: f64,
        y: f64,
        w: f64,
        h: f64,
        #[structopt(long, default_value = "1920")]
        frame_w: f64,
        #[structopt(long, default_value = "1080")]
        frame_h: f64,
        /// Zoom so the box fills this fraction of the frame.
        #[structopt(long)]
        fill: Option<f64>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let a = Args::from_args();
    let credentials = match (a.username, a.password) {
        (Some(username), Some(password)) => Some(Credentials { username, password }),
        (None, None) => None,
        _ => return Err("username and password must be given together".into()),
    };
    let ptz = OnvifPtz::connect(
        &a.uri,
        OnvifPtzOptions {
            credentials,
            auth_type: match a.auth_type.to_ascii_lowercase().as_str() {
                "digest" => AuthType::Digest,
                "usernametoken" => AuthType::UsernameToken,
                _ => AuthType::Any,
            },
            allow_username_token_over_http: a.allow_username_token_over_http,
            profile_token: a.profile,
            ..Default::default()
        },
    )
    .await?;
    println!("profile: {}", ptz.profile_token());

    match a.cmd {
        Cmd::Status => println!("{:?}", ptz.status().await?),
        Cmd::Stop => ptz.stop().await?,
        Cmd::Move {
            pan,
            tilt,
            zoom,
            secs,
        } => {
            ptz.move_velocity(PtzVelocity {
                pan_tilt: Some((pan, tilt)),
                zoom: Some(zoom),
            })
            .await?;
            tokio::time::sleep(Duration::from_secs_f64(secs)).await;
            ptz.stop().await?;
            println!("{:?}", ptz.status().await?);
        }
        Cmd::Abs { pan, tilt, zoom } => {
            ptz.absolute(PtzPosition { pan, tilt, zoom }, None).await?;
            println!("{:?}", ptz.status().await?);
        }
        Cmd::Preset { token } => ptz.goto_preset(&token).await?,
        Cmd::Cue {
            x,
            y,
            w,
            h,
            frame_w,
            frame_h,
            fill,
        } => {
            let cue = SlewToCue::new(
                ptz,
                LensModel::typical_30x(),
                CueConfig {
                    target_fill: fill,
                    ..Default::default()
                },
            );
            println!(
                "{:?}",
                cue.cue(BBox { x, y, w, h }, frame_w, frame_h).await?
            );
            println!("{:?}", cue.driver().status().await?);
        }
    }
    Ok(())
}
