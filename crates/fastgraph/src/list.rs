//! The measurement list under the graph: a row per item with its shown
//! checkbox, colour swatch, name and kind/side caption, and its offset, Rename,
//! Export and Delete actions.
use crate::app::{App, Item, Message, side_label};
use crate::theme;
use fastgraph_core::store::Kind;
use iced::widget::{button, checkbox, column, container, row, scrollable, space, text, text_input};
use iced::{Alignment, Element, Length, Task};

/// The list's height under the graph.
pub const HEIGHT: f32 = 200.0;

/// The rename field's id, for focusing it.
const RENAME_FIELD: &str = "rename";

pub fn view(app: &App) -> Element<'_, Message> {
    container(column![items(app), bar(app)])
        .height(Length::Fixed(HEIGHT))
        .into()
}

/// The bottom bar: Clear, right-aligned, always visible.
fn bar(app: &App) -> Element<'_, Message> {
    let clear = button(text("Clear").size(theme::TEXT_SMALL))
        .on_press_maybe((!app.is_pristine()).then_some(Message::Clear))
        .padding([2, 8])
        .style(theme::button);
    row![space::horizontal(), clear]
        .padding([4, 6])
        .align_y(Alignment::Center)
        .into()
}

fn items(app: &App) -> Element<'_, Message> {
    if app.items.is_empty() {
        return container(
            text("Measurements appear here")
                .size(theme::TEXT_SMALL)
                .style(theme::text(|p| p.fg_dim)),
        )
        .padding([10, 6])
        .height(Length::Fill)
        .into();
    }
    let rows = app.items.iter().map(|item| item_row(app, item));
    scrollable(column(rows).spacing(2).padding([6, 6]))
        .style(theme::scrollbar)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// "Sweep · L", "Imported", "Average · comp coupler.txt" (the compensation
/// built into an average).
pub fn caption(item: &Item) -> String {
    let meta = &item.m.meta;
    let kind = match meta.kind {
        Kind::Sweep => "Sweep",
        Kind::Rta => "RTA",
        Kind::Imported => "Imported",
        Kind::Average => "Average",
    };
    let mut caption = match meta.side {
        Some(side) => format!("{kind} · {}", side_label(side)),
        None => kind.into(),
    };
    if let Some(baked) = &meta.baked_compensation {
        caption.push_str(&format!(" · comp {baked}"));
    }
    caption
}

fn item_row<'a>(app: &'a App, item: &'a Item) -> Element<'a, Message> {
    let id = item.m.meta.id.clone();
    let shown = checkbox(item.shown)
        .on_toggle(move |on| Message::Shown(id.clone(), on))
        .size(14)
        .style(theme::check);
    let swatch = container(space())
        .width(Length::Fixed(14.0))
        .height(Length::Fixed(3.0))
        .style(theme::swatch(item.color));
    let mut line = row![
        shown,
        swatch,
        name(app, item),
        text(caption(item))
            .size(theme::TEXT_SMALL)
            .style(theme::text(|p| p.fg_dim)),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    if item.m.meta.clipped {
        line = line.push(
            text("input clipped")
                .size(theme::TEXT_SMALL)
                .style(theme::text(|p| p.warn)),
        );
    }
    line = line.push(space::horizontal());
    let id = &item.m.meta.id;
    line = line
        .push(
            button(text("−").size(theme::TEXT_SMALL))
                .on_press(Message::OffsetStep(id.clone(), -0.5))
                .padding([2, 6])
                .style(theme::button),
        )
        .push(offset_box(app, item))
        .push(
            button(text("+").size(theme::TEXT_SMALL))
                .on_press(Message::OffsetStep(id.clone(), 0.5))
                .padding([2, 6])
                .style(theme::button),
        );
    for (label, message) in [
        ("Rename", Message::RenameStart(id.clone())),
        ("Export…", Message::Export(id.clone())),
        ("Delete", Message::Delete(id.clone())),
    ] {
        line = line.push(
            button(text(label).size(theme::TEXT_SMALL))
                .on_press(message)
                .padding([2, 8])
                .style(theme::button),
        );
    }
    let selected = app.selected.as_ref() == Some(&item.m.meta.id);
    container(line)
        .padding([3, 4])
        .style(theme::row_fill(selected))
        .into()
}

/// The item's name, or its field while it is being renamed.
fn name<'a>(app: &'a App, item: &'a Item) -> Element<'a, Message> {
    match &app.renaming {
        Some((id, input)) if *id == item.m.meta.id => text_input("Name", input)
            .on_input(Message::RenameInput)
            .on_submit(Message::RenameCommit)
            .id(RENAME_FIELD)
            .size(theme::TEXT)
            .padding([1, 4])
            .width(Length::Fixed(140.0))
            .into(),
        _ => button(text(&item.m.meta.name))
            .on_press(Message::Select(item.m.meta.id.clone()))
            .padding(0)
            .width(Length::Fixed(140.0))
            .style(theme::name_button)
            .into(),
    }
}

/// The offset as the box shows it: `+1.5`, `-0.5`, `0.0`.
pub fn format_offset(db: f64) -> String {
    if db.abs() < 0.05 {
        "0.0".into()
    } else {
        format!("{db:+.1}")
    }
}

/// Offset text such as `1.5`, `+1.5`, `-2` or `1,5`, clamped to the limit.
pub fn parse_offset(text: &str) -> Option<f64> {
    let db: f64 = text.trim().replace(',', ".").parse().ok()?;
    db.is_finite().then(|| round_offset(db))
}

/// Clamp to the limit and round to 0.1 dB (half away from zero), so the box,
/// the trace and the export header agree.
pub fn round_offset(db: f64) -> f64 {
    (db.clamp(-OFFSET_LIMIT, OFFSET_LIMIT) * 10.0).round() / 10.0
}

/// The most an offset can shift a trace, in dB.
const OFFSET_LIMIT: f64 = 60.0;

/// The offset box, with the error colour while its text does not parse.
fn offset_box<'a>(app: &'a App, item: &'a Item) -> Element<'a, Message> {
    let id = &item.m.meta.id;
    let value = app
        .offset_texts
        .get(id)
        .cloned()
        .unwrap_or_else(|| format_offset(item.m.meta.offset_db));
    let valid = parse_offset(&value).is_some();
    let (input, submit) = (id.clone(), id.clone());
    text_input("0.0", &value)
        .on_input(move |t| Message::OffsetInput(input.clone(), t))
        .on_submit(Message::OffsetSubmit(submit))
        .size(theme::TEXT_SMALL)
        .padding([1, 4])
        .width(Length::Fixed(52.0))
        .style(theme::input_checked(valid))
        .into()
}

impl App {
    pub(crate) fn offset_step(&mut self, id: &str, delta: f64) {
        let Some(item) = self.items.iter().find(|i| i.m.meta.id == id) else {
            return;
        };
        let db = round_offset(item.m.meta.offset_db + delta);
        self.offset_texts.remove(id);
        self.set_offset(id, db);
    }

    /// Enter in an offset box: apply valid text, keep invalid as typed.
    pub(crate) fn offset_submit(&mut self, id: &str) {
        let Some(db) = self.offset_texts.get(id).and_then(|t| parse_offset(t)) else {
            return;
        };
        self.offset_texts.remove(id);
        self.set_offset(id, db);
    }

    fn set_offset(&mut self, id: &str, db: f64) {
        let Some(item) = self.items.iter_mut().find(|i| i.m.meta.id == id) else {
            return;
        };
        if item.m.meta.offset_db == db {
            return;
        }
        item.m.meta.offset_db = db;
        self.redraw();
        self.session_changed();
    }

    pub(crate) fn rename_start(&mut self, id: &str) -> Task<Message> {
        let Some(item) = self.items.iter().find(|i| i.m.meta.id == id) else {
            return Task::none();
        };
        self.renaming = Some((id.to_string(), item.m.meta.name.clone()));
        iced::widget::operation::focus(RENAME_FIELD)
    }

    pub(crate) fn rename_input(&mut self, name: String) {
        if let Some((_, input)) = &mut self.renaming {
            *input = name;
        }
    }

    /// Keep the typed name; an empty one is rejected and the old name kept.
    pub(crate) fn rename_commit(&mut self) -> Task<Message> {
        let Some((id, input)) = self.renaming.take() else {
            return Task::none();
        };
        let name = input.trim().to_string();
        if name.is_empty() {
            self.warn("A name can't be empty".into());
            return Task::none();
        }
        let Some(item) = self.items.iter_mut().find(|i| i.m.meta.id == id) else {
            return Task::none();
        };
        item.m.meta.name = name;
        self.redraw();
        self.session_changed();
        Task::none()
    }

    /// Show or hide an item and keep that in its metadata.
    pub(crate) fn set_shown(&mut self, id: &str, shown: bool) -> Task<Message> {
        let Some(item) = self.items.iter_mut().find(|i| i.m.meta.id == id) else {
            return Task::none();
        };
        item.shown = shown;
        item.m.meta.shown = shown;
        self.redraw();
        self.session_changed();
        Task::none()
    }

    pub(crate) fn delete(&mut self, id: &str) -> Task<Message> {
        let Some(index) = self.items.iter().position(|i| i.m.meta.id == id) else {
            return Task::none();
        };
        self.items.remove(index);
        self.distortion.remove(id);
        if self.selected.as_deref() == Some(id) {
            self.selected = None;
        }
        self.offset_texts.remove(id);
        if self.renaming.as_ref().is_some_and(|(r, _)| r == id) {
            self.renaming = None;
        }
        self.redraw();
        self.session_changed();
        Task::none()
    }
}
