use std::env;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Result;
use gtk::Application;
use gtk::prelude::*;
use tracing::error;
use tracing::level_filters::LevelFilter;

use crate::app::AppState;

mod app;
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

    gtk::init().unwrap();
    gstreamer::init().unwrap();

    let app = Application::builder()
        .application_id("com.rc.client")
        .build();

    let state = Arc::new(AppState::new().unwrap());

    let state_clone = state.clone();
    app.connect_activate(move |app| {
        if let Err(e) = ui::build(app, state_clone.clone()) {
            error!(error = ?e, "ui building fatal error");
            app.quit();
        }
    });

    // Networking/control logic runs on tokio, independent of the GTK main loop.
    tokio::spawn(async move {
        if let Err(e) = app::run(state).await {
            error!(error = ?e, "fatal error");
        }
    });

    app.run();
    Ok(())
}
