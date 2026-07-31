use std::env;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Result;
use tracing::error;
use tracing::level_filters::LevelFilter;

use crate::app::AppState;

mod app;
mod audio;
mod charts;
mod latency_test;
mod metrics;
mod overlay;
mod stats;
mod ui;
mod video;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_max_level(
            LevelFilter::from_str(&env::var("RUST_LOG").unwrap_or("INFO".into())).unwrap(),
        )
        .init();

    gstreamer::init()?;

    let state = Arc::new(AppState::new()?);

    // Networking/control logic runs on tokio, independent of the UI's own
    // (winit-driven) event loop below.
    let state_clone = state.clone();
    tokio::spawn(async move {
        if let Err(e) = app::run(state_clone).await {
            error!(error = ?e, "fatal error");
        }
    });

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1280.0, 720.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Rc car client",
        native_options,
        Box::new(move |cc| match ui::App::new(cc, state) {
            Ok(app) => Ok(Box::new(app) as Box<dyn eframe::App>),
            Err(e) => {
                error!(error = ?e, "ui building fatal error");
                Err(format!("{e:?}").into())
            }
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe run failed: {e}"))?;

    Ok(())
}
