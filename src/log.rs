//! Journal output that prints each error once, so a condition that persists
//! (cooler unplugged) doesn't flood the journal.

#[derive(Default)]
pub struct Log {
    last: String,
}

impl Log {
    pub fn error(&mut self, msg: String) {
        if msg != self.last {
            eprintln!("{msg}");
            self.last = msg;
        }
    }

    pub fn info(&mut self, msg: String) {
        eprintln!("{msg}");
        self.last = msg;
    }
}
