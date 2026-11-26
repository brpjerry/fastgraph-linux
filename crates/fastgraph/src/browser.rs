//! The left pane's tab strip, and the Sets tab: the sets in the save folder
//! and the sessions earlier runs left unsaved, to open or import.
use crate::app::{App, Message};
use crate::{devices, tabs, theme};
use fastgraph_core::curve_io::local_time_string;
use fastgraph_core::set::SetHeader;
use iced::widget::{button, column, row, rule, scrollable, text};
use iced::{Alignment, Element, Length};
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

/// A tab of the left pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Pane {
    #[default]
    Measure,
    Sets,
    Settings,
}

const PANES: [(Pane, &str); 3] = [
    (Pane::Measure, "Measure"),
    (Pane::Sets, "Sets"),
    (Pane::Settings, "Settings"),
];

/// A header's `saved` seconds as local date and time, in the locale's
/// date and time format.
pub fn saved_label(secs: u64) -> String {
    local_time_string(UNIX_EPOCH + Duration::from_secs(secs))
}

fn count_label(count: usize) -> String {
    match count {
        1 => "1 measurement".into(),
        n => format!("{n} measurements"),
    }
}

/// The tab strip over the left pane.
pub fn tabs(app: &App) -> Element<'_, Message> {
    let at = tabs::position(&app.pane_slide, app.now);
    tabs::pill_tabs(&PANES, at, Length::Fill, Message::PaneSelected)
}

/// The tab index of `pane`.
pub fn index(pane: Pane) -> usize {
    PANES.iter().position(|&(p, _)| p == pane).unwrap_or(0)
}

/// One listed file: a button that opens it, and an Import button.
fn entry<'a>(path: &Path, title: String, detail: String) -> Element<'a, Message> {
    row![
        button(
            column![
                text(title).size(theme::TEXT),
                devices::caption(detail).style(theme::text(|p| p.fg_dim)),
            ]
            .spacing(2)
            .width(Length::Fill)
        )
        .on_press(Message::OpenSet(path.to_path_buf()))
        .padding([5, 8])
        .width(Length::Fill)
        .style(theme::button),
        button(text("Import"))
            .on_press(Message::ImportSet(path.to_path_buf()))
            .padding([5, 8])
            .style(theme::button),
    ]
    .spacing(4)
    .align_y(Alignment::Center)
    .into()
}

fn set_row<'a>(path: &Path, header: &SetHeader) -> Element<'a, Message> {
    let title = header.title(path);
    let detail = format!(
        "{} · {}",
        saved_label(header.saved),
        count_label(header.count)
    );
    entry(path, title, detail)
}

fn unsaved_row<'a>(path: &Path, header: &SetHeader) -> Element<'a, Message> {
    let title = format!("Unsaved · {}", saved_label(header.saved));
    let detail = if header.name.is_empty() {
        count_label(header.count)
    } else {
        format!("{} · {}", header.name, count_label(header.count))
    };
    entry(path, title, detail)
}

/// The Sets tab.
pub fn view(app: &App) -> Element<'_, Message> {
    let mut list = column![].spacing(4);
    // Nothing is said before the first listing.
    if let (true, Some(folder)) = (app.sets.is_empty(), &app.sets_folder) {
        let said = match folder {
            Some(dir) => format!("No sets in {} yet", dir.display()),
            None => "No sets folder".into(),
        };
        list = list.push(text(said).style(theme::text(|p| p.fg_dim)));
    }
    for (path, header) in &app.sets {
        list = list.push(set_row(path, header));
    }
    if !app.unsaved.is_empty() {
        list = list.push(rule::horizontal(1).style(theme::rule));
        list = list.push(devices::heading("Unsaved sessions"));
        for (path, header) in &app.unsaved {
            list = list.push(unsaved_row(path, header));
        }
    }
    scrollable(list.padding([0, 8]))
        .style(theme::scrollbar)
        .height(Length::Fill)
        .into()
}
