use crate::event::Event;

/// Renders an `Event` as a human-readable line.
pub fn render(e: &Event) -> String {
    match e {
        Event::Started { name } => format!("started: {name}"),
        Event::Progress { name, pct } => format!("{name}: {pct}%"),
        Event::Finished { name, ok: true } => format!("finished: {name} (ok)"),
        Event::Finished { name, ok: false } => format!("finished: {name} (failed)"),
        Event::Note(text) => format!("note: {text}"),
        Event::Tick => "tick".to_string(),
    }
}
