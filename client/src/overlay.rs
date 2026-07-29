//! The on-screen stats panel and its keyboard-driven help panel.
//!
//! Everything shown here comes straight from the `METRICS` registry in
//! `metrics.rs`: one label per entry, shown/hidden per `Visibility`, and the
//! same list doubles as the content of the help panel so the two can never
//! drift apart.

use std::sync::Arc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{ApplicationWindow, Box as GtkBox, EventControllerKey, Label, Orientation, Overlay};

use crate::app::AppState;
use crate::metrics::{HELP_KEY, HIDE_ALL_KEY, METRICS, SHOW_ALL_KEY, Visibility};

const CSS: &str = "
box.stats-panel, box.help-panel {
    background-color: rgba(0, 0, 0, 0.7);
    border-radius: 10px;
    padding: 15px;
}
box.stats-panel label, box.help-panel label {
    color: white;
    font-size: 16px;
    font-weight: bold;
    font-family: monospace;
}
box.help-panel label {
    font-size: 13px;
    font-weight: normal;
}
";

fn build_stats_box() -> (GtkBox, Vec<Label>) {
    let stats_box = GtkBox::new(Orientation::Vertical, 5);
    stats_box.add_css_class("stats-panel");
    stats_box.set_halign(gtk::Align::End);
    stats_box.set_valign(gtk::Align::Start);
    stats_box.set_margin_top(10);
    stats_box.set_margin_end(10);

    let labels: Vec<Label> = METRICS
        .iter()
        .map(|_| {
            let label = Label::new(None);
            label.set_halign(gtk::Align::Start);
            label
        })
        .collect();
    for label in &labels {
        stats_box.append(label);
    }

    (stats_box, labels)
}

fn build_help_box() -> GtkBox {
    let help_box = GtkBox::new(Orientation::Vertical, 3);
    help_box.add_css_class("help-panel");
    help_box.set_halign(gtk::Align::Start);
    help_box.set_valign(gtk::Align::End);
    help_box.set_margin_start(10);
    help_box.set_margin_bottom(10);
    help_box.set_visible(false);

    let add_line = |text: String| {
        let line = Label::new(Some(&text));
        line.set_halign(gtk::Align::Start);
        help_box.append(&line);
    };

    add_line(format!("[{HELP_KEY}] toggle this help"));
    add_line(format!("[{SHOW_ALL_KEY}] show all metrics   [{HIDE_ALL_KEY}] hide all metrics"));
    for metric in METRICS {
        add_line(format!("[{}] {}", metric.key, metric.label));
    }

    help_box
}

/// Wraps `picture` in an `Overlay` carrying the stats panel and help panel,
/// and attaches the key controller that drives `Visibility`. `window` is
/// where the key controller is installed (key events bubble up to it
/// regardless of which child widget has focus).
pub fn build(window: &ApplicationWindow, picture: &gtk::Picture, state: Arc<AppState>) -> Overlay {
    let overlay = Overlay::new();
    overlay.set_child(Some(picture));

    let (stats_box, labels) = build_stats_box();
    let help_box = build_help_box();

    let css_provider = gtk::CssProvider::new();
    css_provider.load_from_data(CSS);
    gtk::style_context_add_provider_for_display(
        &gdk::Display::default().unwrap(),
        &css_provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    overlay.add_overlay(&stats_box);
    overlay.add_overlay(&help_box);

    let visibility = Arc::new(Visibility::new());

    let key_controller = EventControllerKey::new();
    let visibility_for_keys = visibility.clone();
    let help_box_for_keys = help_box.clone();
    key_controller.connect_key_pressed(move |_, keyval, _keycode, _modifiers| {
        if let Some(c) = keyval.to_unicode()
            && visibility_for_keys.handle_key(c)
        {
            help_box_for_keys.set_visible(visibility_for_keys.help_visible());
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_controller);

    glib::timeout_add_local(Duration::from_millis(100), move || {
        for (label, metric) in labels.iter().zip(METRICS.iter()) {
            let visible = visibility.is_enabled(metric.id);
            label.set_visible(visible);
            if visible {
                label.set_text(&(metric.format)(&state.stats));
            }
        }
        glib::ControlFlow::Continue
    });

    overlay
}
