//! A typed fork's transcript, read off disk while the CLI holds it back.
//!
//! The CLI replays a typed fork's transcript only once the fork is over. Where
//! the transcript lives is documented: "`agent_transcript_path` is the
//! subagent's own transcript stored in a nested `subagents/` folder"
//! (https://code.claude.com/docs/en/hooks.md#subagentstop-input), here
//! `<project>/<session>/subagents/agent-<task>.jsonl`. Live-verified 2.1.310:
//! the file appears with the fork's start and grows as the fork works, while
//! stdout is silent. The normalizer shows each record once
//! (`Normalizer::fork_record`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::time::Instant;

use crate::jsonl_tail::TailReader;

/// How often the transcript is read.
const POLL: Duration = Duration::from_millis(250);
/// How long the file may take to appear. It never does under
/// `CLAUDE_CODE_SKIP_PROMPT_HISTORY`, `--no-session-persistence` or a
/// wrapper's own config dir: the tab then fills from the replay.
const LOCATE_FOR: Duration = Duration::from_secs(10);

pub(super) struct ForkTail {
    /// The fork's chip.
    pub chip: String,
    reader: Option<ForkReader>,
    pending: Option<tokio::task::JoinHandle<(ForkReader, Vec<Value>)>>,
    give_up: Instant,
    next: Instant,
}

struct ForkReader {
    /// `<config root>/projects`.
    projects: PathBuf,
    /// `<session>/subagents/agent-<task>.jsonl`, under one of them.
    file: PathBuf,
    /// The transcript, once found.
    reader: Option<TailReader>,
    #[cfg(test)]
    before_read: Option<Box<dyn FnOnce() + Send>>,
}

impl ForkTail {
    pub fn new(claude_root: &Path, (chip, session, task): (&str, &str, &str)) -> Self {
        let now = Instant::now();
        Self {
            chip: chip.to_owned(),
            reader: Some(ForkReader {
                projects: claude_root.join("projects"),
                file: Path::new(session)
                    .join("subagents")
                    .join(format!("agent-{task}.jsonl")),
                reader: None,
                #[cfg(test)]
                before_read: None,
            }),
            pending: None,
            give_up: now + LOCATE_FOR,
            next: now,
        }
    }

    /// When to read next; `None` once the file failed to appear in time.
    pub fn next_read(&self) -> Option<Instant> {
        self.reader.as_ref().and_then(|reader| {
            (reader.reader.is_some() || self.next <= self.give_up).then_some(self.next)
        })
    }

    /// Start at most one read, without blocking the driver's control loop.
    pub fn start_read(&mut self) {
        let Some(mut reader) = self.reader.take() else {
            return;
        };
        self.pending = Some(tokio::task::spawn_blocking(move || {
            let records = reader.read();
            (reader, records)
        }));
    }

    pub fn is_reading(&self) -> bool {
        self.pending.is_some()
    }

    /// Cancellation-safe: a competing stdout/control event leaves the job
    /// in place. A dropped fork detaches its one read and never applies it to
    /// another fork. A failed worker falls back to the CLI's replay.
    pub async fn read_done(&mut self) -> Vec<Value> {
        let result = self.pending.as_mut().expect("read in flight").await;
        self.pending = None;
        self.next = Instant::now() + POLL;
        let Ok((reader, records)) = result else {
            tracing::warn!(chip = %self.chip, "typed fork transcript reader failed; waiting for replay");
            return Vec::new();
        };
        if reader.reader.is_none() && self.next > self.give_up {
            tracing::info!(
                target: "zeron_harness::claude",
                chip = %self.chip,
                "no transcript on disk for the typed fork: its tab fills when it ends"
            );
        }
        self.reader = Some(reader);
        records
    }
}

impl ForkReader {
    fn read(&mut self) -> Vec<Value> {
        #[cfg(test)]
        if let Some(before_read) = self.before_read.take() {
            before_read();
        }
        if self.reader.is_none() {
            self.reader = self.locate().map(TailReader::new);
        }
        self.reader
            .as_mut()
            .map_or_else(Vec::new, TailReader::read_new)
    }

    /// The fork's file, looked for ONE level under `projects/` as Grok's
    /// driver looks for a subagent's history: scanning dodges the CLI's cwd
    /// encoding (long paths are hashed, `CLAUDE_CODE_PROJECT_DIR_NAME` names
    /// the directory outright), and no other fork's file has this name.
    fn locate(&self) -> Option<PathBuf> {
        std::fs::read_dir(&self.projects)
            .ok()?
            .flatten()
            .map(|project| project.path().join(&self.file))
            .find(|path| path.is_file())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read(tail: &mut ForkTail) -> Vec<Value> {
        tail.start_read();
        tail.read_done().await
    }

    #[tokio::test(start_paused = true)]
    async fn a_forks_transcript_is_found_under_any_project_until_the_deadline() {
        let root = tempfile::tempdir().unwrap();
        let mut tail = ForkTail::new(root.path(), ("chip", "s-1", "t1"));
        assert!(read(&mut tail).await.is_empty(), "no history yet");
        // A hashed long path or `CLAUDE_CODE_PROJECT_DIR_NAME`: not what the
        // cwd would name.
        let project = root.path().join("projects/work");
        std::fs::create_dir_all(project.join("s-1/subagents")).unwrap();
        std::fs::write(project.join("s-1/subagents/agent-t1.jsonl"), "{\"n\":1}\n").unwrap();
        tokio::time::advance(POLL).await;
        assert_eq!(tail.next_read(), Some(Instant::now()));
        assert_eq!(read(&mut tail).await, [serde_json::json!({"n": 1})]);
        // Found, it is read for as long as the fork runs.
        tokio::time::advance(LOCATE_FOR).await;
        assert!(tail.next_read().is_some());

        // Another task's transcript never appears: given up at the deadline.
        let mut other = ForkTail::new(root.path(), ("chip", "s-1", "t2"));
        let start = Instant::now();
        for _ in 0..100 {
            let Some(at) = other.next_read() else { break };
            tokio::time::advance(at - Instant::now()).await;
            assert!(read(&mut other).await.is_empty());
        }
        assert_eq!(other.next_read(), None, "still looking");
        let looked = Instant::now() - start;
        assert!(
            looked > LOCATE_FOR - POLL && looked <= LOCATE_FOR,
            "{looked:?}"
        );
    }

    #[tokio::test]
    async fn a_slow_read_leaves_the_driver_free_and_survives_a_cancelled_wait() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("projects/work/s-1/subagents");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("agent-t1.jsonl"), "{\"n\":1}\n").unwrap();
        let mut tail = ForkTail::new(root.path(), ("chip", "s-1", "t1"));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        tail.reader.as_mut().unwrap().before_read = Some(Box::new(move || {
            let _ = started_tx.send(());
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }));
        tail.start_read();
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .expect("the worker starts")
            .unwrap();
        assert!(tail.is_reading());
        assert_eq!(tail.next_read(), None, "never start overlapping reads");
        assert!(
            tokio::time::timeout(Duration::from_millis(10), tail.read_done())
                .await
                .is_err()
        );
        assert!(
            tail.is_reading(),
            "a competing control event keeps the read"
        );
        release_tx.send(()).unwrap();
        assert_eq!(tail.read_done().await, [serde_json::json!({"n": 1})]);
        assert!(!tail.is_reading());
        assert!(tail.next_read().is_some());
    }
}
