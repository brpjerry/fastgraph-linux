//! A segmented tab strip: a rounded track of equal segments, with an accent
//! pill behind the selected one that slides to a new selection.
use crate::theme;
use iced::animation::{Animation, Easing};
use iced::widget::{Space, button, container, row, stack, text};
use iced::{Element, Length};
use std::time::{Duration, Instant};

/// The strip's height, in logical pixels.
pub const HEIGHT: f32 = 28.0;
/// The gap between the track's edge and the pill.
const INSET: f32 = 2.0;
/// How long the pill takes to reach a new selection.
const SLIDE: Duration = Duration::from_millis(160);
/// Fill portions per segment: the pill's position resolution.
const RESOLUTION: f32 = 1000.0;

/// The pill's position, in segments from the first, resting on `index`.
pub fn slide(index: usize) -> Animation<f32> {
    Animation::new(index as f32)
        .duration(SLIDE)
        .easing(Easing::EaseOutCubic)
}

/// Send the pill of `slide` to segment `index` from `now`.
pub fn go(slide: &mut Animation<f32>, index: usize, now: Instant) {
    slide.go_mut(index as f32, now);
}

/// The pill's position at `now`.
pub fn position(slide: &Animation<f32>, now: Instant) -> f32 {
    slide.interpolate_with(|v| v, now)
}

/// The fill portions of the space before the pill and after it, for a pill at
/// `position` in a strip of `count` segments.
fn portions(position: f32, count: usize) -> (u16, u16) {
    let last = count.saturating_sub(1) as f32;
    let at = position.clamp(0.0, last);
    (
        (at * RESOLUTION).round() as u16,
        ((last - at) * RESOLUTION).round() as u16,
    )
}

/// A tab strip of `items` `width` wide, whose pill sits at `position` (see
/// [`position`]); a click on a segment sends `on_select` of its value.
pub fn pill_tabs<'a, T: Copy, M: Clone + 'a>(
    items: &[(T, &'a str)],
    position: f32,
    width: Length,
    on_select: impl Fn(T) -> M,
) -> Element<'a, M> {
    let (before, after) = portions(position, items.len());
    let mut track = row![];
    if before > 0 {
        track = track.push(Space::new().width(Length::FillPortion(before)));
    }
    track = track.push(
        container(Space::new())
            .width(Length::FillPortion(RESOLUTION as u16))
            .height(Length::Fill)
            .style(theme::tab_pill),
    );
    if after > 0 {
        track = track.push(Space::new().width(Length::FillPortion(after)));
    }
    let labels = row(items.iter().enumerate().map(|(i, &(value, label))| {
        // A label reads on the pill while the pill covers most of it.
        let covered = (position - i as f32).abs() < 0.5;
        button(
            text(label)
                .center()
                .width(Length::Fill)
                .height(Length::Fill),
        )
        .on_press(on_select(value))
        .padding([0, 8])
        .width(Length::Fill)
        .height(Length::Fill)
        .style(theme::tab_label(covered))
        .into()
    }));
    container(stack![track, labels])
        .padding(INSET)
        .width(width)
        .height(HEIGHT)
        .style(theme::tab_track)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_pill_splits_the_rest_of_the_track() {
        assert_eq!(portions(0.0, 3), (0, 2000));
        assert_eq!(portions(2.0, 3), (2000, 0));
        assert_eq!(portions(0.5, 3), (500, 1500));
        // An overshooting ease stays on the track.
        assert_eq!(portions(-0.1, 3), (0, 2000));
        assert_eq!(portions(2.2, 3), (2000, 0));
    }

    // Bugs caught: 0, False positives: 0
    #[test]
    fn the_slide_ends_on_the_new_tab() {
        let start = Instant::now();
        let mut s = slide(0);
        go(&mut s, 2, start);
        assert!(s.is_animating(start));
        assert!((position(&s, start) - 0.0).abs() < 1e-3);
        let mid = position(&s, start + SLIDE / 2);
        assert!(mid > 1.0 && mid < 2.0, "{mid}");
        let end = start + SLIDE + Duration::from_millis(1);
        assert!(!s.is_animating(end));
        assert!((position(&s, end) - 2.0).abs() < 1e-3);
    }
}
