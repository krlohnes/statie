use crate::event::Event;

/// Renders an `Event` as a human-readable line.
pub fn render(e: &Event) -> String {
    match e {
        Event::Note(text) => format!("note: {text}"),
        Event::Tick => "tick".to_string(),
    }
}
