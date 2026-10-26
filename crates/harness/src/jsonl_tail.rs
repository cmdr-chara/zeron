//! Polled reading of an append-only JSONL file another process is writing:
//! an agent CLI's own transcript, read while it works (Grok's subagent
//! histories, Claude's typed forks).

use std::path::PathBuf;

use serde_json::Value;

/// Incremental line reader over an append-only JSONL file. A partial trailing
/// line (a write raced mid-append) carries over to the next poll.
pub(crate) struct TailReader {
    path: PathBuf,
    offset: u64,
    carry: Vec<u8>,
}

impl TailReader {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            carry: Vec::new(),
        }
    }

    /// New complete entries since the last poll; io/parse failures read as
    /// "nothing new" (fail soft — the formats are vendor-private).
    pub(crate) fn read_new(&mut self) -> Vec<Value> {
        use std::io::{Read as _, Seek as _};
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        if file.seek(std::io::SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut bytes = Vec::new();
        let Ok(read) = file.read_to_end(&mut bytes) else {
            return Vec::new();
        };
        self.offset += read as u64;
        self.carry.extend_from_slice(&bytes);
        let mut entries = Vec::new();
        let mut consumed = 0;
        for (index, byte) in self.carry.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            if let Ok(v) = serde_json::from_slice::<Value>(&self.carry[consumed..index]) {
                entries.push(v);
            }
            consumed = index + 1;
        }
        // Shift only the incomplete suffix, once per read rather than once
        // per record. A backlog of short lines must remain linear work.
        self.carry.drain(..consumed);
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write as _;

    #[test]
    fn a_tail_reads_each_record_once_when_its_line_is_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-t1.jsonl");
        let mut tail = TailReader::new(path.clone());
        // Not written yet: nothing, and no error.
        assert!(tail.read_new().is_empty());
        let mut file = std::fs::File::create(&path).unwrap();
        write!(file, "{{\"n\":1}}\nnot json\n{{\"n\":").unwrap();
        // The torn line waits for its end.
        assert_eq!(tail.read_new(), [json!({"n": 1})]);
        assert!(tail.read_new().is_empty());
        write!(file, "2}}\n{{\"n\":3}}\n").unwrap();
        assert_eq!(tail.read_new(), [json!({"n": 2}), json!({"n": 3})]);
        assert!(tail.read_new().is_empty());
    }

    #[test]
    fn a_large_backlog_preserves_order_and_a_partial_utf8_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backlog.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        for n in 0..20_000 {
            writeln!(file, "{{\"n\":{n}}}").unwrap();
        }
        file.write_all(b"{\"text\":\"\xe2\x82").unwrap();
        let mut tail = TailReader::new(path);
        let entries = tail.read_new();
        assert_eq!(entries.len(), 20_000);
        for (n, entry) in entries.iter().enumerate() {
            assert_eq!(*entry, json!({"n": n}));
        }
        assert!(tail.read_new().is_empty());
        file.write_all(b"\xac\"}\n").unwrap();
        assert_eq!(tail.read_new(), [json!({"text": "€"})]);
        assert!(tail.read_new().is_empty());
    }
}
