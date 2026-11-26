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
mod export_png;
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
mod zoom;

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

/// The answer to `--version` or `--help`, printed without opening a window or
/// connecting to the audio server; `None` starts the app.
fn cli(args: impl IntoIterator<Item = String>) -> Option<String> {
    match args.into_iter().nth(1).as_deref() {
        Some("--version" | "-V") => Some(format!("fastgraph {}", env!("CARGO_PKG_VERSION"))),
        Some("--help" | "-h") => Some(format!(
            "fastgraph {}\nHeadphone frequency-response measurement for PipeWire.\n\n\
             Usage: fastgraph [--version | --help]\n\n\
             Run without options to open the window (needs Wayland and PipeWire).",
            env!("CARGO_PKG_VERSION")
        )),
        _ => None,
    }
}

fn main() -> iced::Result {
    if let Some(text) = cli(std::env::args()) {
        println!("{text}");
        return Ok(());
    }
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

#[cfg(test)]
mod tests {
    use super::cli;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| a.to_string()).collect()
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn version_and_help_answer_without_starting_the_app() {
        let version = cli(args(&["fastgraph", "--version"])).unwrap();
        assert_eq!(version, format!("fastgraph {}", env!("CARGO_PKG_VERSION")));
        assert_eq!(cli(args(&["fastgraph", "-V"])), Some(version));
        assert!(
            cli(args(&["fastgraph", "--help"]))
                .unwrap()
                .contains("Usage: fastgraph")
        );
        assert!(cli(args(&["fastgraph", "-h"])).is_some());
        assert_eq!(cli(args(&["fastgraph"])), None);
    }
}
