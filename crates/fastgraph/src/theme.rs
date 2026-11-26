//! The app's look, adapted from fastpeq-gui (WTFPL): a
//! true-black OLED surface with a three-step ramp, 1 px hairlines, 4 px
//! corners, flat fills and a soft-red accent kept for state and focus. No
//! shadows, gradients or blur. The light mode keeps the shapes on a
//! near-white surface with dark text.
//!
//! Every colour in the GUI is a role of [`Palette`]. The accent lives in the
//! iced [`Theme`] (its `primary`), the mode in its background, and
//! [`Palette::of`] derives the hover, pressed and dim tints from them.
//! [`Palette::from_prefs`] gives the same roles without a theme, for drawing
//! outside the window.
use fastgraph_core::prefs::Prefs;
pub use fastgraph_core::prefs::ThemeMode;
use iced::border::Radius;
use iced::widget::{
    self, checkbox, container, overlay::menu, pick_list, progress_bar, rule, scrollable,
};
use iced::{Background, Border, Color, Font, Shadow, Theme, color};
use std::cell::Cell;
use std::sync::OnceLock;

/// The default accent, a soft light red.
pub const ACCENT: Color = color!(0xe9a2a1);
/// The light mode's default accent: the same red, dark enough to read on
/// the light surfaces (as text, an edge and the first trace).
pub const ACCENT_LIGHT: Color = color!(0xa53838);

/// The default accent of `mode`.
pub fn default_accent(mode: ThemeMode) -> Color {
    match mode {
        ThemeMode::Dark => ACCENT,
        ThemeMode::Light => ACCENT_LIGHT,
    }
}

/// Corner radius of everything.
pub const RADIUS: f32 = 4.0;
/// A hairline.
pub const HAIRLINE: f32 = 1.0;

/// The type scale (small and body text), in logical pixels.
pub const TEXT_SMALL: f32 = 10.0;
pub const TEXT: f32 = 12.0;

/// How many trace colours items cycle through.
pub const TRACE_COLOURS: usize = 8;

/// The colour roles. Surfaces step up from `bg`; text steps down from `fg`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// Window background: pixels off.
    pub bg: Color,
    /// Popups: menus.
    pub bg_raised: Color,
    /// Resting fill for controls such as pickers and checkboxes.
    pub bg_overlay: Color,
    /// Text box fill.
    pub input_bg: Color,
    /// Text box outline at rest.
    pub input_border: Color,
    /// 1 px hairline between surfaces.
    pub border: Color,
    /// A hovered or focused control's border.
    pub border_focus: Color,
    /// Primary text.
    pub fg: Color,
    /// Secondary text: labels, units, captions.
    pub fg_dim: Color,
    /// Tertiary marks that are not text: minor rules, the crosshair.
    pub fg_faint: Color,
    /// Text and marks of a control that can't be used.
    pub disabled: Color,
    pub accent: Color,
    /// Text and icons on an accent fill.
    pub accent_fg: Color,
    /// The accent pressed.
    pub accent_dim: Color,
    pub hover: Color,
    pub pressed: Color,
    pub selected: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
    /// The measurement traces, the accent first.
    pub traces: [Color; TRACE_COLOURS],
}

/// The colours that differ between the modes.
struct Base {
    bg: Color,
    bg_raised: Color,
    bg_overlay: Color,
    input_bg: Color,
    input_border: Color,
    border: Color,
    fg: Color,
    fg_dim: Color,
    fg_faint: Color,
    /// How far `fg` is mixed toward `bg` for disabled text.
    disabled: f32,
    ok: Color,
    warn: Color,
    err: Color,
    /// The default traces after the accent.
    traces: [Color; TRACE_COLOURS - 1],
}

const DARK: Base = Base {
    bg: color!(0x000000),
    bg_raised: color!(0x141414),
    bg_overlay: color!(0x1e1e1e),
    input_bg: color!(0x111111),
    input_border: color!(0x3c3c3c),
    border: color!(0x2e2e2e),
    fg: color!(0xe8e8e8),
    fg_dim: color!(0xa0a0a0),
    fg_faint: color!(0x5a5a5a),
    disabled: 0.6,
    ok: color!(0x7bd88f),
    warn: color!(0xf0b072),
    err: color!(0xf07178),
    // Distinct hues, lightest first; each reads on black and none is
    // close to another.
    traces: [
        color!(0x7ac9f7),
        color!(0xf0b072),
        color!(0x7bd88f),
        color!(0xc3a6ff),
        color!(0xffd866),
        color!(0x4fd6be),
        color!(0xf78fe7),
    ],
};

/// Raised surfaces step down from the near-white background; text boxes are
/// white.
const LIGHT: Base = Base {
    bg: color!(0xf7f7f5),
    bg_raised: color!(0xeeeeeb),
    bg_overlay: color!(0xe6e6e3),
    input_bg: color!(0xffffff),
    input_border: color!(0xb8b8b8),
    border: color!(0xd0d0d0),
    fg: color!(0x1a1a1a),
    fg_dim: color!(0x5c5c5c),
    fg_faint: color!(0x9a9a9a),
    disabled: 0.5,
    ok: color!(0x1e7f3a),
    warn: color!(0xa85a00),
    err: color!(0xc62828),
    // The dark hues deepened: each has at least 3:1 on the background and
    // on a major grid line.
    traces: [
        color!(0x1f6fb5),
        color!(0xb35c00),
        color!(0x2a7f3c),
        color!(0x7b4fc9),
        color!(0x8a6d00),
        color!(0x00796b),
        color!(0xb0279a),
    ],
};

impl Palette {
    /// The palette of `mode` around `accent`; `traces` replace the trace
    /// colours (the first is then not forced to the accent).
    pub fn new(mode: ThemeMode, accent: Color, traces: Option<[Color; TRACE_COLOURS]>) -> Self {
        let base = match mode {
            ThemeMode::Dark => &DARK,
            ThemeMode::Light => &LIGHT,
        };
        let bg = base.bg;
        let fg = base.fg;
        let mut defaults = [accent; TRACE_COLOURS];
        defaults[1..].copy_from_slice(&base.traces);
        Self {
            bg,
            bg_raised: base.bg_raised,
            bg_overlay: base.bg_overlay,
            input_bg: base.input_bg,
            input_border: base.input_border,
            border: base.border,
            // The accent faded toward the background.
            border_focus: mix(accent, bg, 0.48),
            fg,
            fg_dim: base.fg_dim,
            fg_faint: base.fg_faint,
            disabled: mix(fg, bg, base.disabled),
            accent,
            // Black on a light accent, white on a dark one: whichever
            // contrasts more (they tie at a luminance of 0.179).
            accent_fg: if luminance(accent) > 0.179 {
                Color::BLACK
            } else {
                Color::WHITE
            },
            accent_dim: mix(accent, bg, 0.5),
            // The hover, pressed and selected tints: the accent at
            // 0x24, 0x3d and 0x2e over the background, flattened (see `over`).
            hover: over(bg, accent, 0x24),
            pressed: over(bg, accent, 0x3d),
            selected: over(bg, accent, 0x2e),
            ok: base.ok,
            warn: base.warn,
            err: base.err,
            traces: traces.unwrap_or(defaults),
        }
    }

    /// The palette the preferences describe: their mode, accent and trace
    /// colours, each defaulting to the mode's own.
    pub fn from_prefs(prefs: &Prefs) -> Self {
        let accent = prefs.accent.as_deref().and_then(parse_hex);
        Self::new(
            prefs.theme,
            accent.unwrap_or(default_accent(prefs.theme)),
            trace_colors(prefs),
        )
    }

    /// The palette `theme` was made with by [`with_colors`]. A theme with a
    /// light background gets the light roles.
    pub fn of(theme: &Theme) -> Self {
        let colors = theme.palette();
        let mode = mode_of(colors.background);
        let accent = colors.primary;
        let traces = TRACES
            .get()
            .and_then(|(own, traces)| (own == (mode, accent)).then_some(traces));
        Self::new(mode, accent, traces)
    }

    /// Trace colour `index`, cycling.
    pub fn trace(&self, index: usize) -> Color {
        self.traces[index % TRACE_COLOURS]
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::new(ThemeMode::Dark, ACCENT, None)
    }
}

/// The mode a background belongs to.
fn mode_of(background: Color) -> ThemeMode {
    if luminance(background) > 0.5 {
        ThemeMode::Light
    } else {
        ThemeMode::Dark
    }
}

/// The trace colours the preferences set, if they set all eight validly.
pub fn trace_colors(prefs: &Prefs) -> Option<[Color; TRACE_COLOURS]> {
    let list = prefs.trace_colors.as_ref()?;
    let colors: Vec<Color> = list.iter().map(|c| parse_hex(c)).collect::<Option<_>>()?;
    colors.try_into().ok()
}

fn mix(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// `color` at alpha `a` over `bg`, as one opaque colour, blended in sRGB.
/// iced blends in linear light, where the same alpha comes out far
/// stronger, so the tints are flattened here as an sRGB blend shows them.
fn over(bg: Color, color: Color, a: u8) -> Color {
    mix(bg, color, f32::from(a) / 255.0)
}

/// What a theme is told apart by: its mode and accent.
type ThemeKey = (ThemeMode, Color);

thread_local! {
    /// The trace colours of the theme [`with_colors`] built last on this
    /// thread (iced updates, views and draws on one thread), which
    /// [`Palette::of`] reads, for a theme with that mode and accent: an iced
    /// theme has no room for them.
    static TRACES: Cell<Option<(ThemeKey, [Color; TRACE_COLOURS])>> = const { Cell::new(None) };
}

/// A colour from `#rrggbb` or `rrggbb`, in either case.
pub fn parse_hex(text: &str) -> Option<Color> {
    let digits = text.strip_prefix('#').unwrap_or(text);
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let rgb = u32::from_str_radix(digits, 16).ok()?;
    Some(Color::from_rgb8(
        (rgb >> 16) as u8,
        (rgb >> 8) as u8,
        rgb as u8,
    ))
}

/// `#rrggbb`.
pub fn to_hex(color: Color) -> String {
    let [r, g, b, _] = color.into_rgba8();
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// The app's theme of `mode` around `accent`, with `traces` as the trace
/// colours, which [`Palette::of`] returns until the next call.
pub fn with_colors(
    mode: ThemeMode,
    accent: Color,
    traces: Option<[Color; TRACE_COLOURS]>,
) -> Theme {
    TRACES.set(traces.map(|traces| ((mode, accent), traces)));
    let palette = Palette::new(mode, accent, traces);
    Theme::custom(
        match mode {
            ThemeMode::Dark => "fastgraph",
            ThemeMode::Light => "fastgraph light",
        },
        iced::theme::Palette {
            background: palette.bg,
            text: palette.fg,
            primary: palette.accent,
            success: palette.ok,
            warning: palette.warn,
            danger: palette.err,
        },
    )
}

/// The theme the preferences describe.
pub fn from_prefs(prefs: &Prefs) -> Theme {
    let p = Palette::from_prefs(prefs);
    let traces = trace_colors(prefs);
    with_colors(prefs.theme, p.accent, traces)
}

// --- type ------------------------------------------------------------------

/// The window's settings: the UI font at the body size.
pub fn settings() -> iced::Settings {
    iced::Settings {
        default_font: ui(),
        default_text_size: TEXT.into(),
        antialiasing: true,
        ..iced::Settings::default()
    }
}

/// The preferred UI and monospace fonts.
pub const UI_FAMILY: &str = "Lato";
pub const MONO_FAMILY: &str = "JetBrainsMono Nerd Font";
/// Bundled with iced (`fira-sans`), for when a system font is missing.
pub const FALLBACK_FAMILY: &str = "Fira Sans";

/// UI text: Lato, or Fira Sans when Lato isn't installed.
pub fn ui() -> Font {
    static FONT: OnceLock<Font> = OnceLock::new();
    *FONT.get_or_init(|| pick(UI_FAMILY, installed(UI_FAMILY)))
}

/// Numbers and readouts (Hz, dB, levels): JetBrainsMono Nerd Font, or Fira
/// Sans when it isn't installed.
pub fn mono() -> Font {
    static FONT: OnceLock<Font> = OnceLock::new();
    *FONT.get_or_init(|| pick(MONO_FAMILY, installed(MONO_FAMILY)))
}

/// `family`, or the bundled fallback when it is not `available`.
fn pick(family: &'static str, available: bool) -> Font {
    Font::with_name(if available { family } else { FALLBACK_FAMILY })
}

/// Whether the system has a face of `family` (iced loads the system fonts).
fn installed(family: &str) -> bool {
    use iced::advanced::graphics::text::font_system;
    let mut system = font_system()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    system
        .raw()
        .db()
        .faces()
        .any(|face| face.families.iter().any(|(name, _)| name == family))
}

// --- text ------------------------------------------------------------------

/// A text style in the colour `role` picks, as `.style(theme::text(|p| p.fg_dim))`.
pub fn text(role: fn(&Palette) -> Color) -> impl Fn(&Theme) -> widget::text::Style {
    move |theme| widget::text::Style {
        color: Some(role(&Palette::of(theme))),
    }
}

// --- surfaces --------------------------------------------------------------

fn border(color: Color) -> Border {
    Border {
        color,
        width: HAIRLINE,
        radius: RADIUS.into(),
    }
}

/// A separator rule.
pub fn rule(theme: &Theme) -> rule::Style {
    rule::Style {
        color: Palette::of(theme).border,
        radius: Radius::default(),
        fill_mode: rule::FillMode::Full,
        snap: true,
    }
}

/// The window's own black, behind a line drawn over the grid.
pub fn backdrop(theme: &Theme) -> container::Style {
    container::Style::default().background(Palette::of(theme).bg)
}

/// A list row's fill: the selected row's tint, else none.
pub fn row_fill(selected: bool) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let p = Palette::of(theme);
        container::Style {
            background: selected.then(|| p.selected.into()),
            border: Border {
                radius: RADIUS.into(),
                ..Border::default()
            },
            ..container::Style::default()
        }
    }
}

/// A measurement's colour swatch.
pub fn swatch(index: usize) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let color = Palette::of(theme).trace(index);
        container::Style {
            background: Some(color.into()),
            border: Border {
                color,
                width: 0.0,
                radius: 2.0.into(),
            },
            ..container::Style::default()
        }
    }
}

/// The accent's swatch.
pub fn accent_swatch(theme: &Theme) -> container::Style {
    let color = Palette::of(theme).accent;
    container::Style {
        background: Some(color.into()),
        border: Border {
            radius: 2.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// A tooltip: a raised card with a hairline, primary text.
pub fn card(theme: &Theme) -> container::Style {
    let p = Palette::of(theme);
    container::Style {
        text_color: Some(p.fg),
        background: Some(p.bg_raised.into()),
        border: border(p.border),
        ..container::Style::default()
    }
}

// --- buttons ---------------------------------------------------------------

fn button_style(
    background: Option<Color>,
    text_color: Color,
    border: Border,
) -> widget::button::Style {
    widget::button::Style {
        background: background.map(Background::Color),
        text_color,
        border,
        shadow: Shadow::default(),
        snap: true,
    }
}

fn hovered(status: widget::button::Status) -> bool {
    matches!(
        status,
        widget::button::Status::Hovered | widget::button::Status::Pressed
    )
}

/// The hover or pressed tint for `status`, if any.
fn tint(p: &Palette, status: widget::button::Status) -> Option<Color> {
    match status {
        widget::button::Status::Hovered => Some(p.hover),
        widget::button::Status::Pressed => Some(p.pressed),
        _ => None,
    }
}

/// The shared text box style: its own fill, a hairline rounded outline that
/// takes the accent on focus, and the error colour for text while invalid.
pub fn input_checked(
    valid: bool,
) -> impl Fn(&Theme, widget::text_input::Status) -> widget::text_input::Style {
    move |theme, status| {
        let p = Palette::of(theme);
        // Edges no further from the background than the faint marks are
        // moved toward the text colours so they show on the fill.
        let visible_edge = |color, toward| {
            if contrast(color, p.bg) <= contrast(p.fg_faint, p.bg) {
                mix(color, toward, 0.65)
            } else {
                color
            }
        };
        let edge = match status {
            widget::text_input::Status::Focused { .. } => visible_edge(p.accent, p.fg),
            widget::text_input::Status::Hovered => visible_edge(p.border_focus, p.fg_dim),
            _ => p.input_border,
        };
        let disabled = status == widget::text_input::Status::Disabled;
        widget::text_input::Style {
            background: if disabled { p.bg } else { p.input_bg }.into(),
            border: border(edge),
            icon: p.fg_dim,
            placeholder: if disabled { p.disabled } else { p.fg_dim },
            value: if !valid {
                p.err
            } else if disabled {
                p.disabled
            } else {
                p.fg
            },
            selection: if valid {
                mix(p.input_bg, p.fg, 0.4)
            } else {
                mix(p.input_bg, p.err, 0.24)
            },
        }
    }
}

/// The text box style for boxes with no validity check.
pub fn input(theme: &Theme, status: widget::text_input::Status) -> widget::text_input::Style {
    input_checked(true)(theme, status)
}

/// A plain button: a hairline frame, secondary
/// text; hovered, the accent tint and focus edge.
pub fn button(theme: &Theme, status: widget::button::Status) -> widget::button::Style {
    let p = Palette::of(theme);
    match status {
        widget::button::Status::Disabled => button_style(None, p.disabled, border(p.border)),
        widget::button::Status::Active => button_style(None, p.fg_dim, border(p.border)),
        _ => button_style(tint(&p, status), p.fg, border(p.border_focus)),
    }
}

/// A bare text button: no frame, primary text, the accent on hover.
pub fn name_button(theme: &Theme, status: widget::button::Status) -> widget::button::Style {
    let p = Palette::of(theme);
    let color = if hovered(status) { p.accent } else { p.fg };
    button_style(None, color, Border::default())
}

/// The one action a view confirms with: accent fill, `accent_fg` text.
pub fn primary(theme: &Theme, status: widget::button::Status) -> widget::button::Style {
    let p = Palette::of(theme);
    match status {
        widget::button::Status::Disabled => button_style(None, p.disabled, border(p.border)),
        _ => {
            let fill = match status {
                widget::button::Status::Hovered => mix(p.accent, p.fg, 0.25),
                widget::button::Status::Pressed => p.accent_dim,
                _ => p.accent,
            };
            button_style(
                Some(fill),
                if status == widget::button::Status::Pressed {
                    p.fg
                } else {
                    p.accent_fg
                },
                border(fill),
            )
        }
    }
}

/// One of a row of segments: the selected
/// one has the accent edge and full-strength text.
pub fn segment(selected: bool) -> impl Fn(&Theme, widget::button::Status) -> widget::button::Style {
    move |theme, status| {
        let p = Palette::of(theme);
        if status == widget::button::Status::Disabled {
            let edge = if selected { p.accent_dim } else { p.border };
            return button_style(None, p.disabled, border(edge));
        }
        let edge = if selected {
            p.accent
        } else if hovered(status) {
            p.border_focus
        } else {
            p.border
        };
        let text = if selected || hovered(status) {
            p.fg
        } else {
            p.fg_dim
        };
        button_style(tint(&p, status), text, border(edge))
    }
}

// --- tabs ------------------------------------------------------------------

/// A fully rounded corner for a strip `crate::tabs::HEIGHT` tall.
const PILL_RADIUS: f32 = crate::tabs::HEIGHT / 2.0;

/// A tab strip's track: a raised, fully rounded fill with a hairline edge.
pub fn tab_track(theme: &Theme) -> container::Style {
    let p = Palette::of(theme);
    container::Style {
        background: Some(p.bg_overlay.into()),
        border: Border {
            color: p.border,
            width: HAIRLINE,
            radius: PILL_RADIUS.into(),
        },
        ..container::Style::default()
    }
}

/// The accent pill behind a tab strip's selected segment.
pub fn tab_pill(theme: &Theme) -> container::Style {
    let p = Palette::of(theme);
    container::Style {
        background: Some(p.accent.into()),
        border: Border {
            radius: PILL_RADIUS.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// A tab's label: `accent_fg` on the pill, else secondary text that
/// brightens on hover. The segment itself has no fill: the pill is the fill.
pub fn tab_label(
    on_pill: bool,
) -> impl Fn(&Theme, widget::button::Status) -> widget::button::Style {
    move |theme, status| {
        let p = Palette::of(theme);
        let color = if on_pill {
            p.accent_fg
        } else if hovered(status) {
            p.fg
        } else {
            p.fg_dim
        };
        button_style(None, color, Border::default())
    }
}

// --- inputs ----------------------------------------------------------------

/// A drop-down, framed like a field.
pub fn picker(theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    let p = Palette::of(theme);
    let edge = match status {
        pick_list::Status::Active => p.border,
        pick_list::Status::Hovered | pick_list::Status::Opened { .. } => p.border_focus,
    };
    pick_list::Style {
        text_color: p.fg,
        placeholder_color: p.fg_dim,
        handle_color: p.fg_dim,
        background: Background::Color(p.bg_overlay),
        border: border(edge),
    }
}

/// A drop-down's open menu: a raised popup with a hairline, the hovered
/// option in the accent tint.
pub fn menu(theme: &Theme) -> menu::Style {
    let p = Palette::of(theme);
    menu::Style {
        background: Background::Color(p.bg_raised),
        border: border(p.border),
        text_color: p.fg,
        selected_text_color: p.fg,
        selected_background: Background::Color(p.selected),
        shadow: Shadow::default(),
    }
}

/// A checkbox: a hairline box on the overlay step, the accent when checked.
pub fn check(theme: &Theme, status: checkbox::Status) -> checkbox::Style {
    let p = Palette::of(theme);
    let (checked, hovered, disabled) = match status {
        checkbox::Status::Active { is_checked } => (is_checked, false, false),
        checkbox::Status::Hovered { is_checked } => (is_checked, true, false),
        checkbox::Status::Disabled { is_checked } => (is_checked, false, true),
    };
    let (fill, edge) = match (checked, disabled) {
        (_, true) => (p.bg_overlay, p.border),
        (true, false) => (p.accent, p.accent),
        (false, false) => (
            p.bg_overlay,
            if hovered { p.border_focus } else { p.border },
        ),
    };
    checkbox::Style {
        background: Background::Color(fill),
        icon_color: p.accent_fg,
        border: border(edge),
        text_color: Some(if disabled { p.disabled } else { p.fg }),
    }
}

/// A slider: a 4 px track on the overlay step,
/// filled in the accent, and a round handle.
pub fn slider(theme: &Theme, status: widget::slider::Status) -> widget::slider::Style {
    let p = Palette::of(theme);
    let handle = match status {
        widget::slider::Status::Hovered | widget::slider::Status::Dragged => p.fg,
        widget::slider::Status::Active => p.fg_dim,
    };
    widget::slider::Style {
        rail: widget::slider::Rail {
            backgrounds: (Background::Color(p.accent), Background::Color(p.bg_overlay)),
            width: 4.0,
            border: Border {
                radius: RADIUS.into(),
                ..Border::default()
            },
        },
        handle: widget::slider::Handle {
            shape: widget::slider::HandleShape::Circle { radius: 6.0 },
            background: Background::Color(handle),
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
        },
    }
}

/// The input meter: green, red while a clip is held.
pub fn meter(clipped: bool) -> impl Fn(&Theme) -> progress_bar::Style {
    move |theme| {
        let p = Palette::of(theme);
        progress_bar::Style {
            background: Background::Color(p.bg_overlay),
            bar: Background::Color(if clipped { p.err } else { p.ok }),
            border: Border {
                radius: 2.0.into(),
                ..Border::default()
            },
        }
    }
}

/// A thin scrollbar: no rail, a hairline-coloured scroller.
pub fn scrollbar(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let p = Palette::of(theme);
    let mut style = scrollable::default(theme, status);
    let active = matches!(
        status,
        scrollable::Status::Hovered {
            is_vertical_scrollbar_hovered: true,
            ..
        } | scrollable::Status::Dragged {
            is_vertical_scrollbar_dragged: true,
            ..
        }
    );
    let rail = scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller: scrollable::Scroller {
            background: Background::Color(if active { p.fg_faint } else { p.border }),
            border: Border {
                radius: RADIUS.into(),
                ..Border::default()
            },
        },
    };
    style.vertical_rail = rail;
    style.horizontal_rail = rail;
    style
}

/// WCAG relative luminance.
fn luminance(c: Color) -> f32 {
    let channel = |v: f32| {
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
}

/// WCAG contrast ratio, from 1 to 21.
fn contrast(a: Color, b: Color) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODES: [ThemeMode; 2] = [ThemeMode::Dark, ThemeMode::Light];

    /// The app's theme of `mode` with its default colours.
    fn default_theme(mode: ThemeMode) -> Theme {
        with_colors(mode, default_accent(mode), None)
    }

    // In both modes, including the light mode's resting border on white.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn text_boxes_are_outlined_and_filled_apart_from_buttons_and_background() {
        for mode in MODES {
            let theme = default_theme(mode);
            let p = Palette::of(&theme);
            let button_fills = [
                widget::button::Status::Active,
                widget::button::Status::Hovered,
                widget::button::Status::Pressed,
            ]
            .map(|s| button(&theme, s).background);
            for status in [
                widget::text_input::Status::Active,
                widget::text_input::Status::Hovered,
                widget::text_input::Status::Focused { is_hovered: false },
            ] {
                let style = input(&theme, status);
                assert!(style.border.width > 0.0);
                assert_eq!(style.background, Background::Color(p.input_bg));
                assert_eq!(p.input_bg.a, 1.0);
                assert_eq!(style.border.color.a, 1.0);
                assert_eq!(style.border.width, HAIRLINE);
                assert_eq!(style.border.radius, RADIUS.into());
                let fill = style.background;
                assert_ne!(fill, p.bg.into());
                for b in button_fills {
                    assert_ne!(Some(fill), b);
                }
                for fill_of in [p.hover, p.pressed, p.selected, p.accent_dim, p.accent] {
                    assert_ne!(fill, fill_of.into());
                }
                let ratio = contrast(style.border.color, p.input_bg);
                assert!(ratio >= 1.5, "{mode:?} {status:?}: {ratio:.2}");
            }
            let focused = input(
                &theme,
                widget::text_input::Status::Focused { is_hovered: false },
            );
            assert_eq!(focused.border.color, p.accent);
            let resting = input(&theme, widget::text_input::Status::Active);
            assert_eq!(resting.border.color, p.input_border);
            // The outline also sets the box apart from the window around it.
            assert!(contrast(resting.border.color, p.bg) >= 1.5, "{mode:?}");
            let invalid = input_checked(false)(&theme, widget::text_input::Status::Active);
            assert_eq!(invalid.value, p.err);
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn text_status_and_trace_colours_read_on_their_surfaces() {
        for mode in MODES {
            let p = Palette::of(&default_theme(mode));
            for surface in [p.bg, p.bg_raised, p.bg_overlay, p.hover, p.selected] {
                for (role, color) in [("fg", p.fg), ("fg_dim", p.fg_dim), ("accent", p.accent)] {
                    let ratio = contrast(color, surface);
                    assert!(ratio >= 4.5, "{mode:?} {role}: {ratio:.2}");
                }
                for color in [p.ok, p.warn, p.err] {
                    assert!(contrast(color, surface) >= 3.0, "{mode:?}");
                }
            }
            // Each trace stands out from the grid lines it crosses.
            for (i, &trace) in p.traces.iter().enumerate() {
                let ratio = contrast(trace, p.border);
                assert!(
                    ratio >= 3.0,
                    "{mode:?} trace {i} on a grid line: {ratio:.2}"
                );
            }
            assert!(contrast(p.accent_fg, p.accent) >= 4.5, "{mode:?}");
            // Disabled text is fainter than secondary text but still there.
            let disabled = contrast(p.disabled, p.bg);
            assert!(disabled >= 2.5 && disabled < contrast(p.fg_dim, p.bg));
        }
        // Traces on black stand out by more.
        let dark = Palette::default();
        for (i, &trace) in dark.traces.iter().enumerate() {
            assert!(contrast(trace, dark.bg) >= 4.5, "trace {i}");
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn light_default_traces_have_three_to_one_on_the_light_surfaces() {
        let p = Palette::new(ThemeMode::Light, ACCENT_LIGHT, None);
        assert_eq!(p.trace(0), ACCENT_LIGHT);
        for (i, &trace) in p.traces.iter().enumerate() {
            for surface in [p.bg, p.bg_raised, p.input_bg] {
                let ratio = contrast(trace, surface);
                assert!(ratio >= 3.0, "trace {i} {}: {ratio:.2}", to_hex(trace));
            }
        }
        // The dark mode's default accent would not.
        assert!(contrast(ACCENT, p.bg) < 3.0);
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn text_box_text_selection_and_placeholder_read_on_the_fill() {
        for mode in MODES {
            let theme = default_theme(mode);
            let p = Palette::of(&theme);
            let active = widget::text_input::Status::Active;
            let ok = input(&theme, active);
            assert!(contrast(ok.selection, p.input_bg) >= 1.5, "{mode:?}");
            assert!(contrast(ok.value, ok.selection) >= 3.0, "{mode:?}");
            let bad = input_checked(false)(&theme, active);
            assert!(contrast(bad.selection, p.input_bg) >= 1.2, "{mode:?}");
            assert!(contrast(bad.value, bad.selection) >= 3.0, "{mode:?}");
            assert_eq!(ok.placeholder, p.fg_dim);
            assert!(contrast(ok.placeholder, p.input_bg) >= 4.5, "{mode:?}");
            let off = input(&theme, widget::text_input::Status::Disabled);
            assert_eq!(off.placeholder, p.disabled);
        }
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn a_disabled_text_box_uses_the_window_fill_and_keeps_the_error_colour() {
        for mode in MODES {
            let theme = default_theme(mode);
            let p = Palette::of(&theme);
            let off = input(&theme, widget::text_input::Status::Disabled);
            assert_eq!(off.background, Background::Color(p.bg));
            assert_eq!(off.value, p.disabled);
            let bad = input_checked(false)(&theme, widget::text_input::Status::Disabled);
            assert_eq!(bad.background, Background::Color(p.bg));
            assert_eq!(bad.value, p.err);
        }
    }

    /// A text box's hover and focus edges under `theme`.
    fn edges(theme: &Theme) -> (Color, Color) {
        let hover = input(theme, widget::text_input::Status::Hovered)
            .border
            .color;
        let focus = input(
            theme,
            widget::text_input::Status::Focused { is_hovered: false },
        )
        .border
        .color;
        (hover, focus)
    }

    // Bugs caught: 1, False positives: 0
    #[test]
    fn hover_and_focus_edges_show_on_a_dark_accent() {
        let theme = with_colors(ThemeMode::Dark, parse_hex("#000080").unwrap(), None);
        let p = Palette::of(&theme);
        let (hover, focus) = edges(&theme);
        assert!(contrast(hover, p.input_bg) >= 1.5, "hover");
        assert!(contrast(focus, p.input_bg) >= 1.5, "focus");
        assert!(luminance(focus) > luminance(hover));
    }

    // A pale custom accent on the light mode's white boxes.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn hover_and_focus_edges_show_on_a_pale_accent_in_light_mode() {
        for hex in ["#ffff00", "#ffffff", "#e9a2a1"] {
            let theme = with_colors(ThemeMode::Light, parse_hex(hex).unwrap(), None);
            let p = Palette::of(&theme);
            let (hover, focus) = edges(&theme);
            assert!(contrast(hover, p.input_bg) >= 1.5, "{hex} hover");
            assert!(contrast(focus, p.input_bg) >= 1.5, "{hex} focus");
            assert!(luminance(focus) < luminance(hover), "{hex}");
        }
    }

    // A dark custom accent still carries readable labels and checkmarks.
    // Bugs caught: 1, False positives: 0
    #[test]
    fn text_on_the_accent_reads_for_any_accent() {
        for mode in MODES {
            for hex in ["#000080", "#000000", "#ffffff", "#ffff00", "#808080"] {
                let p = Palette::new(mode, parse_hex(hex).unwrap(), None);
                let ratio = contrast(p.accent_fg, p.accent);
                assert!(ratio >= 4.5, "{hex}: {ratio:.2}");
            }
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn trace_colours_cycle_from_the_accent_and_are_distinct() {
        for mode in MODES {
            let p = Palette::new(mode, default_accent(mode), None);
            assert_eq!(p.trace(0), p.accent);
            assert_eq!(p.trace(TRACE_COLOURS), p.accent);
            for i in 0..TRACE_COLOURS {
                for j in i + 1..TRACE_COLOURS {
                    let (a, b) = (p.traces[i], p.traces[j]);
                    let d = (a.r - b.r).abs() + (a.g - b.g).abs() + (a.b - b.b).abs();
                    assert!(d > 0.2, "{mode:?}: traces {i} and {j} are too close");
                }
            }
        }
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_foreign_theme_gets_the_default_traces() {
        let green = parse_hex("#00ff00").unwrap();
        let mut traces = Palette::default().traces;
        traces[3] = green;
        let custom = with_colors(ThemeMode::Dark, ACCENT, Some(traces));
        assert_eq!(Palette::of(&custom).trace(3), green);
        // Another theme does not get them, though they were set last.
        let p = Palette::of(&Theme::Dark);
        assert_eq!(
            p.traces,
            Palette::new(ThemeMode::Dark, p.accent, None).traces
        );
        assert_eq!(Palette::of(&custom).trace(3), green);
        // Nor does the other mode with the same accent.
        let light = with_colors(ThemeMode::Light, ACCENT, None);
        assert_eq!(
            Palette::of(&light).traces,
            Palette::new(ThemeMode::Light, ACCENT, None).traces
        );
        // Built without custom traces, the theme has the defaults.
        let plain = with_colors(ThemeMode::Dark, ACCENT, None);
        assert_eq!(Palette::of(&plain).traces, Palette::default().traces);
    }

    // The mode survives the trip through an iced theme, and preferences
    // give the palette without one.
    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_palette_of_a_theme_matches_the_preferences_it_was_built_from() {
        for mode in MODES {
            for accent in [None, Some("#30a0f0".to_string())] {
                for traces in [None, Some(vec!["#123456".to_string(); TRACE_COLOURS])] {
                    let prefs = Prefs {
                        theme: mode,
                        accent: accent.clone(),
                        trace_colors: traces,
                        ..Prefs::default()
                    };
                    let want = Palette::from_prefs(&prefs);
                    assert_eq!(Palette::of(&from_prefs(&prefs)), want, "{mode:?}");
                }
            }
        }
        let light = Palette::from_prefs(&Prefs {
            theme: ThemeMode::Light,
            ..Prefs::default()
        });
        assert_eq!(light.bg, LIGHT.bg);
        assert_eq!(light.accent, ACCENT_LIGHT);
        assert_eq!(Palette::from_prefs(&Prefs::default()), Palette::default());
        assert_eq!(Palette::of(&Theme::Light).bg, LIGHT.bg);
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn a_missing_system_font_falls_back_to_the_bundled_fira_sans() {
        assert_eq!(pick(UI_FAMILY, true), Font::with_name("Lato"));
        assert_eq!(pick(MONO_FAMILY, false), Font::with_name("Fira Sans"));
        assert!(!installed("No Such Font Family"));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn no_colour_literals_outside_the_theme() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let banned = [
            "color!(",
            "Color::from_rgb",
            "Color::from_linear",
            "Color::new(",
            "Color::parse",
            "Color::WHITE",
            "Color::BLACK",
            "rgb8(",
        ];
        let theme_file = dir.join("theme.rs");
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path == theme_file || path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            for (n, line) in source.lines().enumerate() {
                for pattern in banned {
                    if line.contains(pattern) {
                        found.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
                    }
                }
            }
        }
        assert!(found.is_empty(), "colour literals:\n{}", found.join("\n"));
    }
}
