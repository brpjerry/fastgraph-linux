pub mod audio;
pub mod curve_io;
pub mod deconv;
pub mod detect;
pub mod distortion;
pub mod fsutil;
pub mod prefs;
pub mod response;
pub mod rta;
pub mod session;
pub mod set;
pub mod signal;
pub mod store;

/// Callback used by background workers to report events to the UI layer.
pub type EventSink<E> = Box<dyn FnMut(E) + Send + 'static>;
