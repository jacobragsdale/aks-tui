//! `kubectl`: what a pod is, how a cluster is asked about one, and the thread
//! that keeps asking.
//!
//! Nothing here is stored beyond the cache the next start paints from. A pod
//! is read live and the next read replaces it. The worker has its own thread
//! and its own `kubectl` processes, so the screen never waits on a cluster.
//!
//! Lifted from ticket-tui's AKS tab (`6f73eef^:src/aks.rs`), with the
//! per-cluster sweep turned into a per-scope cadence: the open tab is read
//! every few seconds, the others every half minute for their badges.

use std::cell::Cell;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Scope;
use crate::timestamp::Timestamp;

/// How often the open tab is read when `config.toml` does not say.
pub const DEFAULT_REFRESH: Duration = Duration::from_secs(5);

/// How often a tab nobody is looking at is read, for its badge.
pub const HIDDEN_REFRESH: Duration = Duration::from_secs(30);

/// How far a failing scope's cadence stretches, doubling each time.
const MAX_CADENCE: Duration = Duration::from_secs(120);

/// The bound on every one-shot `kubectl` call's request. A cluster that
/// cannot be reached answers in ten seconds rather than never.
const REQUEST_TIMEOUT: &str = "--request-timeout=10s";

/// The bound on the whole call, request or not: a credential plugin waiting
/// on a device-code login is the one thing `--request-timeout` cannot end.
const CALL_CAP: Duration = Duration::from_secs(20);

/// One pod, by where it lives.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct PodKey {
    /// The cluster's name in `config.toml`, not its context.
    pub cluster: String,
    pub namespace: String,
    pub name: String,
}

/// One container of a pod, as its status reports it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Container {
    pub name: String,
    pub image: String,
    pub ready: bool,
    pub restarts: u32,
    /// `Running`, or the reason it is waiting or has stopped:
    /// `CrashLoopBackOff`, `Completed`, `ExitCode:137`.
    pub state: String,
    /// Why it last stopped, and with what code, when it has stopped before.
    pub last_termination: Option<(String, i64)>,
}

/// One pod, as `kubectl get pods` would print it, with what the details pane
/// wants besides.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Pod {
    pub key: PodKey,
    /// The STATUS word: `Running`, `CrashLoopBackOff`, `Init:1/2`, …
    pub status: String,
    /// Containers ready, and containers in the spec.
    pub ready: (usize, usize),
    pub restarts: u32,
    pub created: Option<Timestamp>,
    pub node: String,
    pub ip: String,
    /// What made it, as `(kind, name)`: `("Deployment", "orders-api")`.
    pub owner: Option<(String, String)>,
    pub containers: Vec<Container>,
    /// Every label, sorted by key.
    pub labels: Vec<(String, String)>,
}

impl Pod {
    /// One `items[]` entry of `kubectl get pods -o json`. `None` for an entry
    /// with no name or namespace, which is not a pod.
    #[must_use]
    pub fn from_json(cluster: &str, item: &Value) -> Option<Self> {
        let metadata = &item["metadata"];
        let name = metadata["name"].as_str()?;
        let namespace = metadata["namespace"].as_str()?;
        let statuses = item["status"]["containerStatuses"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let containers: Vec<Container> = item["spec"]["containers"]
            .as_array()
            .map(|specs| {
                specs
                    .iter()
                    .filter_map(|spec| {
                        let name = spec["name"].as_str()?;
                        let status = statuses
                            .iter()
                            .find(|status| status["name"].as_str() == Some(name));
                        Some(container(name, spec, status))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut labels: Vec<(String, String)> = metadata["labels"]
            .as_object()
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|(key, value)| Some((key.clone(), value.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        labels.sort();
        Some(Self {
            key: PodKey {
                cluster: cluster.to_owned(),
                namespace: namespace.to_owned(),
                name: name.to_owned(),
            },
            status: status_word(item),
            ready: (
                containers.iter().filter(|held| held.ready).count(),
                containers.len(),
            ),
            restarts: containers.iter().map(|held| held.restarts).sum(),
            created: metadata["creationTimestamp"]
                .as_str()
                .and_then(Timestamp::parse),
            node: item["spec"]["nodeName"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            ip: item["status"]["podIP"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            owner: owner_of(item),
            containers,
            labels,
        })
    }

    #[must_use]
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels
            .iter()
            .find(|(held, _)| held == key)
            .map(|(_, value)| value.as_str())
    }

    /// `1/2`, the READY column.
    #[must_use]
    pub fn ready_label(&self) -> String {
        format!("{}/{}", self.ready.0, self.ready.1)
    }

    /// `Deployment/orders-api`, or a dash for a pod nothing put there.
    #[must_use]
    pub fn owner_label(&self) -> String {
        self.owner.as_ref().map_or_else(
            || "\u{2014}".to_owned(),
            |(kind, name)| format!("{kind}/{name}"),
        )
    }

    /// What `owner:` filters on: `orders-api`.
    #[must_use]
    pub fn owner_name(&self) -> &str {
        self.owner.as_ref().map_or("", |(_, name)| name.as_str())
    }

    /// Whether deleting it restarts anything: a pod with a controller is put
    /// back by that controller, a bare pod is simply gone.
    #[must_use]
    pub const fn restartable(&self) -> bool {
        self.owner.is_some()
    }

    /// Whether the STATUS word is one somebody has to look at.
    #[must_use]
    pub fn is_unhealthy(&self) -> bool {
        unhealthy_word(self.status.strip_prefix("Init:").unwrap_or(&self.status))
    }

    /// The glyph the conventions give the pod: `●` running and ready, `◐`
    /// on its way somewhere, `✓` finished, `✗` in trouble, `○` anything else.
    #[must_use]
    pub fn glyph(&self) -> &'static str {
        if self.is_unhealthy() {
            "\u{2717}"
        } else if self.status == "Running" && self.ready.1 > 0 && self.ready.0 == self.ready.1 {
            "\u{25cf}"
        } else if matches!(self.status.as_str(), "Completed" | "Succeeded") {
            "\u{2713}"
        } else if matches!(
            self.status.as_str(),
            "Running" | "Pending" | "ContainerCreating" | "PodInitializing" | "Terminating"
        ) || self.status.starts_with("Init:")
        {
            "\u{25d0}"
        } else {
            "\u{25cb}"
        }
    }

    /// The name of the container the log follows when nobody has chosen one.
    #[must_use]
    pub fn first_container(&self) -> Option<&str> {
        self.containers.first().map(|held| held.name.as_str())
    }

    /// The `app` label, or the `app.kubernetes.io/name` one: what `app:`
    /// filters on.
    #[must_use]
    pub fn app(&self) -> Option<&str> {
        self.label("app")
            .or_else(|| self.label("app.kubernetes.io/name"))
    }
}

/// One container, joined from its spec and its status.
fn container(name: &str, spec: &Value, status: Option<&Value>) -> Container {
    let state = status.map(|status| &status["state"]);
    let word = state.map_or_else(
        || "Waiting".to_owned(),
        |state| {
            if !state["running"].is_null() {
                "Running".to_owned()
            } else if let Some(reason) = non_empty(&state["waiting"]["reason"]) {
                reason.to_owned()
            } else if !state["terminated"].is_null() {
                termination_word(&state["terminated"])
            } else {
                "Waiting".to_owned()
            }
        },
    );
    let last = status
        .map(|status| &status["lastState"]["terminated"])
        .filter(|terminated| !terminated.is_null())
        .map(|terminated| {
            (
                non_empty(&terminated["reason"])
                    .unwrap_or("Terminated")
                    .to_owned(),
                terminated["exitCode"].as_i64().unwrap_or_default(),
            )
        });
    Container {
        name: name.to_owned(),
        image: status
            .and_then(|status| non_empty(&status["image"]))
            .or_else(|| non_empty(&spec["image"]))
            .unwrap_or_default()
            .to_owned(),
        ready: status.is_some_and(|status| status["ready"].as_bool() == Some(true)),
        restarts: status
            .and_then(|status| status["restartCount"].as_u64())
            .and_then(|count| u32::try_from(count).ok())
            .unwrap_or_default(),
        state: word,
        last_termination: last,
    }
}

fn non_empty(value: &Value) -> Option<&str> {
    value.as_str().filter(|held| !held.is_empty())
}

/// What a stopped container says: its reason, or its exit code when it gave
/// none.
fn termination_word(terminated: &Value) -> String {
    non_empty(&terminated["reason"]).map_or_else(
        || {
            format!(
                "ExitCode:{}",
                terminated["exitCode"].as_i64().unwrap_or_default()
            )
        },
        str::to_owned,
    )
}

/// The STATUS word `kubectl get pods` prints, cut to the cases that come up:
/// the pod's own reason or phase, overridden by the first init container
/// still going, else by whatever the containers are waiting on or stopped
/// for, and `Terminating` over all of it once a delete is in.
// ponytail: skipped from kubectl's printPod — sidecar init containers,
// Signal:N, NotReady, NodeLost→Unknown, and the "(N ago)" restart suffix.
fn status_word(item: &Value) -> String {
    let status = &item["status"];
    let phase = status["phase"].as_str().unwrap_or("Unknown");
    let mut word = non_empty(&status["reason"]).unwrap_or(phase).to_owned();
    let init_total = item["spec"]["initContainers"]
        .as_array()
        .map_or(0, Vec::len);
    let mut initializing = false;
    for (index, held) in status["initContainerStatuses"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let state = &held["state"];
        let terminated = &state["terminated"];
        if !terminated.is_null() {
            if terminated["exitCode"].as_i64() == Some(0) {
                continue;
            }
            word = format!("Init:{}", termination_word(terminated));
        } else if let Some(reason) =
            non_empty(&state["waiting"]["reason"]).filter(|reason| *reason != "PodInitializing")
        {
            word = format!("Init:{reason}");
        } else {
            word = format!("Init:{index}/{init_total}");
        }
        initializing = true;
        break;
    }
    if !initializing {
        let mut has_running = false;
        // Back to front, the way kubectl reads them, so the first container's
        // reason is the one that stands.
        for held in status["containerStatuses"]
            .as_array()
            .into_iter()
            .flatten()
            .rev()
        {
            let state = &held["state"];
            if let Some(reason) = non_empty(&state["waiting"]["reason"]) {
                word = reason.to_owned();
            } else if !state["terminated"].is_null() {
                word = termination_word(&state["terminated"]);
            } else if held["ready"].as_bool() == Some(true) && !state["running"].is_null() {
                has_running = true;
            }
        }
        if word == "Completed" && has_running {
            word = "Running".to_owned();
        }
    }
    if !item["metadata"]["deletionTimestamp"].is_null() && !matches!(phase, "Succeeded" | "Failed")
    {
        word = "Terminating".to_owned();
    }
    word
}

/// Whether a STATUS word, with any `Init:` in front of it removed, is one
/// somebody has to look at.
fn unhealthy_word(word: &str) -> bool {
    matches!(
        word,
        "CrashLoopBackOff"
            | "Error"
            | "ImagePullBackOff"
            | "ErrImagePull"
            | "InvalidImageName"
            | "CreateContainerConfigError"
            | "CreateContainerError"
            | "OOMKilled"
            | "Evicted"
            | "Failed"
            | "ContainerStatusUnknown"
            | "Unknown"
    ) || word.starts_with("ExitCode:")
}

/// What made the pod. A ReplicaSet named after a pod-template hash is a
/// Deployment's, and is reported as that Deployment, which is the name that
/// means something.
// ponytail: a Job's CronJob is not resolved; a ReplicaSet with no hash label
// stays a ReplicaSet.
fn owner_of(item: &Value) -> Option<(String, String)> {
    let references = item["metadata"]["ownerReferences"].as_array()?;
    let owner = references
        .iter()
        .find(|reference| reference["controller"].as_bool() == Some(true))
        .or_else(|| references.first())?;
    let kind = owner["kind"].as_str()?;
    let name = owner["name"].as_str()?;
    if kind == "ReplicaSet"
        && let Some(hash) = non_empty(&item["metadata"]["labels"]["pod-template-hash"])
        && let Some(base) = name.strip_suffix(&format!("-{hash}"))
    {
        return Some(("Deployment".to_owned(), base.to_owned()));
    }
    Some((kind.to_owned(), name.to_owned()))
}

/// Where the worker reads from. `kubectl` in the app; a fake in the tests.
pub trait KubeSource: Send {
    fn pods(&self, scope: &Scope) -> Result<Vec<Pod>>;
}

/// The real thing: `kubectl` on the path, with the context the scope names.
pub struct Kubectl;

impl Kubectl {
    /// `kubectl --context C --request-timeout=10s …`: its output, or the one
    /// line of its complaint that says what to fix.
    pub fn run(context: &str, arguments: &[&str]) -> Result<String> {
        let mut command = Command::new("kubectl");
        command
            .arg("--context")
            .arg(context)
            .arg(REQUEST_TIMEOUT)
            .args(arguments);
        run_capped(command, CALL_CAP)
    }
}

/// Runs one command to completion, or kills it at `cap`. Both pipes are
/// drained on threads of their own, so a child that fills one never blocks.
fn run_capped(mut command: Command, cap: Duration) -> Result<String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                anyhow!("{program} is not installed or not on PATH")
            } else {
                anyhow!("{program} could not be run: {error}")
            }
        })?;
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let deadline = Instant::now() + cap;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("{program} could not be waited for"))?
        {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(20));
    };
    let out = stdout.join().unwrap_or_default();
    let err = stderr.join().unwrap_or_default();
    match status {
        None => bail!(
            "{program} did not answer in {}s — a kubelogin waiting for a device-code login \
             looks like this; run `kubelogin convert-kubeconfig -l azurecli`",
            cap.as_secs()
        ),
        Some(status) if status.success() => Ok(out),
        Some(_) => bail!("{}", kubectl_error(&err)),
    }
}

/// Reads one pipe to its end on a thread of its own.
fn drain(pipe: Option<impl Read + Send + 'static>) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut pipe) = pipe {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            text = String::from_utf8_lossy(&bytes).into_owned();
        }
        text
    })
}

impl KubeSource for Kubectl {
    fn pods(&self, scope: &Scope) -> Result<Vec<Pod>> {
        let mut arguments = vec!["get", "pods", "-o", "json"];
        match &scope.namespace {
            Some(namespace) => arguments.extend(["-n", namespace]),
            None => arguments.push("--all-namespaces"),
        }
        let raw = Self::run(&scope.context, &arguments)?;
        let listed: Value = serde_json::from_str(&raw)
            .context("kubectl answered with something other than JSON")?;
        Ok(listed["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| Pod::from_json(&scope.cluster, item))
            .collect())
    }
}

/// The one line of `kubectl`'s complaint that says what to fix. The client
/// logs a retry or two before it gives up, and puts a documentation link
/// after the reason, so neither the first line nor the last is the one.
#[must_use]
pub fn kubectl_error(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !is_klog(line))
        .collect();
    let chosen = lines
        .iter()
        .find(|line| line.contains("az login"))
        .or_else(|| {
            lines.iter().find(|line| {
                line.starts_with("error:")
                    || line.starts_with("Error from server")
                    || line.starts_with("Unable to connect")
            })
        })
        .or_else(|| lines.first());
    chosen.map_or_else(
        || "kubectl failed".to_owned(),
        |line| {
            line.strip_prefix("error:")
                .map_or_else(|| (*line).to_owned(), |rest| rest.trim().to_owned())
        },
    )
}

/// `E0830 12:00:00.000000   12345 round_trippers.go:…] …`: the client's own
/// log line, which says nothing a person can act on.
fn is_klog(line: &str) -> bool {
    let mut characters = line.chars();
    matches!(characters.next(), Some('E' | 'W' | 'I' | 'F'))
        && characters.by_ref().take(4).all(|c| c.is_ascii_digit())
        && characters.next() == Some(' ')
}

/// When one scope is next worth reading. Something never polled is due at
/// once; a read that failed doubles the wait, up to two minutes; a base of
/// zero is "only when asked".
#[derive(Clone, Copy, Debug)]
pub struct Cadence {
    base: Duration,
    current: Duration,
    /// When it was last read, and when it is next due.
    last: Option<Instant>,
    due: Option<Instant>,
    /// Set by `r` or a tab switch: due now whatever the clock says.
    asked: bool,
}

impl Cadence {
    #[must_use]
    pub const fn new(base: Duration) -> Self {
        Self {
            base,
            current: base,
            last: None,
            due: None,
            asked: true,
        }
    }

    /// Changes how often this is read. The next read is the new interval
    /// after the last one: a tab just left is not read again five seconds
    /// later on its way to every thirty.
    pub fn set_base(&mut self, base: Duration) {
        if self.base != base {
            self.base = base;
            self.current = base;
            self.due = self.last.and_then(|last| self.next_after(last));
        }
    }

    /// When a read at `at` makes the next one due — never, on a base of
    /// zero.
    fn next_after(&self, at: Instant) -> Option<Instant> {
        if self.base.is_zero() {
            None
        } else {
            at.checked_add(self.current)
        }
    }

    /// Whether this is due at `now`.
    #[must_use]
    pub fn is_due(&self, now: Instant) -> bool {
        self.asked || self.due.is_some_and(|due| now >= due)
    }

    /// How long until it is due, or `None` while it never will be on its
    /// own.
    #[must_use]
    pub fn until_due(&self, now: Instant) -> Option<Duration> {
        if self.asked {
            return Some(Duration::ZERO);
        }
        self.due.map(|due| due.saturating_duration_since(now))
    }

    /// Records a poll, which sets the next one — never, on a base of zero.
    pub fn polled(&mut self, now: Instant, failed: bool) {
        self.asked = false;
        if failed {
            self.current = (self.current * 2).min(MAX_CADENCE).max(self.base);
        } else {
            self.current = self.base;
        }
        self.last = Some(now);
        self.due = self.next_after(now);
    }

    /// Due now, whatever the clock says.
    pub const fn ask(&mut self) {
        self.asked = true;
    }
}

/// What the run tells the worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request {
    /// Which tab is on screen, by index. It is read at once and then on the
    /// fast cadence; the others fall back to the slow one.
    Showing(usize),
    /// Read one scope again now.
    Refresh(usize),
    Stop,
}

/// What the worker sends back. Nothing here is written anywhere but the
/// cache: the screen shows it, and the next read replaces it.
#[derive(Debug)]
pub enum Event {
    /// A read of this scope has started, for the spinner.
    Reading(usize),
    /// One scope's pods, replacing the last read's and nothing else.
    Pods {
        scope: usize,
        pods: Result<Vec<Pod>, String>,
    },
    Stopped,
}

/// The worker's own state, apart from the thread it usually runs on, so a
/// test can drive it with a clock of its own.
pub struct Watcher {
    source: Box<dyn KubeSource>,
    events: Sender<Event>,
    /// Each scope and when it is next worth reading. One cadence each, so a
    /// dead cluster backing off never slows a live one.
    scopes: Vec<(Scope, Cadence)>,
    showing: Option<usize>,
    fast: Duration,
}

impl Watcher {
    #[must_use]
    pub fn new(
        source: Box<dyn KubeSource>,
        events: Sender<Event>,
        scopes: Vec<Scope>,
        fast: Duration,
    ) -> Self {
        Self {
            source,
            events,
            scopes: scopes
                .into_iter()
                .map(|scope| (scope, Cadence::new(HIDDEN_REFRESH)))
                .collect(),
            showing: None,
            fast,
        }
    }

    /// One request. Answers whether to keep going.
    pub fn handle(&mut self, request: Request) -> bool {
        match request {
            Request::Stop => return false,
            Request::Showing(index) => {
                if self.showing != Some(index) {
                    self.showing = Some(index);
                    for (at, (_, cadence)) in self.scopes.iter_mut().enumerate() {
                        cadence.set_base(if at == index {
                            self.fast
                        } else {
                            HIDDEN_REFRESH
                        });
                    }
                    if let Some((_, cadence)) = self.scopes.get_mut(index) {
                        cadence.ask();
                    }
                }
            }
            Request::Refresh(index) => {
                if let Some((_, cadence)) = self.scopes.get_mut(index) {
                    cadence.ask();
                }
            }
        }
        true
    }

    /// Reads one scope: the one on screen when it is due, else the first
    /// other that is. One read a call, so a request sent during a round is
    /// taken between two reads rather than after the last.
    pub fn poll(&mut self, now: Instant) {
        let Some(index) = self.next_due(now) else {
            return;
        };
        let (scope, cadence) = &mut self.scopes[index];
        let _ = self.events.send(Event::Reading(index));
        let pods = self
            .source
            .pods(scope)
            .map_err(|error| format!("{error:#}"));
        cadence.polled(now, pods.is_err());
        let _ = self.events.send(Event::Pods { scope: index, pods });
    }

    fn next_due(&self, now: Instant) -> Option<usize> {
        if let Some(index) = self.showing
            && self
                .scopes
                .get(index)
                .is_some_and(|(_, cadence)| cadence.is_due(now))
        {
            return Some(index);
        }
        self.scopes
            .iter()
            .position(|(_, cadence)| cadence.is_due(now))
    }

    /// Every read that is due at `now`, for a test that wants the whole
    /// round in one call.
    #[cfg(test)]
    pub(crate) fn poll_all(&mut self, now: Instant) {
        while self.next_due(now).is_some() {
            self.poll(now);
        }
    }

    /// How long until something is due, or `None` while nothing ever will be
    /// on its own.
    #[must_use]
    pub fn until_due(&self, now: Instant) -> Option<Duration> {
        self.scopes
            .iter()
            .filter_map(|(_, cadence)| cadence.until_due(now))
            .min()
    }
}

/// The handle the main thread holds: requests in, events out.
pub struct Handle {
    requests: Sender<Request>,
    events: Receiver<Event>,
    stopped: Cell<bool>,
    /// The thread, joined when the handle goes so a child it holds is killed
    /// before the process is: a process on its way out runs no destructor
    /// on another thread.
    thread: Option<thread::JoinHandle<()>>,
}

/// How long a quit waits for the worker to finish what it has in hand. A
/// read of an unreachable cluster can take ten seconds, and a quit is not
/// worth that; a worker with nothing in hand answers in a millisecond.
const STOP_GRACE: Duration = Duration::from_secs(2);

impl Handle {
    /// Starts the worker on its own thread. It ends when the handle is
    /// dropped.
    pub fn spawn(source: Box<dyn KubeSource>, scopes: Vec<Scope>, fast: Duration) -> Result<Self> {
        let (request_sender, request_receiver) = mpsc::channel();
        let (event_sender, event_receiver) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("aks-tui-kube".into())
            .spawn(move || {
                watch(
                    Watcher::new(source, event_sender, scopes, fast),
                    &request_receiver,
                );
            })
            .context("failed to start the cluster worker")?;
        Ok(Self {
            requests: request_sender,
            events: event_receiver,
            stopped: Cell::new(false),
            thread: Some(thread),
        })
    }

    /// Tells the worker what is worth doing. Fails only when it is gone.
    pub fn send(&self, request: Request) -> Result<()> {
        self.requests
            .send(request)
            .context("the cluster worker stopped")
    }

    /// The next event, if one is waiting.
    pub fn try_event(&self) -> Option<Event> {
        match self.events.try_recv() {
            Ok(event) => Some(event),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                (!self.stopped.replace(true)).then_some(Event::Stopped)
            }
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Stop);
        let Some(thread) = self.thread.take() else {
            return;
        };
        // Joined from a thread of its own, so the wait can be given up: a
        // worker mid-read finishes that read first, and the process does not
        // stand around for it.
        let (done, finished) = mpsc::channel();
        let _ = thread::Builder::new()
            .name("aks-tui-kube-stop".into())
            .spawn(move || {
                let _ = thread.join();
                let _ = done.send(());
            });
        let _ = finished.recv_timeout(STOP_GRACE);
    }
}

/// The loop: read whatever is due, then wait until the next thing is or a
/// request arrives, whichever comes first.
fn watch(mut watcher: Watcher, requests: &Receiver<Request>) {
    loop {
        watcher.poll(Instant::now());
        let wait = watcher
            .until_due(Instant::now())
            .unwrap_or(Duration::from_secs(3600));
        match requests.recv_timeout(wait) {
            Ok(request) => {
                if !watcher.handle(request) {
                    return;
                }
                // Everything else waiting is taken now, so a burst of requests
                // costs one poll rather than one each.
                while let Ok(request) = requests.try_recv() {
                    if !watcher.handle(request) {
                        return;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

    /// One pod as `kubectl get pods -o json` lists it, with the parts a case
    /// needs and nothing else.
    fn item(name: &str, extra: Value) -> Value {
        let mut base = json!({
            "metadata": {
                "name": name,
                "namespace": "dev",
                "creationTimestamp": "2026-08-30T10:00:00Z",
                "labels": {"app": "orders-api", "pod-template-hash": "7d9f5b"},
                "ownerReferences": [{"kind": "ReplicaSet", "name": "orders-api-7d9f5b", "controller": true}]
            },
            "spec": {
                "nodeName": "aks-nodepool1-0",
                "containers": [{"name": "api", "image": "myacr.azurecr.io/team/orders-api:1.2.3"}]
            },
            "status": {
                "phase": "Running",
                "podIP": "10.0.0.7",
                "containerStatuses": [{
                    "name": "api", "ready": true, "restartCount": 2,
                    "image": "myacr.azurecr.io/team/orders-api:1.2.3",
                    "state": {"running": {"startedAt": "2026-08-30T10:00:05Z"}},
                    "lastState": {"terminated": {"reason": "OOMKilled", "exitCode": 137}}
                }]
            }
        });
        merge(&mut base, extra);
        base
    }

    fn merge(base: &mut Value, extra: Value) {
        match (base, extra) {
            (Value::Object(base), Value::Object(extra)) => {
                for (key, value) in extra {
                    match base.get_mut(&key) {
                        Some(held) if held.is_object() && value.is_object() => merge(held, value),
                        _ => {
                            base.insert(key, value);
                        }
                    }
                }
            }
            (base, extra) => *base = extra,
        }
    }

    pub(crate) fn pod(cluster: &str, namespace: &str, name: &str, status: &str) -> Pod {
        Pod {
            key: PodKey {
                cluster: cluster.to_owned(),
                namespace: namespace.to_owned(),
                name: name.to_owned(),
            },
            status: status.to_owned(),
            ready: (1, 1),
            restarts: 0,
            created: Timestamp::parse("2026-08-30T10:00:00Z"),
            node: "aks-nodepool1-0".to_owned(),
            ip: "10.0.0.7".to_owned(),
            owner: Some(("Deployment".to_owned(), "orders-api".to_owned())),
            containers: vec![Container {
                name: "api".to_owned(),
                image: "myacr.azurecr.io/team/orders-api:1.2.3".to_owned(),
                ready: true,
                restarts: 0,
                state: "Running".to_owned(),
                last_termination: None,
            }],
            labels: vec![("app".to_owned(), "orders-api".to_owned())],
        }
    }

    /// A pod in trouble, which is what the badge counts and the glyph paints.
    pub(crate) fn crashing(cluster: &str, namespace: &str, name: &str) -> Pod {
        let mut pod = pod(cluster, namespace, name, "CrashLoopBackOff");
        pod.ready = (0, 1);
        pod.restarts = 9;
        pod.containers[0].ready = false;
        pod.containers[0].restarts = 9;
        pod.containers[0].state = "CrashLoopBackOff".to_owned();
        pod.containers[0].last_termination = Some(("Error".to_owned(), 1));
        pod
    }

    pub(crate) fn scope(cluster: &str, namespace: Option<&str>) -> Scope {
        Scope {
            cluster: cluster.to_owned(),
            context: format!("aks-{cluster}"),
            namespace: namespace.map(str::to_owned),
        }
    }

    #[test]
    fn a_pod_reads_its_ready_count_restarts_owner_node_and_containers_from_kubectls_json() {
        let pod = Pod::from_json("qa", &item("orders-api-7d9f5b-abc12", json!({}))).unwrap();
        assert_eq!(pod.key.cluster, "qa");
        assert_eq!(pod.key.namespace, "dev");
        assert_eq!(pod.key.name, "orders-api-7d9f5b-abc12");
        assert_eq!(pod.status, "Running");
        assert_eq!(pod.ready_label(), "1/1");
        assert_eq!(pod.restarts, 2);
        assert_eq!(pod.node, "aks-nodepool1-0");
        assert_eq!(pod.ip, "10.0.0.7");
        assert_eq!(
            pod.created.map(Timestamp::to_rfc3339).as_deref(),
            Some("2026-08-30T10:00:00Z")
        );
        assert_eq!(
            pod.owner,
            Some(("Deployment".to_owned(), "orders-api".to_owned()))
        );
        assert_eq!(pod.owner_label(), "Deployment/orders-api");
        assert_eq!(pod.owner_name(), "orders-api");
        assert_eq!(pod.containers.len(), 1);
        assert_eq!(pod.containers[0].state, "Running");
        assert_eq!(
            pod.containers[0].last_termination,
            Some(("OOMKilled".to_owned(), 137))
        );
        assert_eq!(pod.label("app"), Some("orders-api"));
        assert_eq!(pod.app(), Some("orders-api"));
        assert!(pod.restartable());
        assert_eq!(pod.glyph(), "\u{25cf}");
        assert!(Pod::from_json("qa", &json!({"metadata": {}})).is_none());
        // And it survives the cache.
        let written = serde_json::to_string(&pod).unwrap();
        assert_eq!(serde_json::from_str::<Pod>(&written).unwrap(), pod);
    }

    #[test]
    fn the_status_word_follows_kubectl_for_running_pending_creating_crashloop_error_completed_terminating_and_init()
     {
        let cases = [
            (json!({}), "Running", "\u{25cf}"),
            (
                json!({"status": {"phase": "Pending", "containerStatuses": []}}),
                "Pending",
                "\u{25d0}",
            ),
            (
                json!({"status": {"phase": "Pending", "containerStatuses": [
                    {"name": "api", "ready": false, "state": {"waiting": {"reason": "ContainerCreating"}}}
                ]}}),
                "ContainerCreating",
                "\u{25d0}",
            ),
            (
                json!({"status": {"containerStatuses": [
                    {"name": "api", "ready": false, "restartCount": 9, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
                ]}}),
                "CrashLoopBackOff",
                "\u{2717}",
            ),
            (
                json!({"status": {"phase": "Failed", "containerStatuses": [
                    {"name": "api", "ready": false, "state": {"terminated": {"reason": "Error", "exitCode": 1}}}
                ]}}),
                "Error",
                "\u{2717}",
            ),
            (
                json!({"status": {"phase": "Failed", "containerStatuses": [
                    {"name": "api", "ready": false, "state": {"terminated": {"exitCode": 137}}}
                ]}}),
                "ExitCode:137",
                "\u{2717}",
            ),
            (
                json!({"status": {"phase": "Succeeded", "containerStatuses": [
                    {"name": "api", "ready": false, "state": {"terminated": {"reason": "Completed", "exitCode": 0}}}
                ]}}),
                "Completed",
                "\u{2713}",
            ),
            // A sidecar that finished beside a server still running reads as
            // running, the way kubectl puts it back.
            (
                json!({"spec": {"containers": [{"name": "api"}, {"name": "init-db"}]},
                       "status": {"containerStatuses": [
                    {"name": "api", "ready": true, "state": {"running": {}}},
                    {"name": "init-db", "ready": false, "state": {"terminated": {"reason": "Completed", "exitCode": 0}}}
                ]}}),
                "Running",
                "\u{25d0}",
            ),
            (
                json!({"metadata": {"deletionTimestamp": "2026-08-30T11:00:00Z"}}),
                "Terminating",
                "\u{25d0}",
            ),
            (
                json!({"spec": {"initContainers": [{"name": "migrate"}, {"name": "seed"}]},
                       "status": {"phase": "Pending", "initContainerStatuses": [
                    {"name": "migrate", "state": {"terminated": {"exitCode": 0}}},
                    {"name": "seed", "state": {"running": {}}}
                ]}}),
                "Init:1/2",
                "\u{25d0}",
            ),
            (
                json!({"spec": {"initContainers": [{"name": "migrate"}]},
                       "status": {"phase": "Pending", "initContainerStatuses": [
                    {"name": "migrate", "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
                ]}}),
                "Init:CrashLoopBackOff",
                "\u{2717}",
            ),
            (
                json!({"status": {"phase": "Failed", "reason": "Evicted", "containerStatuses": []}}),
                "Evicted",
                "\u{2717}",
            ),
            (
                json!({"status": {"containerStatuses": [
                    {"name": "api", "ready": false, "state": {"waiting": {"reason": "ImagePullBackOff"}}}
                ]}}),
                "ImagePullBackOff",
                "\u{2717}",
            ),
        ];
        for (extra, word, glyph) in cases {
            let pod = Pod::from_json("qa", &item("p", extra)).unwrap();
            assert_eq!(pod.status, word);
            assert_eq!(pod.glyph(), glyph, "{word}");
        }
    }

    #[test]
    fn a_replica_set_owner_reads_as_its_deployment_when_the_template_hash_says_so() {
        let deployment = Pod::from_json("qa", &item("p", json!({}))).unwrap();
        assert_eq!(
            deployment.owner,
            Some(("Deployment".to_owned(), "orders-api".to_owned()))
        );
        let stateful = Pod::from_json(
            "qa",
            &item(
                "p",
                json!({"metadata": {"ownerReferences": [{"kind": "StatefulSet", "name": "redis", "controller": true}]}}),
            ),
        )
        .unwrap();
        assert_eq!(
            stateful.owner,
            Some(("StatefulSet".to_owned(), "redis".to_owned()))
        );
        let unhashed = Pod::from_json(
            "qa",
            &item(
                "p",
                json!({"metadata": {"labels": {"pod-template-hash": ""}, "ownerReferences": [{"kind": "ReplicaSet", "name": "orders-api-7d9f5b", "controller": true}]}}),
            ),
        )
        .unwrap();
        assert_eq!(
            unhashed.owner,
            Some(("ReplicaSet".to_owned(), "orders-api-7d9f5b".to_owned()))
        );
        let bare = Pod::from_json(
            "qa",
            &item("p", json!({"metadata": {"ownerReferences": []}})),
        )
        .unwrap();
        assert_eq!(bare.owner, None);
        assert!(!bare.restartable());
        assert_eq!(bare.owner_label(), "\u{2014}");
    }

    #[test]
    fn kubectl_errors_read_as_the_one_line_that_says_what_to_fix() {
        assert_eq!(
            kubectl_error("error: context \"aks-qa\" does not exist\n"),
            "context \"aks-qa\" does not exist"
        );
        assert_eq!(
            kubectl_error(
                "E0830 12:00:00.000000   12345 memcache.go:265] couldn't get current server API group list\nUnable to connect to the server: getting credentials: exec: executable kubelogin not found\n\nIt looks like you are trying to use a client-go credential plugin\nSee https://kubernetes.io/docs/reference/access-authn-authz/authentication/#client-go-credential-plugins\n"
            ),
            "Unable to connect to the server: getting credentials: exec: executable kubelogin not found"
        );
        assert_eq!(
            kubectl_error(
                "ERROR: AADSTS700082: The refresh token has expired. Please run 'az login' to setup account.\nUnable to connect to the server: getting credentials: exec: executable kubelogin failed with exit code 1\n"
            ),
            "ERROR: AADSTS700082: The refresh token has expired. Please run 'az login' to setup account."
        );
        assert_eq!(
            kubectl_error(
                "Error from server (Forbidden): pods is forbidden: User \"j\" cannot list resource \"pods\" in API group \"\" in the namespace \"prod\"\n"
            ),
            "Error from server (Forbidden): pods is forbidden: User \"j\" cannot list resource \"pods\" in API group \"\" in the namespace \"prod\""
        );
        assert_eq!(kubectl_error("\n  \n"), "kubectl failed");
    }

    #[test]
    fn a_call_that_will_not_end_is_killed_at_the_cap_and_one_that_answers_is_read_whole() {
        let mut echo = Command::new("sh");
        echo.args(["-c", "printf out; printf err >&2"]);
        assert_eq!(run_capped(echo, Duration::from_secs(5)).unwrap(), "out");

        let mut fails = Command::new("sh");
        fails.args([
            "-c",
            "echo 'error: context \"x\" does not exist' >&2; exit 1",
        ]);
        let error = run_capped(fails, Duration::from_secs(5)).unwrap_err();
        assert_eq!(format!("{error:#}"), "context \"x\" does not exist");

        let mut hangs = Command::new("sleep");
        hangs.arg("30");
        let started = Instant::now();
        let error = run_capped(hangs, Duration::from_millis(200)).unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "killed, not waited for"
        );
        assert!(format!("{error:#}").contains("kubelogin"), "{error:#}");

        let error = run_capped(Command::new("aks-tui-no-such-program"), CALL_CAP).unwrap_err();
        assert!(format!("{error:#}").contains("not installed"), "{error:#}");
    }

    /// One scope read and what it answers.
    type Answer = (Scope, Result<Vec<Pod>, String>);

    /// A source over canned answers, counting what it was asked.
    #[derive(Clone, Default)]
    pub(crate) struct FakeKube {
        /// What each scope answers with; one with no entry answers nothing.
        pub answers: Arc<Mutex<Vec<Answer>>>,
        pub reads: Arc<Mutex<Vec<Scope>>>,
    }

    impl FakeKube {
        pub(crate) fn answer(&self, scope: &Scope, pods: Result<Vec<Pod>, &str>) {
            let mut answers = self.answers.lock().unwrap();
            answers.retain(|(held, _)| held != scope);
            answers.push((scope.clone(), pods.map_err(str::to_owned)));
        }
    }

    impl KubeSource for FakeKube {
        fn pods(&self, scope: &Scope) -> Result<Vec<Pod>> {
            self.reads.lock().unwrap().push(scope.clone());
            let answers = self.answers.lock().unwrap();
            match answers.iter().find(|(held, _)| held == scope) {
                Some((_, Ok(pods))) => Ok(pods.clone()),
                Some((_, Err(message))) => Err(anyhow!(message.clone())),
                None => Ok(Vec::new()),
            }
        }
    }

    fn scopes() -> Vec<Scope> {
        vec![
            scope("qa", Some("dev")),
            scope("qa", Some("qa")),
            scope("prod", Some("prod")),
        ]
    }

    fn watcher(fake: &FakeKube, fast: Duration) -> (Watcher, Receiver<Event>) {
        let (sender, receiver) = mpsc::channel();
        (
            Watcher::new(Box::new(fake.clone()), sender, scopes(), fast),
            receiver,
        )
    }

    fn drain(receiver: &Receiver<Event>) -> Vec<Event> {
        std::iter::from_fn(|| receiver.try_recv().ok()).collect()
    }

    fn pods_events(events: &[Event]) -> Vec<(usize, Result<usize, String>)> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Pods { scope, pods } => {
                    Some((*scope, pods.as_ref().map(Vec::len).map_err(Clone::clone)))
                }
                _ => None,
            })
            .collect()
    }

    fn reads_of(fake: &FakeKube, cluster: &str, namespace: &str) -> usize {
        fake.reads
            .lock()
            .unwrap()
            .iter()
            .filter(|held| held.cluster == cluster && held.namespace.as_deref() == Some(namespace))
            .count()
    }

    #[test]
    fn every_scope_is_read_at_once_the_open_one_first_then_on_its_own_cadence() {
        let fake = FakeKube::default();
        fake.answer(
            &scope("qa", Some("qa")),
            Ok(vec![pod("qa", "qa", "a", "Running")]),
        );
        let (mut watcher, receiver) = watcher(&fake, Duration::from_secs(5));
        watcher.handle(Request::Showing(1));
        let start = Instant::now();
        assert_eq!(watcher.until_due(start), Some(Duration::ZERO));
        watcher.poll_all(start);
        let events = drain(&receiver);
        assert!(matches!(events[0], Event::Reading(1)), "the open tab first");
        assert_eq!(
            pods_events(&events),
            vec![(1, Ok(1)), (0, Ok(0)), (2, Ok(0))]
        );
        // The open tab again after five seconds; the others not for thirty.
        watcher.poll_all(start + Duration::from_secs(4));
        assert_eq!(fake.reads.lock().unwrap().len(), 3);
        assert_eq!(
            watcher.until_due(start + Duration::from_secs(4)),
            Some(Duration::from_secs(1))
        );
        watcher.poll_all(start + Duration::from_secs(5));
        assert_eq!(reads_of(&fake, "qa", "qa"), 2);
        assert_eq!(reads_of(&fake, "qa", "dev"), 1);
        watcher.poll_all(start + HIDDEN_REFRESH);
        assert_eq!(reads_of(&fake, "qa", "dev"), 2);
        assert_eq!(reads_of(&fake, "prod", "prod"), 2);
    }

    #[test]
    fn switching_tabs_reads_the_new_one_at_once_and_slows_the_old_one_down() {
        let fake = FakeKube::default();
        let (mut watcher, _receiver) = watcher(&fake, Duration::from_secs(5));
        watcher.handle(Request::Showing(0));
        let start = Instant::now();
        watcher.poll_all(start);
        assert_eq!(fake.reads.lock().unwrap().len(), 3);

        watcher.handle(Request::Showing(2));
        assert_eq!(
            watcher.until_due(start + Duration::from_secs(1)),
            Some(Duration::ZERO)
        );
        watcher.poll_all(start + Duration::from_secs(1));
        assert_eq!(
            reads_of(&fake, "prod", "prod"),
            2,
            "read the moment it shows"
        );
        assert_eq!(reads_of(&fake, "qa", "dev"), 1);
        // Six seconds on: prod again on the fast cadence, dev still waiting.
        watcher.poll_all(start + Duration::from_secs(7));
        assert_eq!(reads_of(&fake, "prod", "prod"), 3);
        assert_eq!(reads_of(&fake, "qa", "dev"), 1);
        // The same tab again is not a switch.
        watcher.handle(Request::Showing(2));
        assert_ne!(
            watcher.until_due(start + Duration::from_secs(7)),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn a_scope_that_fails_backs_off_on_its_own_and_blanks_nothing_else() {
        let fake = FakeKube::default();
        fake.answer(
            &scope("qa", Some("dev")),
            Err("context \"aks-qa\" does not exist"),
        );
        fake.answer(
            &scope("prod", Some("prod")),
            Ok(vec![pod("prod", "prod", "a", "Running")]),
        );
        let (mut watcher, receiver) = watcher(&fake, Duration::from_secs(5));
        watcher.handle(Request::Showing(0));
        let start = Instant::now();
        watcher.poll_all(start);
        assert_eq!(
            pods_events(&drain(&receiver)),
            vec![
                (0, Err("context \"aks-qa\" does not exist".to_owned())),
                (1, Ok(0)),
                (2, Ok(1)),
            ]
        );
        // The failing scope waits ten seconds, not five.
        watcher.poll_all(start + Duration::from_secs(5));
        assert_eq!(reads_of(&fake, "qa", "dev"), 1);
        watcher.poll_all(start + Duration::from_secs(10));
        assert_eq!(reads_of(&fake, "qa", "dev"), 2);
        // Twenty more once it has failed twice; a clean read puts it back.
        watcher.poll_all(start + Duration::from_secs(20));
        assert_eq!(reads_of(&fake, "qa", "dev"), 2);
        fake.answer(&scope("qa", Some("dev")), Ok(Vec::new()));
        watcher.poll_all(start + Duration::from_secs(30));
        assert_eq!(reads_of(&fake, "qa", "dev"), 3);
        watcher.poll_all(start + Duration::from_secs(35));
        assert_eq!(reads_of(&fake, "qa", "dev"), 4, "back on the fast cadence");
    }

    #[test]
    fn a_refresh_reads_one_scope_again_at_once_and_a_zero_cadence_reads_only_when_asked() {
        let fake = FakeKube::default();
        let (mut watcher, _receiver) = watcher(&fake, Duration::ZERO);
        watcher.handle(Request::Showing(0));
        let start = Instant::now();
        watcher.poll_all(start);
        assert_eq!(fake.reads.lock().unwrap().len(), 3);
        // The open tab is never read again on its own; the hidden ones are.
        assert_eq!(
            watcher.until_due(start),
            Some(HIDDEN_REFRESH),
            "the hidden tabs' cadence is the next thing due"
        );
        watcher.poll_all(start + Duration::from_secs(20));
        assert_eq!(reads_of(&fake, "qa", "dev"), 1);
        watcher.handle(Request::Refresh(0));
        assert_eq!(
            watcher.until_due(start + Duration::from_secs(20)),
            Some(Duration::ZERO)
        );
        watcher.poll_all(start + Duration::from_secs(20));
        assert_eq!(reads_of(&fake, "qa", "dev"), 2);
        assert_eq!(reads_of(&fake, "qa", "qa"), 1, "only the one asked for");
        watcher.handle(Request::Refresh(9));
        assert_ne!(
            watcher.until_due(start + Duration::from_secs(20)),
            Some(Duration::ZERO)
        );
        assert!(!watcher.handle(Request::Stop));
    }

    #[test]
    fn the_handle_runs_the_worker_on_its_own_thread_and_says_once_when_it_stops() {
        let fake = FakeKube::default();
        fake.answer(
            &scope("qa", Some("dev")),
            Ok(vec![pod("qa", "dev", "a", "Running")]),
        );
        let handle = Handle::spawn(Box::new(fake), scopes(), Duration::from_secs(5)).unwrap();
        handle.send(Request::Showing(0)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = None;
        while Instant::now() < deadline && seen.is_none() {
            if let Some(Event::Pods { scope: 0, pods }) = handle.try_event() {
                seen = Some(pods.map(|pods| pods.len()));
            } else {
                thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(seen, Some(Ok(1)));
        handle.send(Request::Stop).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stopped = 0;
        while Instant::now() < deadline {
            match handle.try_event() {
                Some(Event::Stopped) => {
                    stopped += 1;
                    break;
                }
                Some(_) => {}
                None => thread::sleep(Duration::from_millis(10)),
            }
        }
        assert_eq!(stopped, 1);
        assert!(handle.try_event().is_none(), "Stopped is said once");
    }
}
