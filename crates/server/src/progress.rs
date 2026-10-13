//! Background work as the client's progress UI shows it: LSP's server-initiated
//! work-done progress.
//!
//! A worker holds a [`Reporter`] and opens a [`Task`] for each stretch of work
//! a user may be waiting on — the walk, the build tool, the member-reference
//! pass — and the loop turns each [`Progress`] event into a
//! `window/workDoneProgress/create` request and `$/progress` notifications.
//! Zed shows them beside the language server's name, which is how a reviewer
//! knows cross-file references are still coming rather than absent.

use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_channel::Sender;
use lsp_server::{Message, Notification, Request, RequestId};

/// One event of one task, named by the task's number.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Progress {
    Begin {
        task: u64,
        title: String,
    },
    Report {
        task: u64,
        message: String,
        percentage: Option<u32>,
    },
    End {
        task: u64,
    },
}

/// Where a worker sends its progress; a reporter with nowhere to send drops it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Reporter(Option<Sender<Progress>>);

/// Numbers tasks across the session: a token is used for one begin–end
/// cycle, so every task gets its own.
static NEXT_TASK: AtomicU64 = AtomicU64::new(1);

impl Reporter {
    pub(crate) fn new(sender: Sender<Progress>) -> Self {
        Self(Some(sender))
    }

    /// Open a task; it ends when the [`Task`] is dropped, on every path out.
    pub(crate) fn begin(&self, title: &str) -> Task {
        let task = NEXT_TASK.fetch_add(1, Ordering::Relaxed);
        self.send(Progress::Begin {
            task,
            title: title.to_owned(),
        });
        Task {
            reporter: self.clone(),
            task,
            last: None,
        }
    }

    fn send(&self, event: Progress) {
        if let Some(sender) = &self.0 {
            // A loop that has gone has no client to show it to.
            let _ = sender.send(event);
        }
    }
}

/// One stretch of background work, open until dropped.
pub(crate) struct Task {
    reporter: Reporter,
    task: u64,
    /// The last percentage sent, so a pass over thousands of files sends a
    /// hundred reports rather than thousands.
    last: Option<u32>,
}

impl Task {
    /// `done` of `total`, sent when the whole percentage moves.
    pub(crate) fn step(&mut self, done: usize, total: usize, unit: &str) {
        let percentage = (done.min(total) * 100 / total.max(1)) as u32;
        if self.last == Some(percentage) {
            return;
        }
        self.last = Some(percentage);
        self.reporter.send(Progress::Report {
            task: self.task,
            message: format!("{done}/{total} {unit}"),
            percentage: Some(percentage),
        });
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.reporter.send(Progress::End { task: self.task });
    }
}

/// The messages that tell the client about `event`. A task's token is created
/// with its begin: LSP lets a server report on a token it created, and the
/// `create` request is answered by a response the loop ignores.
pub(crate) fn messages(event: &Progress) -> Vec<Message> {
    let (task, value) = match event {
        Progress::Begin { task, title } => (
            *task,
            serde_json::json!({ "kind": "begin", "title": title, "cancellable": false }),
        ),
        Progress::Report {
            task,
            message,
            percentage,
        } => (
            *task,
            match percentage {
                Some(percentage) => serde_json::json!({
                    "kind": "report", "message": message, "percentage": percentage,
                }),
                None => serde_json::json!({ "kind": "report", "message": message }),
            },
        ),
        Progress::End { task } => (*task, serde_json::json!({ "kind": "end" })),
    };
    let token = token(task);
    let progress = Message::Notification(Notification {
        method: "$/progress".to_owned(),
        params: serde_json::json!({ "token": token, "value": value }),
    });
    match event {
        Progress::Begin { .. } => vec![
            Message::Request(Request {
                id: RequestId::from(token.clone()),
                method: "window/workDoneProgress/create".to_owned(),
                params: serde_json::json!({ "token": token }),
            }),
            progress,
        ],
        _ => vec![progress],
    }
}

/// The token of a task, which is also the id of the request creating it.
fn token(task: u64) -> String {
    format!("javelin/progress/{task}")
}

/// Whether the client said it shows server-initiated progress.
pub(crate) fn supported(init_params: &serde_json::Value) -> bool {
    init_params
        .pointer("/capabilities/window/workDoneProgress")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_ends_when_dropped_and_reports_each_percentage_once() {
        let (sender, events) = crossbeam_channel::unbounded();
        let reporter = Reporter::new(sender);
        {
            let mut task = reporter.begin("Resolving references");
            for done in 0..=400 {
                task.step(done, 400, "files");
            }
        }
        let events: Vec<Progress> = events.try_iter().collect();
        assert!(
            matches!(&events[0], Progress::Begin { title, .. } if title == "Resolving references")
        );
        assert!(matches!(events.last(), Some(Progress::End { .. })));
        // 0% through 100%, each once, between the begin and the end.
        assert_eq!(events.len(), 1 + 101 + 1, "{events:?}");
    }

    #[test]
    fn a_begin_creates_its_token_before_reporting_on_it() {
        let messages = messages(&Progress::Begin {
            task: 7,
            title: "Indexing".to_owned(),
        });
        let Message::Request(create) = &messages[0] else {
            panic!("{messages:?}");
        };
        assert_eq!(create.method, "window/workDoneProgress/create");
        assert_eq!(create.id, RequestId::from(token(7)));
        let Message::Notification(begin) = &messages[1] else {
            panic!("{messages:?}");
        };
        assert_eq!(begin.params["token"], create.params["token"]);
        assert_eq!(begin.params["value"]["kind"], "begin");
        let end = messages_of_end(7);
        assert_eq!(end.params["token"], create.params["token"]);
    }

    fn messages_of_end(task: u64) -> Notification {
        match messages(&Progress::End { task }).remove(0) {
            Message::Notification(end) => end,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn only_a_client_that_says_it_shows_progress_is_sent_any() {
        assert!(supported(
            &serde_json::json!({ "capabilities": { "window": { "workDoneProgress": true } } })
        ));
        assert!(!supported(&serde_json::json!({ "capabilities": {} })));
    }

    #[test]
    fn a_reporter_with_nowhere_to_send_drops_everything() {
        let mut task = Reporter::default().begin("Indexing");
        task.step(1, 2, "files");
    }
}
