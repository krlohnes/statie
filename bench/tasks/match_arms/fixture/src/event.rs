/// An event emitted while a task runs.
pub enum Event {
    Started { name: String },
    Progress { name: String, pct: u8 },
    Finished { name: String, ok: bool },
    Note(String),
    Tick,
}
