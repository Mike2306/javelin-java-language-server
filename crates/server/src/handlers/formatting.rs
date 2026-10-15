//! `textDocument/formatting` — the buffer through the formatter `format.command`
//! names, answered as one edit replacing the whole document.
//!
//! The formatter is often a JVM, seconds from its first byte, so it runs on
//! a thread of its own holding a clone of the connection's sender, and the
//! request loop goes on answering everything else meanwhile.
//!
//! ponytail: `$/cancelRequest` does not reach the thread; `format.timeoutMs`
//! bounds it instead, and an exit waits for a run in flight up to that long.
//! The upgrade is a token the thread polls between its waits.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use javelin_core::config::FormatConfig;
use javelin_document::DocumentId;
use lsp_server::{Message, Request, Response};
use lsp_types::request::{Formatting, Request as _};
use lsp_types::{Position, Range, TextEdit};

use crate::ServerState;
use crate::handlers::to_lsp_position;

/// Answer a formatting request off the loop, or hand `request` back where
/// it is not one or no formatter is configured, for the router to answer.
pub fn start(state: &ServerState, request: Request, sender: &Sender<Message>) -> Option<Request> {
    let format = &state.config.format;
    if request.method != Formatting::METHOD || format.command.is_empty() {
        return Some(request);
    }
    let (id, params) = match crate::extract_params::<Formatting>(request) {
        Ok(extracted) => extracted,
        Err(response) => {
            let _ = sender.send(Message::Response(response));
            return None;
        }
    };
    let Some(document) = state
        .documents
        .get(&DocumentId::new(params.text_document.uri.as_str()))
    else {
        let _ = sender.send(Message::Response(Response::new_ok(
            id,
            None::<Vec<TextEdit>>,
        )));
        return None;
    };
    let text = document.text().to_owned();
    let end = to_lsp_position(document.position_at(text.len() as u32));
    let dir = crate::workspace::path_of(params.text_document.uri.as_str())
        .and_then(|path| path.parent().map(ToOwned::to_owned));
    let format = format.clone();
    let sender = sender.clone();
    std::thread::spawn(move || {
        let response = match run(&format, dir.as_deref(), &text) {
            Ok(formatted) if formatted == text => Response::new_ok(id, Vec::<TextEdit>::new()),
            Ok(formatted) => Response::new_ok(
                id,
                vec![TextEdit::new(
                    Range::new(Position::new(0, 0), end),
                    formatted,
                )],
            ),
            Err(reason) => Response::new_err(
                id,
                lsp_server::ErrorCode::RequestFailed as i32,
                format!("cannot format: {reason}"),
            ),
        };
        // A client gone while the formatter ran has nobody to tell.
        let _ = sender.send(Message::Response(response));
    });
    None
}

/// What one of the formatter's pipes came to.
enum Pipe {
    Stdin(std::io::Result<()>),
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

/// `text` through the formatter, or why not: one that cannot start, exits
/// non-zero, reads only part of the file, outlives the timeout — it and
/// whatever it started holding its pipes — or writes nothing or not UTF-8.
fn run(format: &FormatConfig, dir: Option<&std::path::Path>, text: &str) -> Result<String, String> {
    let (program, args) = format.command.split_first().ok_or("no command")?;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Where a formatter looks for its project's settings.
    if let Some(dir) = dir.filter(|dir| dir.is_dir()) {
        command.current_dir(dir);
    }
    // A group of its own, so a timeout reaches the JVM a wrapper script
    // started and not only the script.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = command
        .spawn()
        .map_err(|err| format!("`{program}` did not start: {err}"))?;
    let deadline = Instant::now() + Duration::from_millis(format.timeout_ms);
    let timed_out = |child: &mut Child| {
        kill(child);
        Err(format!(
            "`{program}` took longer than {} ms",
            format.timeout_ms
        ))
    };
    // Each pipe on a thread of its own — a formatter blocked writing a full
    // stdout pipe never reads the rest of its stdin — and every wait bounded
    // by the deadline: a process the formatter left behind can hold a pipe
    // open long after the formatter itself exited.
    let (sent, pipes) = mpsc::channel();
    if let Some(mut stdin) = child.stdin.take() {
        let (sent, text) = (sent.clone(), text.to_owned());
        std::thread::spawn(move || sent.send(Pipe::Stdin(stdin.write_all(text.as_bytes()))));
    }
    if let Some(mut stdout) = child.stdout.take() {
        let sent = sent.clone();
        std::thread::spawn(move || sent.send(Pipe::Stdout(read_all(&mut stdout))));
    }
    if let Some(mut stderr) = child.stderr.take() {
        std::thread::spawn(move || sent.send(Pipe::Stderr(read_all(&mut stderr))));
    } else {
        drop(sent);
    }
    let (mut wrote, mut stdout, mut stderr) = (Ok(()), Vec::new(), Vec::new());
    loop {
        match pipes.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Pipe::Stdin(result)) => wrote = result,
            Ok(Pipe::Stdout(bytes)) => stdout = bytes,
            Ok(Pipe::Stderr(bytes)) => stderr = bytes,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => return timed_out(&mut child),
        }
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => return timed_out(&mut child),
            Err(err) => return Err(format!("`{program}` could not be waited on: {err}")),
        }
    };
    let first_line = || {
        String::from_utf8_lossy(&stderr)
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(ToOwned::to_owned)
    };
    if !status.success() {
        return Err(first_line().unwrap_or_else(|| format!("`{program}` failed: {status}")));
    }
    // Output from a formatter that stopped reading is the part it read.
    if let Err(err) = wrote {
        return Err(format!("`{program}` did not read the whole file: {err}"));
    }
    if stdout.is_empty() && !text.is_empty() {
        return Err(first_line().unwrap_or_else(|| format!("`{program}` wrote nothing")));
    }
    String::from_utf8(stdout).map_err(|_| format!("`{program}` wrote something not UTF-8"))
}

fn read_all(pipe: &mut impl Read) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = pipe.read_to_end(&mut bytes);
    bytes
}

/// The formatter and, where processes have groups, everything it started.
///
/// ponytail: on Windows only the formatter itself is killed, so a JVM its
/// wrapper script started can outlive the timeout; the answer is bounded
/// all the same. The upgrade is a Job Object that kills the whole tree.
fn kill(child: &mut Child) {
    #[cfg(unix)]
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{}", child.id())])
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, timeout_ms: u64) -> FormatConfig {
        FormatConfig {
            command: vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
            timeout_ms,
        }
    }

    #[test]
    fn the_formatters_stdout_is_the_answer() {
        assert_eq!(
            run(&sh("tr a-z A-Z", 5_000), None, "class a {}\n"),
            Ok("CLASS A {}\n".to_owned())
        );
    }

    #[test]
    fn a_failing_formatter_answers_its_first_stderr_line() {
        assert_eq!(
            run(
                &sh("echo out; echo first >&2; echo second >&2; exit 3", 5_000),
                None,
                "x"
            ),
            Err("first".to_owned())
        );
        assert_eq!(
            run(
                &sh("echo '\n  A.java:1: error: bad\nnext' >&2; exit 1", 5_000),
                None,
                "x"
            ),
            Err("A.java:1: error: bad".to_owned())
        );
    }

    #[test]
    fn no_output_a_missing_program_and_a_slow_one_are_refusals_not_hangs() {
        assert!(run(&sh("cat >/dev/null", 5_000), None, "x").is_err());
        let missing = FormatConfig {
            command: vec!["javelin-no-such-formatter".to_owned()],
            timeout_ms: 5_000,
        };
        assert!(run(&missing, None, "x").is_err_and(|reason| reason.contains("did not start")));
        let started = Instant::now();
        assert!(run(&sh("sleep 5", 100), None, "x").is_err_and(|reason| reason.contains("100 ms")));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    // Unix only: Windows has no process group to kill the left-behind child with.
    #[cfg(unix)]
    #[test]
    fn a_process_left_holding_the_pipes_is_bounded_and_killed() {
        let started = Instant::now();
        let pid = std::env::temp_dir().join(format!("javelin-format-{}", std::process::id()));
        let script = format!("sleep 30 & echo $! > {}; cat", pid.display());
        assert!(run(&sh(&script, 300), None, "x").is_err_and(|reason| reason.contains("300 ms")));
        assert!(started.elapsed() < Duration::from_secs(5));
        let pid = std::fs::read_to_string(&pid).expect("the script wrote its child");
        // A killed orphan answers `kill -0` until init reaps it, which a busy CI
        // runner takes its time over.
        let alive = || {
            Command::new("kill")
                .args(["-0", pid.trim()])
                .status()
                .expect("kill runs")
                .success()
        };
        let reaped = Instant::now() + Duration::from_secs(5);
        while alive() && Instant::now() < reaped {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(), "the formatter's own child outlived the timeout");
    }

    #[test]
    fn a_formatter_that_reads_only_part_of_the_file_is_refused() {
        let text = "class A {}\n".repeat(100_000);
        assert!(run(&sh("head -n 1", 5_000), None, &text).is_err());
    }

    #[test]
    fn an_empty_file_formats_to_an_empty_file() {
        assert_eq!(run(&sh("cat", 5_000), None, ""), Ok(String::new()));
    }

    #[test]
    fn a_file_larger_than_a_pipe_buffer_goes_through_whole() {
        let text = "class A {}\n".repeat(100_000);
        assert_eq!(run(&sh("cat", 5_000), None, &text), Ok(text));
    }
}
