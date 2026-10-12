//! Code navigation through language servers: one LSP client, a table of servers.

pub mod rpc;
pub mod servers;
pub mod uri;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::config::PluginConfig;
use rpc::{Connection, Incoming};
use uri::Located;

/// How long one request may take before its server counts as hung; a cold one answers slowly.
const REQUEST_TIMEOUT: Duration =
    if cfg!(test) { Duration::from_secs(2) } else { Duration::from_secs(30) };
/// Documents one server keeps open; past it, the least recently asked closes.
const OPEN_DOCS: usize = 32;

/// What `D` or `R` asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Definition,
    References,
}

/// One identifier on the asked line: its name and char column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Occurrence {
    pub name: String,
    pub column: usize,
}

/// What a query asked about, echoed on every answer so a stale one never lands.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Tag {
    /// The pane's own count: each query, and each re-ask after a hide or a new config, is new.
    pub generation: u64,
    /// The repo-relative file shown.
    pub path: String,
    /// [`content_hash`] of the text shown.
    pub hash: u64,
    /// The asked line, 1-based.
    pub line: u32,
}

#[derive(Clone, Debug)]
pub struct Query {
    pub tag: Tag,
    pub kind: Kind,
    /// Each with its index in the pane's list, which its answer carries back.
    pub occurrences: Vec<(usize, Occurrence)>,
}

/// One request to the worker.
#[derive(Debug)]
pub enum LspJob {
    Query(Query),
    /// The list closed: every request in flight is dropped.
    Cancel,
    /// Paths the watcher saw change, relative to the repo.
    Changed {
        paths: Vec<String>,
    },
    /// Changes were lost: every server stops, and the next query starts it again.
    Rescan,
    /// A new config: servers whose program or arguments changed stop, their requests unanswered.
    Reconfigure(Box<PluginConfig>),
}

/// One place an answer points at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub place: Located,
    /// 1-based.
    pub line: u32,
    /// The line's text, trimmed: a definition's signature.
    pub text: String,
    pub context: Context,
}

impl Location {
    /// Where it points, its identity; the text and context only show it.
    pub fn spot(&self) -> (&Located, u32) {
        (&self.place, self.line)
    }
}

/// The lines around a location, untrimmed, that the list previews.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Context {
    /// 1-based line of `lines[0]`.
    pub from: u32,
    pub lines: Vec<String>,
}

/// How many lines each side of a location its context holds.
const CONTEXT: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Ready(Vec<Location>),
    /// The server's program is not there to run.
    NoServer {
        name: &'static str,
        hint: &'static str,
    },
    /// The file on disk is not the text shown.
    Mismatch,
    Failed(String),
    /// No server answers for this file.
    Unsupported,
}

/// One answer: for one occurrence, or for the whole query when `occurrence` is `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspCompletion {
    pub tag: Tag,
    pub occurrence: Option<usize>,
    pub outcome: Outcome,
}

/// Whether the pane is on screen; an answer is sent, and so wakes the pane, only under it.
pub type Gate = Arc<Mutex<bool>>;

/// The identifiers on `line`, left to right; a word starting with a digit is a number, not one.
pub fn occurrences(line: &str) -> Vec<Occurrence> {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut found = Vec::new();
    let mut chars = line.chars().enumerate().peekable();
    while let Some((column, c)) = chars.next() {
        if !word(c) {
            continue;
        }
        let mut name = String::from(c);
        while let Some((_, next)) = chars.next_if(|&(_, next)| word(next)) {
            name.push(next);
        }
        if !c.is_numeric() {
            found.push(Occurrence { name, column });
        }
    }
    found
}

/// The hash a query's text is compared by, on both sides of the worker.
pub fn content_hash(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// The worker's input: the pane's jobs and its servers' frames, in one queue.
#[derive(Debug)]
enum Input {
    Job(LspJob),
    Frame { server: usize, epoch: u64, frame: Option<Value> },
    Stop,
}

/// The pane's end of the worker; dropping it stops every server.
#[derive(Debug)]
pub struct Jobs(mpsc::Sender<Input>);

impl Jobs {
    /// Jobs that land in `probe` instead of a worker, for tests of who sends what, in what order.
    #[cfg(test)]
    pub(crate) fn probe(probe: mpsc::Sender<LspJob>) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for input in rx {
                if let Input::Job(job) = input
                    && probe.send(job).is_err()
                {
                    break;
                }
            }
        });
        Self(tx)
    }

    /// Whether the worker took the job.
    pub fn send(&self, job: LspJob) -> bool {
        self.0.send(Input::Job(job)).is_ok()
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        let _ = self.0.send(Input::Stop);
    }
}

/// Run the worker until its [`Jobs`] drop; the handle joins once every server is gone.
pub fn spawn(
    repo: PathBuf,
    config: PluginConfig,
    tx: crate::wake::Sender<LspCompletion>,
    gate: Gate,
) -> (Jobs, std::thread::JoinHandle<()>) {
    let (input, rx) = mpsc::channel();
    let worker = Worker {
        rx,
        backlog: VecDeque::new(),
        repo,
        config,
        servers: (0..servers::SERVERS.len()).map(|_| None).collect(),
        input: input.clone(),
        tx,
        gate,
        epoch: 0,
        tick: 0,
    };
    let handle = std::thread::Builder::new()
        .name("lsp".into())
        .spawn(move || worker.run())
        .expect("spawn lsp worker");
    (Jobs(input), handle)
}

/// A request in flight: whom to answer, and what was asked.
#[derive(Clone, Debug)]
struct Asked {
    tag: Tag,
    kind: Kind,
    index: usize,
    occurrence: Occurrence,
}

/// One document a server holds open.
#[derive(Debug)]
struct Doc {
    hash: u64,
    version: i64,
    /// When it was last asked about, for closing the least recent.
    used: u64,
}

/// A started server.
#[derive(Debug)]
struct Live {
    conn: Connection,
    epoch: u64,
    argv: Vec<String>,
    /// `Some` until `initialize` is answered: its request and what waits on it.
    starting: Option<(u64, Vec<Asked>)>,
    /// Positions count UTF-8 bytes, else UTF-16 units.
    utf8: bool,
    /// Paths changed while it started, told once it is up.
    unsent: BTreeSet<String>,
    docs: HashMap<PathBuf, Doc>,
    asked: HashMap<u64, Asked>,
}

struct Worker {
    rx: mpsc::Receiver<Input>,
    /// Input that arrived while a server was being shut down, handled before the queue.
    backlog: VecDeque<Input>,
    repo: PathBuf,
    config: PluginConfig,
    servers: Vec<Option<Live>>,
    /// Handed to each server's reader, so its frames join the queue.
    input: mpsc::Sender<Input>,
    tx: crate::wake::Sender<LspCompletion>,
    gate: Gate,
    epoch: u64,
    tick: u64,
}

impl Worker {
    fn run(mut self) {
        loop {
            let next = if let Some(input) = self.backlog.pop_front() {
                Ok(input)
            } else {
                let servers = self.servers.iter().flatten();
                match servers.filter_map(|s| s.conn.next_deadline()).min() {
                    Some(at) => self.rx.recv_timeout(at.saturating_duration_since(Instant::now())),
                    None => self.rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                }
            };
            match next {
                Ok(Input::Job(job)) => self.job(job),
                Ok(Input::Frame { server, epoch, frame }) => self.frame(server, epoch, frame),
                Ok(Input::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            self.expire();
        }
        for index in 0..self.servers.len() {
            self.stop(index);
        }
    }

    /// Shut server `index` down within its grace, keeping whatever else arrives meanwhile.
    fn stop(&mut self, index: usize) {
        let Some(live) = self.servers[index].take() else { return };
        let (rx, backlog, epoch) = (&self.rx, &mut self.backlog, live.epoch);
        live.conn.shutdown(|at| {
            loop {
                match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                    Ok(Input::Frame { server, epoch: e, frame })
                        if server == index && e == epoch =>
                    {
                        break frame;
                    }
                    // Its other servers' frames and the pane's jobs wait their turn.
                    Ok(other) => backlog.push_back(other),
                    Err(_) => break None,
                }
            }
        });
    }

    fn job(&mut self, job: LspJob) {
        match job {
            LspJob::Query(query) => self.query(query),
            LspJob::Cancel => {
                for live in self.servers.iter_mut().flatten() {
                    for id in live.asked.drain().map(|(id, _)| id).collect::<Vec<_>>() {
                        live.conn.cancel(id);
                    }
                    if let Some((_, queued)) = &mut live.starting {
                        queued.clear();
                    }
                }
            }
            LspJob::Changed { paths } => self.changed(&paths),
            LspJob::Rescan => {
                for index in 0..self.servers.len() {
                    self.stop(index);
                }
            }
            LspJob::Reconfigure(config) => {
                self.config = *config;
                for (index, server) in servers::SERVERS.iter().enumerate() {
                    let argv = server.argv(&self.config);
                    if self.servers[index].as_ref().is_some_and(|live| live.argv != argv) {
                        self.stop(index);
                    }
                }
            }
        }
    }

    fn deliver(&self, tag: &Tag, occurrence: Option<usize>, outcome: Outcome) {
        // Checked and sent under one lock, so no answer slips past a pane that just hid.
        let visible = self.gate.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if *visible {
            let _ = self.tx.send(LspCompletion { tag: tag.clone(), occurrence, outcome });
        }
    }

    fn query(&mut self, query: Query) {
        let Some((index, server)) = servers::for_path(&query.tag.path) else {
            return self.deliver(&query.tag, None, Outcome::Unsupported);
        };
        // A file the disk no longer holds as shown starts no server.
        let text = crate::app::worktree_content(&self.repo, &query.tag.path).ok();
        if text.is_none_or(|t| content_hash(&t) != query.tag.hash) {
            return self.deliver(&query.tag, None, Outcome::Mismatch);
        }
        if self.servers[index].is_none() {
            let Some(cmd) = server.command(&self.config) else {
                let outcome = Outcome::NoServer { name: server.name, hint: server.install_hint };
                return self.deliver(&query.tag, None, outcome);
            };
            match self.start(index, cmd) {
                Ok(live) => self.servers[index] = Some(live),
                Err(e) => {
                    return self.deliver(
                        &query.tag,
                        None,
                        Outcome::Failed(format!("{}: {e}", server.name)),
                    );
                }
            }
        }
        let Query { tag, kind, occurrences } = query;
        let asked = occurrences.into_iter().map(|(index, occurrence)| Asked {
            tag: tag.clone(),
            kind,
            index,
            occurrence,
        });
        self.submit(index, asked.collect());
    }

    /// Ask server `index` now, or once it has started.
    fn submit(&mut self, index: usize, asked: Vec<Asked>) {
        let Some(live) = self.servers[index].as_mut() else { return };
        match &mut live.starting {
            Some((_, queued)) => queued.extend(asked),
            None => self.ask(index, asked),
        }
    }

    /// Start server `index` and ask it to `initialize`.
    fn start(&mut self, index: usize, cmd: std::process::Command) -> std::io::Result<Live> {
        self.epoch += 1;
        let (epoch, input) = (self.epoch, self.input.clone());
        let mut conn = Connection::spawn(cmd, move |frame| {
            let _ = input.send(Input::Frame { server: index, epoch, frame });
        })?;
        let root = uri::from_path(&self.repo).unwrap_or_default();
        let params = json!({
            "processId": std::process::id(),
            "rootUri": root,
            "workspaceFolders": [{"uri": root, "name": "worktree"}],
            "capabilities": {
                "general": {"positionEncodings": ["utf-8", "utf-16"]},
                "textDocument": {"definition": {"linkSupport": true}, "references": {}},
                "workspace": {"didChangeWatchedFiles": {"dynamicRegistration": true}, "workspaceFolders": true},
            },
        });
        let id = conn.request("initialize", &params, REQUEST_TIMEOUT);
        let argv = servers::SERVERS[index].argv(&self.config);
        let live = Live {
            conn,
            epoch,
            argv,
            starting: Some((id, Vec::new())),
            utf8: false,
            unsent: BTreeSet::new(),
            docs: HashMap::new(),
            asked: HashMap::new(),
        };
        Ok(live)
    }

    /// Bring each asked file up to date on server `index`, then ask about each occurrence.
    fn ask(&mut self, index: usize, asked: Vec<Asked>) {
        let mut texts: HashMap<Tag, Option<String>> = HashMap::new();
        for a in asked {
            let text = texts.entry(a.tag.clone()).or_insert_with(|| {
                let text = crate::app::worktree_content(&self.repo, &a.tag.path).ok();
                let text = text.filter(|t| content_hash(t) == a.tag.hash);
                if text.is_none() {
                    self.deliver(&a.tag, None, Outcome::Mismatch);
                }
                text
            });
            let Some(text) = text.as_deref() else { continue };
            let path = self.repo.join(&a.tag.path);
            let Some(uri) = uri::from_path(&path) else { continue };
            self.tick += 1;
            let Some(live) = self.servers[index].as_mut() else { return };
            live.sync(
                &path,
                &uri,
                text,
                servers::SERVERS[index].language_id(&a.tag.path),
                self.tick,
            );
            let asked_line = text.lines().nth(a.tag.line.saturating_sub(1) as usize).unwrap_or("");
            let mut params = json!({
                "textDocument": {"uri": uri},
                "position": {
                    "line": a.tag.line.saturating_sub(1),
                    "character": position(asked_line, a.occurrence.column, live.utf8),
                },
            });
            let method = match a.kind {
                Kind::Definition => "textDocument/definition",
                Kind::References => {
                    params["context"] = json!({"includeDeclaration": true});
                    "textDocument/references"
                }
            };
            let id = live.conn.request(method, &params, REQUEST_TIMEOUT);
            live.asked.insert(id, a);
        }
        self.drop_stalled(index);
    }

    fn frame(&mut self, index: usize, epoch: u64, frame: Option<Value>) {
        let Some(live) = self.servers[index].as_mut().filter(|l| l.epoch == epoch) else { return };
        let Some(frame) = frame else {
            let name = servers::SERVERS[index].name;
            return self.fail(index, &format!("{name} exited"));
        };
        let Incoming::Response { id, result } = live.conn.receive(&frame) else {
            return self.drop_stalled(index);
        };
        if let Some((init, _)) = &live.starting
            && *init == id
        {
            let Ok(answer) = result else {
                let name = servers::SERVERS[index].name;
                return self.fail(index, &format!("{name} did not start"));
            };
            live.utf8 = answer["capabilities"]["positionEncoding"] == "utf-8";
            live.conn.notify("initialized", &json!({}));
            let queued = live.starting.take().map(|(_, q)| q).unwrap_or_default();
            let unsent: Vec<String> = std::mem::take(&mut live.unsent).into_iter().collect();
            self.tell(index, &unsent);
            return self.ask(index, queued);
        }
        let Some(asked) = live.asked.remove(&id) else { return };
        let outcome = match result {
            Ok(answer) => Outcome::Ready(locations(&self.repo, &answer)),
            Err(message) => Outcome::Failed(message),
        };
        self.deliver(&asked.tag, Some(asked.index), outcome);
    }

    fn changed(&mut self, paths: &[String]) {
        for index in 0..self.servers.len() {
            match self.servers[index].as_mut() {
                Some(live) if live.starting.is_some() => live.unsent.extend(paths.iter().cloned()),
                Some(_) => self.tell(index, paths),
                None => {}
            }
        }
    }

    /// Tell server `index` what changed on disk.
    fn tell(&mut self, index: usize, paths: &[String]) {
        self.tick += 1;
        let Some(live) = self.servers[index].as_mut() else { return };
        let mut changes = Vec::new();
        for rel in paths {
            let path = self.repo.join(rel);
            let Some(uri) = uri::from_path(&path) else { continue };
            // Changed or deleted: servers tell a new file by their own state, not by `Created`.
            let kind = if path.exists() { 2 } else { 3 };
            changes.push(json!({"uri": uri, "type": kind}));
            // An open document keeps the text it was sent: it hears the disk only from us.
            if live.docs.contains_key(&path) {
                match crate::app::worktree_content(&self.repo, rel) {
                    Ok(text) if kind != 3 => {
                        let language = servers::SERVERS[index].language_id(rel);
                        live.sync(&path, &uri, &text, language, self.tick);
                    }
                    _ => live.close(&path, &uri),
                }
            }
        }
        if !changes.is_empty() {
            live.conn.notify("workspace/didChangeWatchedFiles", &json!({"changes": changes}));
        }
        self.drop_stalled(index);
    }

    /// Fail what server `index` was asked, and stop it; the next query starts it again.
    fn fail(&mut self, index: usize, message: &str) {
        let Some(mut live) = self.servers[index].take() else { return };
        let queued = live.starting.take().map(|(_, q)| q).unwrap_or_default();
        for asked in queued.into_iter().chain(live.asked.into_values()) {
            self.deliver(&asked.tag, Some(asked.index), Outcome::Failed(message.to_string()));
        }
    }

    fn drop_stalled(&mut self, index: usize) {
        if self.servers[index].as_ref().is_some_and(|l| l.conn.stalled()) {
            let name = servers::SERVERS[index].name;
            self.fail(index, &format!("{name} stopped answering"));
        }
    }

    /// A server past a request's deadline counts as hung: it stops, and all it was asked fails.
    fn expire(&mut self) {
        let now = Instant::now();
        for index in 0..self.servers.len() {
            if self.servers[index].as_mut().is_some_and(|live| !live.conn.expired(now).is_empty()) {
                let name = servers::SERVERS[index].name;
                self.fail(index, &format!("{name} did not answer in time"));
            }
        }
    }
}

impl Live {
    /// Open the document, or send its new text if the server holds an older one.
    fn sync(&mut self, path: &Path, uri: &str, text: &str, language: &str, tick: u64) {
        let hash = content_hash(text);
        match self.docs.get_mut(path) {
            Some(doc) if doc.hash == hash => doc.used = tick,
            Some(doc) => {
                doc.version += 1;
                (doc.hash, doc.used) = (hash, tick);
                let params = json!({
                    "textDocument": {"uri": uri, "version": doc.version},
                    "contentChanges": [{"text": text}],
                });
                self.conn.notify("textDocument/didChange", &params);
            }
            None => {
                if self.docs.len() >= OPEN_DOCS
                    && let Some(oldest) =
                        self.docs.iter().min_by_key(|(_, d)| d.used).map(|(p, _)| p.clone())
                    && let Some(oldest_uri) = uri::from_path(&oldest)
                {
                    self.close(&oldest, &oldest_uri);
                }
                let params = json!({"textDocument": {
                    "uri": uri, "languageId": language, "version": 1, "text": text,
                }});
                self.conn.notify("textDocument/didOpen", &params);
                self.docs.insert(path.to_path_buf(), Doc { hash, version: 1, used: tick });
            }
        }
    }

    fn close(&mut self, path: &Path, uri: &str) {
        if self.docs.remove(path).is_some() {
            self.conn.notify("textDocument/didClose", &json!({"textDocument": {"uri": uri}}));
        }
    }
}

/// The places an answer names, deduplicated, each with its line's text.
fn locations(repo: &Path, answer: &Value) -> Vec<Location> {
    let items = match answer {
        Value::Array(items) => items.iter().collect(),
        Value::Null => Vec::new(),
        single => vec![single],
    };
    let mut found: Vec<(PathBuf, u32)> = Vec::new();
    for item in items {
        // A `LocationLink` names its target; a `Location` is its own.
        let (uri, range) = match item.get("targetUri") {
            Some(uri) => (uri, &item["targetSelectionRange"]),
            None => (&item["uri"], &item["range"]),
        };
        let (Some(path), Some(line)) =
            (uri.as_str().and_then(uri::to_path), range["start"]["line"].as_u64())
        else {
            continue;
        };
        let at = (path, u32::try_from(line).unwrap_or(u32::MAX).saturating_add(1));
        if !found.contains(&at) {
            found.push(at);
        }
    }
    let places: Vec<Located> = found.iter().map(|(path, _)| uri::locate(repo, path)).collect();
    let repo_paths = places.iter().filter_map(|p| match p {
        Located::Repo(rel) => Some(rel.as_str()),
        Located::External(_) => None,
    });
    // A file git ignores is not one the navigator lists; when git cannot say, none counts as listed.
    let ignored = crate::git::check_ignore(repo, repo_paths);
    let mut texts: HashMap<PathBuf, Vec<String>> = HashMap::new();
    found
        .into_iter()
        .zip(places)
        .map(|((path, line), place)| {
            let place = match place {
                Located::Repo(rel) if ignored.as_ref().is_none_or(|i| i.contains(&rel)) => {
                    Located::External(path.clone())
                }
                place => place,
            };
            let lines = texts.entry(path.clone()).or_insert_with(|| {
                read_capped(&path).map_or_else(Vec::new, |t| t.lines().map(Into::into).collect())
            });
            let at = line as usize - 1;
            let text = lines.get(at).map_or("", |t| t.trim()).to_string();
            let from = at.saturating_sub(CONTEXT).min(lines.len());
            let around = lines[from..(at + CONTEXT + 1).min(lines.len())].to_vec();
            let context = Context { from: from as u32 + 1, lines: around };
            Location { place, line, text, context }
        })
        .collect()
}

/// A char column as the server counts: UTF-8 bytes or UTF-16 units.
fn position(line: &str, column: usize, utf8: bool) -> usize {
    line.chars().take(column).map(|c| if utf8 { c.len_utf8() } else { c.len_utf16() }).sum()
}

/// A regular file's text for showing a line of it, within the diff's byte budget.
fn read_capped(path: &Path) -> Option<String> {
    use std::io::Read;
    // A FIFO would block the open until a writer came.
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
        return None;
    }
    let mut bytes = Vec::new();
    let file = std::fs::File::open(path).ok()?;
    file.take(crate::diff::MAX_BYTES as u64).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::{Gate, Jobs, Kind, LspCompletion, LspJob, Occurrence, Outcome, Query, Tag};
    use super::{content_hash, uri::Located};
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(20);

    /// A git repo whose `.go` files the fake server answers, and the worker running it.
    struct Rig {
        repo: tempfile::TempDir,
        jobs: Option<Jobs>,
        handle: Option<std::thread::JoinHandle<()>>,
        rx: Receiver<LspCompletion>,
        gate: Gate,
        generation: u64,
    }

    fn config_for(fake: &Path) -> crate::config::PluginConfig {
        let dir = tempfile::tempdir().unwrap();
        let text = format!("gopls = {:?}\nclangd = {:?}\n", fake.display(), fake.display());
        std::fs::write(dir.path().join("config.toml"), text).unwrap();
        crate::config::plugin_config_in(dir.path()).unwrap()
    }

    impl Rig {
        fn new(flags: &[&str]) -> Self {
            let (repo, _git) = crate::test_support::test_repo();
            let script = repo.path().join(".fake-lsp");
            std::fs::create_dir(&script).unwrap();
            for flag in flags {
                std::fs::write(script.join(flag), "").unwrap();
            }
            let config = config_for(crate::test_support::fake_lsp());
            let (tx, rx) = crate::wake::channel(&crate::wake::Waker::detached());
            let gate: Gate = std::sync::Arc::new(std::sync::Mutex::new(true));
            let (jobs, handle) = super::spawn(repo.path().to_path_buf(), config, tx, gate.clone());
            Self { repo, jobs: Some(jobs), handle: Some(handle), rx, gate, generation: 0 }
        }

        fn script(&self, file: &str, value: &serde_json::Value) {
            std::fs::write(self.repo.path().join(".fake-lsp").join(file), value.to_string())
                .unwrap();
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.repo.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }

        fn uri(&self, rel: &str) -> String {
            super::uri::from_path(&self.repo.path().join(rel)).unwrap()
        }

        /// Ask `kind` about the named words on `line` of `rel`, as the pane shows it now.
        fn ask(&mut self, kind: Kind, rel: &str, line: u32, words: &[&str]) -> Tag {
            let text = std::fs::read_to_string(self.repo.path().join(rel)).unwrap();
            let shown = text.lines().nth(line as usize - 1).unwrap();
            let occurrences = words
                .iter()
                .map(|w| Occurrence {
                    name: (*w).to_string(),
                    column: shown[..shown.find(w).unwrap()].chars().count(),
                })
                .enumerate()
                .collect();
            self.generation += 1;
            let tag = Tag {
                generation: self.generation,
                path: rel.into(),
                hash: content_hash(&text),
                line,
            };
            let query = Query { tag: tag.clone(), kind, occurrences };
            assert!(self.jobs.as_ref().unwrap().send(LspJob::Query(query)));
            tag
        }

        fn next(&self) -> LspCompletion {
            self.rx.recv_timeout(WAIT).expect("an answer")
        }

        fn log(&self) -> String {
            std::fs::read_to_string(self.repo.path().join(".fake-lsp/lsp.log")).unwrap_or_default()
        }

        #[cfg(unix)]
        fn pid(&self) -> String {
            std::fs::read_to_string(self.repo.path().join(".fake-lsp/pid")).unwrap()
        }

        /// Wait until the fake has logged a frame `has` matches.
        fn logged(&self, has: impl Fn(&str) -> bool) {
            let until = std::time::Instant::now() + WAIT;
            while !self.log().lines().any(&has) {
                assert!(std::time::Instant::now() < until, "never logged: {}", self.log());
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn stop(&mut self) {
            drop(self.jobs.take());
            self.handle.take().unwrap().join().unwrap();
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            if self.handle.is_some() {
                self.stop();
            }
        }
    }

    fn location(uri: &str, line: u32) -> serde_json::Value {
        let at = json!({"line": line, "character": 0});
        json!({"uri": uri, "range": {"start": at, "end": at}})
    }

    #[cfg(unix)]
    fn alive(pid: &str) -> bool {
        std::process::Command::new("kill").args(["-0", pid]).status().unwrap().success()
    }

    #[test]
    fn a_line_splits_into_its_identifiers_left_to_right() {
        let names = |line: &str| {
            super::occurrences(line).into_iter().map(|o| (o.name, o.column)).collect::<Vec<_>>()
        };
        let cpp = names("  auto user = loadUser(id);");
        assert_eq!(
            cpp,
            [("auto".into(), 2), ("user".into(), 7), ("loadUser".into(), 14), ("id".into(), 23)]
        );
        let go = names("x := foo(0x1F, 2e3) + foo(y)");
        let go: Vec<&str> = go.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(go, ["x", "foo", "foo", "y"], "numbers are no identifiers, repeats stay");
        assert_eq!(names("💡 ünï_1"), [("ünï_1".into(), 2)]);
        assert!(names("  {}();").is_empty());
    }

    #[test]
    fn an_occurrence_right_of_wide_characters_is_asked_at_its_own_position() {
        for flags in [&[][..], &["utf8"][..]] {
            let mut rig = Rig::new(flags);
            rig.write("lib/defs.go", "package lib\n\nfunc Target() {}\n");
            rig.write("main.go", "package main\n\nvar s = \"💡💡💡\"; foo(bar)\n");
            rig.script("definition.json", &json!({"foo": location(&rig.uri("lib/defs.go"), 2)}));
            let tag = rig.ask(Kind::Definition, "main.go", 3, &["foo", "bar"]);
            let mut answers = [rig.next(), rig.next()];
            answers.sort_by_key(|a| a.occurrence);
            let target = super::Location {
                place: Located::Repo("lib/defs.go".into()),
                line: 3,
                text: "func Target() {}".into(),
                context: super::Context {
                    from: 1,
                    lines: vec!["package lib".into(), String::new(), "func Target() {}".into()],
                },
            };
            assert_eq!(
                answers[0],
                LspCompletion {
                    tag: tag.clone(),
                    occurrence: Some(0),
                    outcome: Outcome::Ready(vec![target]),
                },
                "{flags:?}"
            );
            assert_eq!(answers[1].outcome, Outcome::Ready(Vec::new()), "`bar` has none: {flags:?}");
        }
    }

    #[test]
    fn a_file_changed_after_it_was_asked_is_answered_from_its_new_text() {
        let mut rig = Rig::new(&[]);
        rig.write("a.go", "package a\n\nvar x = old()\n");
        let defs =
            json!({"old": location(&rig.uri("a.go"), 0), "new": location(&rig.uri("a.go"), 1)});
        rig.script("definition.json", &defs);
        rig.ask(Kind::Definition, "a.go", 3, &["old"]);
        assert!(matches!(&rig.next().outcome, Outcome::Ready(l) if l[0].line == 1));
        // The fake answers from the text it holds, never the disk: only a resync names `new`.
        rig.write("a.go", "package a\n\nvar x = new()\n");
        rig.ask(Kind::Definition, "a.go", 3, &["new"]);
        assert!(matches!(&rig.next().outcome, Outcome::Ready(l) if l[0].line == 2));
        assert!(rig.log().contains("textDocument/didChange"));
    }

    #[test]
    fn text_unlike_the_disk_is_a_mismatch_and_an_unknown_file_unsupported() {
        let mut rig = Rig::new(&[]);
        rig.write("a.go", "package a\n");
        let mut tag = rig.ask(Kind::Definition, "a.go", 1, &["a"]);
        rig.next();
        rig.write("a.go", "package b\n");
        tag.generation += 1;
        let occurrences = vec![(0, Occurrence { name: "a".into(), column: 1 })];
        let stale = Query { tag: tag.clone(), kind: Kind::Definition, occurrences };
        rig.jobs.as_ref().unwrap().send(LspJob::Query(stale));
        assert_eq!(rig.next(), LspCompletion { tag, occurrence: None, outcome: Outcome::Mismatch });
        rig.write("README.md", "# a\n");
        let tag = rig.ask(Kind::Definition, "README.md", 1, &["a"]);
        assert_eq!(
            rig.next(),
            LspCompletion { tag, occurrence: None, outcome: Outcome::Unsupported }
        );
    }

    #[test]
    fn closing_the_list_cancels_what_is_in_flight() {
        let mut rig = Rig::new(&["silent"]);
        rig.write("a.go", "package a\n");
        rig.ask(Kind::References, "a.go", 1, &["a"]);
        rig.logged(|l| l.contains("textDocument/references") && l.contains("includeDeclaration"));
        rig.jobs.as_ref().unwrap().send(LspJob::Cancel);
        rig.logged(|l| l.contains("$/cancelRequest"));
    }

    #[test]
    fn a_hidden_pane_hears_no_answer_even_one_racing_the_hide() {
        let mut rig = Rig::new(&[]);
        rig.write("a.go", "package a\n");
        rig.script("definition.json", &json!({"a": location(&rig.uri("a.go"), 0)}));
        // Holding the gate stalls the worker exactly between the answer and its send.
        let gate = rig.gate.clone();
        let mut gate = gate.lock().unwrap();
        rig.ask(Kind::Definition, "a.go", 1, &["a"]);
        rig.logged(|l| l.contains("textDocument/definition"));
        std::thread::sleep(Duration::from_millis(200));
        *gate = false;
        drop(gate);
        assert!(rig.rx.recv_timeout(Duration::from_millis(500)).is_err(), "no answer, no wake");
    }

    #[cfg(unix)]
    #[test]
    fn stopping_or_reconfiguring_shuts_the_server_down_and_drops_its_answers() {
        let mut rig = Rig::new(&[]);
        rig.write("a.go", "package a\n");
        rig.ask(Kind::Definition, "a.go", 1, &["a"]);
        rig.next();
        let first = rig.pid();
        let dir = tempfile::tempdir().unwrap();
        let fake = crate::test_support::fake_lsp();
        let reconfigure = |rig: &Rig, text: String| {
            std::fs::write(dir.path().join("config.toml"), text).unwrap();
            let config = crate::config::plugin_config_in(dir.path()).unwrap();
            rig.jobs.as_ref().unwrap().send(LspJob::Reconfigure(Box::new(config)));
        };
        // clangd's index changed, gopls's argv did not: gopls keeps running.
        reconfigure(&rig, format!("gopls = {:?}\nclangd_index = true\n", fake.display()));
        std::thread::sleep(Duration::from_millis(200));
        assert!(alive(&first));
        // A request in flight when its server is replaced is never answered.
        std::fs::write(rig.repo.path().join(".fake-lsp/silent"), "").unwrap();
        rig.ask(Kind::Definition, "a.go", 1, &["a"]);
        rig.logged(|l| l.matches("textDocument/definition").count() > 0 && l.contains(r#""id":3"#));
        let link = rig.repo.path().join("gopls2");
        std::os::unix::fs::symlink(fake, &link).unwrap();
        reconfigure(&rig, format!("gopls = {:?}\n", link.display()));
        rig.logged(|l| l.contains(r#""method":"exit""#));
        let log = rig.log();
        assert!(log.find(r#""method":"shutdown""#) < log.find(r#""method":"exit""#), "{log}");
        assert!(rig.rx.recv_timeout(Duration::from_millis(300)).is_err(), "no answer");
        let until = std::time::Instant::now() + WAIT;
        while alive(&first) {
            assert!(std::time::Instant::now() < until, "the old server outlived its config");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::remove_file(rig.repo.path().join(".fake-lsp/silent")).unwrap();
        rig.ask(Kind::Definition, "a.go", 1, &["a"]);
        rig.next();
        let second = rig.pid();
        assert_ne!(first, second);
        rig.stop();
        assert!(!alive(&second), "quit ends the server");
    }

    #[test]
    fn the_watcher_reaches_files_the_server_never_opened() {
        let mut rig = Rig::new(&[]);
        rig.write("a.go", "package a\n");
        rig.write("old.go", "package a\n");
        rig.write("gone.go", "package a\n");
        rig.ask(Kind::Definition, "a.go", 1, &["a"]);
        rig.next();
        rig.write("new.go", "package a\n");
        rig.write("old.go", "package a // edited\n");
        std::fs::remove_file(rig.repo.path().join("gone.go")).unwrap();
        let paths = ["new.go", "old.go", "gone.go"].map(String::from).to_vec();
        rig.jobs.as_ref().unwrap().send(LspJob::Changed { paths });
        let change = |kind: u8, rel: &str| format!(r#"{{"type":{kind},"uri":"{}"}}"#, rig.uri(rel));
        let (new, changed, deleted) =
            (change(2, "new.go"), change(2, "old.go"), change(3, "gone.go"));
        rig.logged(|l| {
            l.contains("didChangeWatchedFiles")
                && l.contains(&new)
                && l.contains(&changed)
                && l.contains(&deleted)
        });
    }

    #[test]
    fn a_change_while_the_server_starts_is_told_once_it_is_up() {
        let mut rig = Rig::new(&["slow_init"]);
        rig.write("a.go", "package a\n");
        rig.ask(Kind::Definition, "a.go", 1, &["a"]);
        rig.logged(|l| l.contains(r#""method":"initialize""#));
        rig.write("b.go", "package a\n");
        rig.jobs.as_ref().unwrap().send(LspJob::Changed { paths: vec!["b.go".into()] });
        rig.next();
        let b = rig.uri("b.go");
        rig.logged(|l| l.contains("didChangeWatchedFiles") && l.contains(&b));
        let log = rig.log();
        let at = |m: &str| log.find(m).unwrap();
        assert!(at(r#""method":"initialized""#) < at(r#""method":"workspace/didChange"#), "{log}");
    }

    #[cfg(unix)]
    #[test]
    fn a_server_that_never_answers_is_stopped_and_the_next_query_starts_another() {
        let mut rig = Rig::new(&["silent"]);
        rig.write("a.go", "package a\n");
        let tag = rig.ask(Kind::Definition, "a.go", 1, &["a"]);
        rig.logged(|l| l.contains("textDocument/definition"));
        let hung = rig.pid();
        let answer = rig.next();
        assert_eq!((answer.tag, answer.occurrence), (tag, Some(0)));
        assert!(matches!(answer.outcome, Outcome::Failed(m) if m.contains("in time")));
        assert!(!alive(&hung), "a hung server is stopped");
        rig.ask(Kind::Definition, "a.go", 1, &["a"]);
        rig.logged(|l| l.contains("textDocument/definition"));
        let until = std::time::Instant::now() + WAIT;
        while rig.pid() == hung {
            assert!(std::time::Instant::now() < until, "no new server");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_place_that_is_a_fifo_is_listed_without_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe.go");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(made.success());
        let uri = super::uri::from_path(&fifo).unwrap();
        let found = super::locations(dir.path(), &location(&uri, 0));
        assert_eq!(found[0].text, "");
    }

    #[test]
    fn a_place_is_external_when_git_cannot_say_whether_it_is_ignored() {
        let plain = tempfile::tempdir().unwrap();
        std::fs::write(plain.path().join("a.go"), "package a\n").unwrap();
        let uri = super::uri::from_path(&plain.path().join("a.go")).unwrap();
        let found = super::locations(plain.path(), &location(&uri, 0));
        assert_eq!(found[0].place, Located::External(plain.path().join("a.go")));
    }

    #[test]
    fn a_place_git_ignores_or_outside_the_worktree_is_external() {
        let mut rig = Rig::new(&[]);
        rig.write(".gitignore", "build/\n");
        rig.write("build/gen.go", "package gen\n\nfunc Gen() {}\n");
        rig.write("a.go", "package a\n\nvar x = Gen()\n");
        let elsewhere = tempfile::tempdir().unwrap();
        let far = elsewhere.path().join("far.go");
        std::fs::write(&far, "func Far() {}\n").unwrap();
        let far_uri = super::uri::from_path(&far).unwrap();
        let link = json!({"targetUri": rig.uri("build/gen.go"), "targetRange": {}, "targetSelectionRange": {"start": {"line": 2, "character": 5}}});
        rig.script(
            "definition.json",
            &json!({"Gen": [link, location(&far_uri, 0), location(&rig.uri("a.go"), 2)]}),
        );
        rig.ask(Kind::Definition, "a.go", 3, &["Gen"]);
        let Outcome::Ready(found) = rig.next().outcome else { panic!("ready") };
        let places: Vec<_> = found.iter().map(|l| (&l.place, l.text.as_str())).collect();
        assert_eq!(
            places,
            [
                (&Located::External(rig.repo.path().join("build/gen.go")), "func Gen() {}"),
                (&Located::External(PathBuf::from(&far)), "func Far() {}"),
                (&Located::Repo("a.go".into()), "var x = Gen()"),
            ]
        );
    }
}
