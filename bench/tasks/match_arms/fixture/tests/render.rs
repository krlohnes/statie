use match_arms::{render, Event};

// Expected output per variant:
//   Started { name }          -> "started: <name>"
//   Progress { name, pct }    -> "<name>: <pct>%"
//   Finished { name, ok: true }  -> "finished: <name> (ok)"
//   Finished { name, ok: false } -> "finished: <name> (failed)"
//   Note(text)                -> "note: <text>"
//   Tick                      -> "tick"

#[test]
fn started() {
    let e = Event::Started {
        name: "build".to_string(),
    };
    assert_eq!(render(&e), "started: build");
}

#[test]
fn progress() {
    let e = Event::Progress {
        name: "build".to_string(),
        pct: 42,
    };
    assert_eq!(render(&e), "build: 42%");
}

#[test]
fn finished_ok() {
    let e = Event::Finished {
        name: "build".to_string(),
        ok: true,
    };
    assert_eq!(render(&e), "finished: build (ok)");
}

#[test]
fn finished_failed() {
    let e = Event::Finished {
        name: "build".to_string(),
        ok: false,
    };
    assert_eq!(render(&e), "finished: build (failed)");
}

#[test]
fn note() {
    let e = Event::Note("hello".to_string());
    assert_eq!(render(&e), "note: hello");
}

#[test]
fn tick() {
    assert_eq!(render(&Event::Tick), "tick");
}
