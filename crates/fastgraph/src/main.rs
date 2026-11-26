//! fastgraph: a small, fast headphone frequency-response tool.
//!
//! The window ([`app`]) drives the core library: the device pane picks the
//! microphone and the headphones and starts a measurement ([`devices`]), the
//! response graph shows the measurements ([`graph`]) and the list below it
//! picks which ones ([`list`]), styled by [`theme`].
mod app;
mod browser;
mod devices;
mod distortion;
mod graph;
mod list;
mod plot;
mod rta_mode;
mod settings;
mod status;
mod store_queue;
mod tabs;
#[cfg(test)]
mod testing;
mod theme;
mod toolbar;

use app::App;
use iced::window;

/// The Wayland app id, which compositors report as the window class.
const APP_ID: &str = "fastgraph";

/// `FASTGRAPH_EXIT_AFTER_FIRST_FRAME=1` quits once the first frame is drawn, to time startup:
/// `time FASTGRAPH_EXIT_AFTER_FIRST_FRAME=1 fastgraph`.
fn exit_after_first_frame() -> bool {
    static EXIT: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var_os("FASTGRAPH_EXIT_AFTER_FIRST_FRAME").is_some_and(|v| v == "1")
    });
    *EXIT
}

fn main() -> iced::Result {
    iced::application(App::boot, App::update, App::view)
        .title("fastgraph")
        .theme(App::theme)
        .settings(theme::settings())
        .subscription(App::subscription)
        .window(window::Settings {
            size: iced::Size::new(1280.0, 800.0),
            // Closing waits for the worker's pending writes (`Message::CloseRequested`).
            exit_on_close_request: false,
            platform_specific: window::settings::PlatformSpecific {
                application_id: APP_ID.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .run()
}
