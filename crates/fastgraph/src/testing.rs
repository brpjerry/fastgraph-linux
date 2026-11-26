//! Test support: headless windows on the app's view, one at a time.
use crate::app::Message;
use iced::Element;
use iced_test::Simulator;
use std::ops::{Deref, DerefMut};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The headless window's size: the window's default.
pub const SIZE: (f32, f32) = (1280.0, 800.0);

/// Held while a headless renderer exists: the Vulkan loader can crash when
/// tests on parallel threads create wgpu instances at the same time.
static GPU: Mutex<()> = Mutex::new(());

/// A headless window of `size` showing `element`, with the window's fonts.
pub fn ui<'a>(size: (f32, f32), element: impl Into<Element<'a, Message>>) -> Ui<'a> {
    let gpu = GPU.lock().unwrap_or_else(PoisonError::into_inner);
    Ui {
        simulator: Simulator::with_size(crate::theme::settings(), size, element),
        _gpu: gpu,
    }
}

/// Whether `view` shows `label` as a whole text.
pub fn shows(view: Element<'_, Message>, label: &str) -> bool {
    ui(SIZE, view).find(label).is_ok()
}

/// A [`Simulator`] that keeps [`GPU`] locked until it, declared first, is
/// dropped.
pub struct Ui<'a> {
    simulator: Simulator<'a, Message>,
    _gpu: MutexGuard<'static, ()>,
}

impl Ui<'_> {
    /// The messages the interactions so far produced.
    pub fn messages(self) -> Vec<Message> {
        self.simulator.into_messages().collect()
    }
}

impl<'a> Deref for Ui<'a> {
    type Target = Simulator<'a, Message>;
    fn deref(&self) -> &Self::Target {
        &self.simulator
    }
}

impl DerefMut for Ui<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.simulator
    }
}
