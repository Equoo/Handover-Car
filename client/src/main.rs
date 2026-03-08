
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::process::ExitCode;
use std::sync::Arc;
use std::{env, thread};
use std::str::FromStr;

use common::{ArchivedMessage, Socket, SocketEvent};
use gstreamer::{ClockTime, Registry};
use gstreamer::glib::object::{Cast, ObjectExt};
use gstreamer::{Buffer, prelude::*};
use gstreamer as gst;
use gstreamer_app as gst_app;
use gtk::prelude::*;
use gtk::{Application, ApplicationWindow};
use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use tokio::io::Join;
use tokio::join;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{error, info, warn};
use tracing::level_filters::LevelFilter;

use crate::app::AppState;

mod app;
mod ui;
mod video;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(
            LevelFilter::from_str(&env::var("RUST_LOG").unwrap_or("INFO".into())).unwrap(),
        )
        .init();

    gtk::init().unwrap();
    gstreamer::init().unwrap();

    let app = Application::builder()
        .application_id("com.rc.client")
        .build();


    // let (tx, mut rx) = mpsc::channel::<AppEvent>(32);
    let state = Arc::new(AppState::new().unwrap());

    // let tx_clone = tx.clone();
    let state_clone = state.clone();
    app.connect_activate(move |app| {
        if let Err(e) = ui::build(app, state_clone.clone()) { // , tx_clone.clone()
            error!(error = ?e, "ui building fatal error");
            app.quit();
        }
    });

    // Spawn tokio task for async work (networking, controls, etc.)
    tokio::spawn(async move {
        if let Err(e) = app::run(state).await {
            error!(error = ?e, "fatal error");
            // app_clone.quit();
        }
        // while let Some(event) = rx.recv().await {
        //     match event {
        //         AppEvent::FrameStats { latency_ms } => {
        //             info!("latency: {}ms", latency_ms);
        //         }
        //         AppEvent::Error(e) => {
        //             error!("async error: {}", e);
        //         }
        //     }
        // }
    });

    app.run();
    Ok(())
}
