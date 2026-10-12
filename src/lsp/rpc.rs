//! JSON-RPC over a language server's stdio: framing, the process, and message dispatch.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use process_wrap::std::{ChildWrapper, CommandWrap};
use serde_json::{Value, json};

/// The largest frame a server may send; a references answer in a big tree fits well under it.
const MAX_FRAME: usize = 64 << 20;
/// Frames waiting for a server that stopped reading; past it, the connection has stalled.
const OUTBOX: usize = 64;
/// How long a shut-down server gets to exit on its own before its tree is killed.
const EXIT_GRACE: Duration = Duration::from_millis(500);

/// Write one message as an LSP frame.
pub fn write_message(out: &mut impl Write, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()
}

/// Read one LSP frame; `None` at a clean end of stream between frames.
pub fn read_message(input: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut length = None;
    let mut line = String::new();
    loop {
        line.clear();
        // Header lines are short; a stream that never ends one is not a server.
        if input.by_ref().take(1024).read_line(&mut line)? == 0 {
            return if length.is_none() && line.is_empty() {
                Ok(None)
            } else {
                Err(invalid("stream ended inside a header"))
            };
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = Some(value.trim().parse::<usize>().map_err(|_| invalid("bad length"))?);
        }
    }
    let length = length.ok_or_else(|| invalid("frame without a length"))?;
    if length > MAX_FRAME {
        return Err(invalid("frame over the size limit"));
    }
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    serde_json::from_slice(&body).map(Some).map_err(|e| invalid(&e.to_string()))
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

/// What one server message comes to once the connection has answered what it can.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// The answer to one of our live requests: its result, or the server's error message.
    Response { id: u64, result: Result<Value, String> },
    /// A notification, a request answered here, or an answer nobody waits for any more.
    Handled,
}

/// One running server: its process tree, a writer thread, and our requests in flight.
pub struct Connection {
    child: Box<dyn ChildWrapper>,
    /// Frames for the writer thread, so a server that stops reading never blocks the worker.
    outbox: mpsc::SyncSender<Value>,
    /// The server stopped taking frames, or its writer ended: only a restart helps.
    stalled: bool,
    next_id: u64,
    /// Each live request's deadline.
    pending: HashMap<u64, Instant>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection").field("pid", &self.child.id()).finish_non_exhaustive()
    }
}

impl Connection {
    /// Start `cmd` in its own process tree; its frames go to `on_frame`, then `None` at the end.
    pub fn spawn(
        cmd: Command,
        on_frame: impl Fn(Option<Value>) + Send + 'static,
    ) -> io::Result<Self> {
        let mut cmd = CommandWrap::from(cmd);
        cmd.command_mut().stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(unix)]
        cmd.wrap(process_wrap::std::ProcessGroup::leader());
        #[cfg(windows)]
        cmd.wrap(process_wrap::std::JobObject);
        let mut child = cmd.spawn()?;
        let (stdin, stdout, stderr) =
            (child.stdin().take(), child.stdout().take(), child.stderr().take());
        let (outbox, frames) = mpsc::sync_channel::<Value>(OUTBOX);
        if let Some(mut stdin) = stdin {
            thread::spawn(move || {
                for frame in frames {
                    if write_message(&mut stdin, &frame).is_err() {
                        break;
                    }
                }
            });
        }
        if let Some(stdout) = stdout {
            thread::spawn(move || {
                let mut input = BufReader::new(stdout);
                loop {
                    match read_message(&mut input) {
                        Ok(Some(frame)) => on_frame(Some(frame)),
                        Ok(None) => break,
                        Err(e) => {
                            logln!("lsp read failed: {e}");
                            break;
                        }
                    }
                }
                on_frame(None);
            });
        }
        // Drained so a chatty server never fills the pipe and stalls.
        if let Some(mut stderr) = stderr {
            thread::spawn(move || io::copy(&mut stderr, &mut io::sink()));
        }
        Ok(Self { child, outbox, stalled: false, next_id: 1, pending: HashMap::new() })
    }

    /// Send a request, answered within `timeout` or reported by [`Connection::expired`].
    pub fn request(&mut self, method: &str, params: &Value, timeout: Duration) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, Instant::now() + timeout);
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    pub fn notify(&mut self, method: &str, params: &Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Give up on a request; its late answer is then [`Incoming::Handled`].
    pub fn cancel(&mut self, id: u64) {
        if self.pending.remove(&id).is_some() {
            self.notify("$/cancelRequest", &json!({"id": id}));
        }
    }

    /// Requests past their deadline, which stop waiting.
    pub fn expired(&mut self, now: Instant) -> Vec<u64> {
        let gone: Vec<u64> =
            self.pending.iter().filter(|(_, at)| **at <= now).map(|(id, _)| *id).collect();
        for id in &gone {
            self.cancel(*id);
        }
        gone
    }

    /// The nearest request deadline, for the worker's wait.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().min().copied()
    }

    /// Dispatch one frame: answer the server's requests, drop its notifications.
    pub fn receive(&mut self, frame: &Value) -> Incoming {
        let method = frame.get("method").and_then(Value::as_str);
        match (frame.get("id"), method) {
            (Some(id), Some(method)) => {
                let mut reply = match method {
                    // One empty setting per item asked: every server default stands.
                    "workspace/configuration" => {
                        let items = frame["params"]["items"].as_array().map_or(0, Vec::len);
                        json!({"result": vec![Value::Null; items]})
                    }
                    "client/registerCapability"
                    | "client/unregisterCapability"
                    | "window/workDoneProgress/create"
                    | "window/showMessageRequest" => json!({"result": null}),
                    _ => json!({"error": {"code": -32601, "message": "method not found"}}),
                };
                reply["jsonrpc"] = json!("2.0");
                reply["id"] = id.clone();
                self.send(reply);
                Incoming::Handled
            }
            (Some(id), None) => {
                let Some(id) = id.as_u64().filter(|id| self.pending.remove(id).is_some()) else {
                    return Incoming::Handled;
                };
                let result = match frame.get("error") {
                    Some(error) => Err(error["message"].as_str().unwrap_or("error").to_string()),
                    None => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
                };
                Incoming::Response { id, result }
            }
            _ => Incoming::Handled,
        }
    }

    fn send(&mut self, frame: Value) {
        if self.outbox.try_send(frame).is_err() {
            self.stalled = true;
        }
    }

    pub fn stalled(&self) -> bool {
        self.stalled
    }

    /// `exit` once `shutdown` is answered (`next` yields frames until a deadline), then end the tree.
    pub fn shutdown(mut self, mut next: impl FnMut(Instant) -> Option<Value>) {
        let deadline = Instant::now() + EXIT_GRACE;
        let id = self.request("shutdown", &Value::Null, EXIT_GRACE);
        while Instant::now() < deadline
            && let Some(frame) = next(deadline)
        {
            if matches!(self.receive(&frame), Incoming::Response { id: answered, .. } if answered == id)
            {
                break;
            }
        }
        self.notify("exit", &Value::Null);
        let deadline = Instant::now() + EXIT_GRACE;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // The whole group or job: a server's own helpers go with it.
        let _ = self.child.start_kill();
        let _ = self.child.inner_mut().wait();
    }
}

#[cfg(test)]
mod tests {
    use super::{Incoming, MAX_FRAME, read_message, write_message};
    use serde_json::json;
    use std::io::Cursor;
    use std::sync::mpsc;

    const LONG: std::time::Duration = std::time::Duration::from_hours(1);

    /// A connection to a process that echoes nothing back, and the frames sent to it.
    fn connection() -> (super::Connection, mpsc::Receiver<serde_json::Value>) {
        #[cfg(unix)]
        let cmd = std::process::Command::new("cat");
        #[cfg(windows)]
        let cmd = {
            let mut cmd = std::process::Command::new("cmd");
            cmd.args(["/c", "more"]);
            cmd
        };
        let mut conn = super::Connection::spawn(cmd, |_| {}).unwrap();
        let (outbox, sent) = mpsc::sync_channel(super::OUTBOX);
        conn.outbox = outbox;
        (conn, sent)
    }

    fn frames(bytes: &[u8]) -> Vec<std::io::Result<Option<serde_json::Value>>> {
        let mut input = Cursor::new(bytes.to_vec());
        let mut out = Vec::new();
        loop {
            let next = read_message(&mut input);
            let done = !matches!(next, Ok(Some(_)));
            out.push(next);
            if done {
                return out;
            }
        }
    }

    #[test]
    fn a_frame_round_trips_and_a_clean_end_reads_as_none() {
        let mut bytes = Vec::new();
        write_message(&mut bytes, &json!({"id": 1, "result": "ünï"})).unwrap();
        write_message(&mut bytes, &json!({"method": "x"})).unwrap();
        let read = frames(&bytes);
        assert_eq!(read.len(), 3);
        assert_eq!(read[0].as_ref().unwrap(), &Some(json!({"id": 1, "result": "ünï"})));
        assert_eq!(read[1].as_ref().unwrap(), &Some(json!({"method": "x"})));
        assert!(matches!(read[2], Ok(None)));
    }

    #[test]
    fn other_headers_and_any_case_are_read_past() {
        let body = br#"{"id":2}"#;
        let bytes =
            [format!("content-length: {}\r\nContent-Type: x\r\n\r\n", body.len()).as_bytes(), body]
                .concat();
        assert_eq!(frames(&bytes)[0].as_ref().unwrap(), &Some(json!({"id": 2})));
    }

    #[test]
    fn a_cut_frame_a_missing_length_and_an_oversized_one_fail() {
        assert!(frames(b"Content-Length: 10\r\n\r\n{\"id\"")[0].is_err(), "cut body");
        assert!(frames(b"Content-Length: 10\r\n")[0].is_err(), "cut header");
        assert!(frames(b"Content-Type: x\r\n\r\n{}")[0].is_err(), "no length");
        let huge = format!("Content-Length: {}\r\n\r\n", MAX_FRAME + 1);
        assert!(frames(huge.as_bytes())[0].is_err(), "over the limit");
    }

    #[test]
    fn dispatch_answers_server_requests_and_matches_live_responses() {
        let (mut conn, sent) = connection();
        let id = conn.request("textDocument/definition", &json!({}), LONG);
        // A notification between the request and its answer is dropped.
        assert_eq!(
            conn.receive(&json!({"method": "textDocument/publishDiagnostics"})),
            Incoming::Handled
        );
        let ask =
            json!({"id": "c1", "method": "workspace/configuration", "params": {"items": [{}, {}]}});
        assert_eq!(conn.receive(&ask), Incoming::Handled);
        let reply = std::iter::from_fn(|| sent.try_recv().ok()).find(|m| m["id"] == "c1").unwrap();
        assert_eq!(reply["id"], "c1");
        assert_eq!(reply["result"], json!([null, null]));
        assert_eq!(
            conn.receive(&json!({"id": id, "result": [1]})),
            Incoming::Response { id, result: Ok(json!([1])) }
        );
        // A second answer to the same id, or one to a cancelled request, waits on nothing.
        assert_eq!(conn.receive(&json!({"id": id, "result": [1]})), Incoming::Handled);
        let late = conn.request("x", &json!({}), LONG);
        conn.cancel(late);
        assert_eq!(conn.receive(&json!({"id": late, "result": 1})), Incoming::Handled);
        let unknown = json!({"id": 9, "method": "workspace/applyEdit", "params": {}});
        conn.receive(&unknown);
        let refused = std::iter::from_fn(|| sent.try_recv().ok()).find(|m| m["id"] == 9).unwrap();
        assert_eq!(refused["error"]["code"], -32601, "never applies an edit");
    }

    #[test]
    fn an_error_answer_carries_its_message_and_a_late_request_expires() {
        let (mut conn, _sent) = connection();
        let id = conn.request("x", &json!({}), LONG);
        let error = json!({"id": id, "error": {"code": -32800, "message": "cancelled"}});
        assert_eq!(
            conn.receive(&error),
            Incoming::Response { id, result: Err("cancelled".into()) }
        );
        let slow = conn.request("x", &json!({}), std::time::Duration::ZERO);
        assert_eq!(conn.expired(std::time::Instant::now()), vec![slow]);
        assert_eq!(conn.next_deadline(), None);
    }

    #[test]
    fn a_live_server_answers_through_its_own_requests_and_notifications() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("chatty"), "").unwrap();
        let mut cmd = std::process::Command::new(crate::test_support::fake_lsp());
        cmd.env("FAKE_LSP_DIR", dir.path());
        let (frames_tx, frames) = mpsc::channel();
        let mut conn = super::Connection::spawn(cmd, move |f| drop(frames_tx.send(f))).unwrap();
        let id = conn.request("initialize", &json!({}), LONG);
        let answer = loop {
            let frame = frames.recv().unwrap().expect("the server is up");
            if let Incoming::Response { result, .. } = conn.receive(&frame) {
                break result.unwrap();
            }
        };
        assert_eq!(answer["capabilities"]["definitionProvider"], true);
        assert_eq!(conn.next_deadline(), None, "{id} answered");
        // Our reply to its `workspace/configuration` reached it before the end.
        conn.shutdown(|at| frames.recv_timeout(at - std::time::Instant::now()).ok().flatten());
        let log = std::fs::read_to_string(dir.path().join("lsp.log")).unwrap();
        assert!(log.lines().any(|l| l.contains(r#""id":"ask""#) && l.contains("result")), "{log}");
    }

    #[test]
    fn exit_waits_for_the_shutdown_answer() {
        let (conn, sent) = connection();
        let mut asked = None;
        conn.shutdown(|_| {
            let frames: Vec<_> = std::iter::from_fn(|| sent.try_recv().ok()).collect();
            assert!(frames.iter().all(|f| f["method"] != "exit"), "exit before the answer");
            if let Some(shutdown) = frames.iter().find(|f| f["method"] == "shutdown") {
                asked = Some(shutdown["id"].clone());
            }
            asked.as_ref().map(|id| json!({"id": id, "result": null}))
        });
        assert!(std::iter::from_fn(|| sent.try_recv().ok()).any(|f| f["method"] == "exit"));
    }

    #[test]
    fn a_server_that_stops_reading_stalls_the_connection_instead_of_queueing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("deaf"), "").unwrap();
        let mut cmd = std::process::Command::new(crate::test_support::fake_lsp());
        cmd.env("FAKE_LSP_DIR", dir.path());
        let mut conn = super::Connection::spawn(cmd, |_| {}).unwrap();
        let text = "x".repeat(64 << 10);
        // The pipe holds a few of these and the outbox the rest, then sending fails.
        for _ in 0..super::OUTBOX * 2 {
            conn.notify("textDocument/didChange", &json!({"text": text}));
        }
        assert!(conn.stalled());
    }

    #[test]
    fn a_noisy_server_never_holds_shutdown_past_its_grace() {
        let (conn, _sent) = connection();
        let started = std::time::Instant::now();
        conn.shutdown(|_| Some(json!({"method": "window/logMessage"})));
        assert!(started.elapsed() < super::EXIT_GRACE * 3, "{:?}", started.elapsed());
    }
}
