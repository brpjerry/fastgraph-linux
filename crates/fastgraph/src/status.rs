//! The status line's text and level.

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub text: String,
    pub level: StatusLevel,
}

impl Status {
    /// Every note on one line, the worst setting the level; `None` for none.
    pub(crate) fn joined(notes: Vec<(String, StatusLevel)>) -> Option<Status> {
        let level = notes.iter().map(|(_, level)| *level).max()?;
        let text: Vec<String> = notes.into_iter().map(|(text, _)| text).collect();
        Some(Status {
            text: text.join(" · "),
            level,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StatusLevel {
    Info,
    Warn,
    Error,
}
