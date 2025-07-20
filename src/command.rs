use std::{
    collections::{BTreeMap, btree_map::Entry},
    ffi::{OsStr, OsString},
    fmt, io,
    process::{ExitCode, ExitStatus, Stdio},
    rc::Rc,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::{FutureExt, TryStreamExt as _, stream::FuturesUnordered};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use owo_colors::OwoColorize as _;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{Semaphore, mpsc, oneshot, watch},
    task::JoinHandle,
};

use crate::{RailyardError, RailyardErrorKind};

enum Dependency {
    Waiting(
        Option<(
            tokio::sync::broadcast::Sender<()>,
            tokio::sync::broadcast::Receiver<()>,
            Vec<String>,
        )>,
    ),
    Ready,
}

impl fmt::Debug for Dependency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Dependency::Waiting(_) => write!(f, "Waiting"),
            Dependency::Ready => write!(f, "Recieved"),
        }
    }
}

#[derive(Debug, Clone)]
struct Dependencies {
    dependencies: Arc<Mutex<BTreeMap<String, Dependency>>>,
}

#[derive(Debug)]
struct Cancelled;

impl Dependencies {
    fn new() -> Self {
        Self {
            dependencies: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn cancel(&self) -> String {
        use std::fmt::Write;
        let mut map = self.dependencies.lock().expect("map invariant: poisoned");
        let mut msg = String::new();
        for (item, dep) in map.iter() {
            if let Dependency::Waiting(Some((_, _, waiters))) = dep {
                writeln!(
                    &mut msg,
                    "cancelling {item}: awaited by {}",
                    waiters.join(",")
                )
                .unwrap();
            }
        }
        map.clear();
        msg
    }

    async fn wait(&self, source: String, name: impl Into<String>) -> Result<(), Cancelled> {
        {
            let name = name.into();
            tracing::trace!("checking for {name}");
            let mut rx = {
                let (tx, rx) = tokio::sync::broadcast::channel(1);
                let mut map = self.dependencies.lock().expect("map invariant: poisoned");
                match map
                    .entry(name.clone())
                    .or_insert_with(|| Dependency::Waiting(Some((tx, rx, Vec::new()))))
                {
                    Dependency::Waiting(Some((tx, _, waiters))) => {
                        waiters.push(source);
                        tx.subscribe()
                    }
                    Dependency::Waiting(None) => {
                        panic!("map invariant: subscriber must be present")
                    }
                    Dependency::Ready => {
                        tracing::trace!("{name} already ready");
                        return Ok(());
                    }
                }
            };

            tracing::trace!("waiting for {name}");
            rx.recv().await.map_err(|_| Cancelled)
        }
    }

    fn ready(&self, name: impl Into<String>) {
        let name = name.into();
        let mut map = self.dependencies.lock().expect("map invariant: poisoned");
        match map.entry(name.clone()) {
            Entry::Occupied(mut entry) => {
                tracing::trace!("Notifying occupied entry for {name}");
                let value = entry.get_mut();
                *value = match value {
                    Dependency::Waiting(broadcast) => {
                        let (tx, _, _) = broadcast.take().unwrap();
                        if tx.send(()).is_err() {
                            panic!("map invariant: unable to notify {name}");
                        }
                        tracing::trace!("notified {name}");
                        Dependency::Ready
                    }
                    Dependency::Ready => {
                        panic!("map invariant: already notified for {name}");
                    }
                };
            }
            Entry::Vacant(entry) => {
                tracing::trace!("marking {name} as ready");
                entry.insert(Dependency::Ready);
            }
        };
    }
}

#[must_use = "Commands wont run without calling run()"]
pub(crate) struct Commands {
    semaphore: Rc<Semaphore>,
    local_set: tokio::task::LocalSet,
    notify: mpsc::Receiver<()>,
    sender: Option<mpsc::Sender<()>>,
    progress: MultiProgress,
    style: ProgressStyle,
    dependencies: Dependencies,
    free_commands: usize,
    reports: BTreeMap<String, JoinHandle<Option<CommandReport>>>,
    show_reports: bool,
}

impl Commands {
    pub(crate) fn new(
        progress: MultiProgress,
        style: ProgressStyle,
        show_reports: bool,
        jobs: usize,
    ) -> Self {
        let (tx, rx) = mpsc::channel(32);
        Self {
            semaphore: Rc::new(Semaphore::new(jobs)),
            local_set: tokio::task::LocalSet::new(),
            notify: rx,
            sender: Some(tx),
            progress,
            style,
            dependencies: Dependencies::new(),
            free_commands: 0,
            reports: BTreeMap::new(),
            show_reports,
        }
    }

    pub(crate) fn dependent_command<A: AsRef<OsStr>>(
        &mut self,
        name: impl Into<String>,
        args: &[A],
        dependencies: &[String],
    ) {
        let name = name.into();
        let pb = self.progress.add(
            ProgressBar::new_spinner()
                .with_style(self.style.clone())
                .with_prefix(name.clone()),
        );
        if dependencies.is_empty() {
            self.free_commands += 1;
        }

        let (cmd, reporter) = Command::new(
            name.clone(),
            args.iter().map(Into::into).collect(),
            pb,
            self.sender.clone().unwrap(),
            dependencies.to_vec(),
        );
        self.local_set
            .spawn_local(cmd.run(self.semaphore.clone(), self.dependencies.clone()));
        let handle = self.local_set.spawn_local(reporter.run());
        self.reports.insert(name, handle);
    }

    pub(crate) fn command<A: AsRef<OsStr>>(&mut self, name: impl Into<String>, args: &[A]) {
        let name = name.into();
        let pb = self.progress.add(
            ProgressBar::new_spinner()
                .with_style(self.style.clone())
                .with_prefix(name.clone()),
        );
        self.free_commands += 1;
        let (cmd, reporter) = Command::new(
            name.clone(),
            args.iter().map(Into::into).collect(),
            pb,
            self.sender.clone().unwrap(),
            Vec::new(),
        );
        self.local_set
            .spawn_local(cmd.run(self.semaphore.clone(), self.dependencies.clone()));
        let handle = self.local_set.spawn_local(reporter.run());
        self.reports.insert(name, handle);
    }

    pub(crate) async fn run(mut self) -> ExitCode {
        if self.free_commands == 0 {
            eprintln!(
                "[{}]: No commands without dependencies",
                "ERROR".red().bold()
            );
            return ExitCode::FAILURE;
        }

        let pb = self
            .progress
            .add(ProgressBar::new(self.reports.len() as _).with_style(
                ProgressStyle::with_template("[{pos:.bold}/{len:.bold.dim}] {bar} {msg}").unwrap(),
            ));

        let mut notify = self.notify;
        self.sender.take();
        let sem = self.semaphore.clone();
        let permits = sem.available_permits();
        let deps = self.dependencies.clone();
        let handle = self.local_set.spawn_local(async move {
            let mut no_progress_found = 0;
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if sem.available_permits() == permits {
                    no_progress_found += 1;
                } else {
                    no_progress_found = 0;
                }

                if no_progress_found > 5 {
                    return deps.cancel();
                }
            }
        });

        let aborter = handle.abort_handle();

        self.local_set.spawn_local(async move {
            while notify.recv().await.is_some() {
                pb.inc(1);
            }
            if let Some(Ok(msg)) = handle.now_or_never() {
                pb.set_message(msg);
            }
            pb.finish();

            aborter.abort();
        });

        self.local_set.await;

        let mut exit = ExitCode::SUCCESS;
        let mut reports = Vec::new();
        for handle in self.reports.into_values() {
            if let Ok(Some(CommandReport::Failed(output))) = handle.await {
                reports.push(output);

                exit = ExitCode::FAILURE;
            }
        }
        let mut stderr = tokio::io::stderr();
        if self.show_reports && !reports.is_empty() {
            eprint_centered(&mut stderr, format!(" {} Tasks Failed ", reports.len()))
                .await
                .unwrap();

            for output in &reports {
                eprint_centered(&mut stderr, format!(" {} ", output.name))
                    .await
                    .unwrap();
                let _ = stderr.write_all(&output.buffer).await;
                stderr.flush().await.unwrap();
            }
            eprint_centered(&mut stderr, format!(" {} Tasks Failed ", reports.len()))
                .await
                .unwrap();
            for output in &reports {
                let summary = format!(
                    "{}: {} with {}",
                    output.name.bold().dimmed(),
                    "exited".red().bold(),
                    output.exit_code
                );

                eprintln(&mut stderr, summary).await.unwrap();
            }
        };
        stderr.flush().await.unwrap();
        exit
    }
}

async fn eprintln<S>(output: &mut S, message: impl AsRef<str>) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let message = format!("{}\n", message.as_ref());
    output.write_all(message.as_bytes()).await?;
    output.flush().await
}

async fn eprint_centered<S>(output: &mut S, message: impl AsRef<str>) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let message = format!("{:=^width$}", message.as_ref(), width = 80);
    eprintln(output, message).await
}

struct Command {
    name: String,
    args: Vec<OsString>,
    outcome: oneshot::Sender<CommandReport>,
    watcher: watch::Sender<Option<Vec<u8>>>,
    notify: mpsc::Sender<()>,
    dependencies: Vec<String>,
}

impl Command {
    fn new(
        name: impl Into<String>,
        args: Vec<OsString>,
        progress: ProgressBar,
        notify: mpsc::Sender<()>,
        dependencies: Vec<String>,
    ) -> (Command, CommandReporter) {
        let (out_tx, out_rx) = oneshot::channel();
        let (watch_tx, watch_rx) = watch::channel(None);
        let name = name.into();
        let command = Command {
            name: name.clone(),
            args,
            outcome: out_tx,
            watcher: watch_tx,
            notify,
            dependencies,
        };

        let reporter = CommandReporter {
            name,
            outcome: out_rx,
            watcher: watch_rx,
            progress,
        };

        (command, reporter)
    }

    async fn run(mut self, semaphore: Rc<Semaphore>, dependencies: Dependencies) {
        let Some((prog, args)) = self.args.split_first() else {
            let _ = self.outcome.send(CommandReport::Error(RailyardError {
                command: self.name.clone(),
                kind: RailyardErrorKind::InvalidArguments,
            }));
            let _ = self.notify.send(()).await;
            return;
        };

        let deps: FuturesUnordered<_> = self
            .dependencies
            .iter()
            .map(|dep| dependencies.wait(self.name.clone(), dep))
            .collect();

        if let Err(Cancelled) = deps.try_collect::<()>().await {
            let _ = self.outcome.send(CommandReport::Error(RailyardError {
                command: self.name.clone(),
                kind: RailyardErrorKind::Cancelled,
            }));
            let _ = self.notify.send(()).await;
            return;
        }

        let permit = semaphore.acquire().await.unwrap();

        tracing::debug!("Spawning {:?} with arguments {:?}", prog, args);
        let mut child = match tokio::process::Command::new(prog)
            .args(args)
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|error| RailyardError {
                command: self.name.clone(),
                kind: RailyardErrorKind::SpawnError(error),
            }) {
            Ok(child) => child,
            Err(error) => {
                let _ = self.outcome.send(CommandReport::Error(error));
                let _ = self.notify.send(()).await;
                return;
            }
        };
        tracing::info!("Spawned {}", self.name);

        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let mut buffer = Vec::new();

        let mut stdout_line = Vec::new();
        let mut stderr_line = Vec::new();
        let mut report;

        loop {
            report = tokio::select! {
                out = stdout.read_until(b'\n', &mut stdout_line) => process_stream(out, &mut stderr_line, &mut buffer, &mut self),
                out = stderr.read_until(b'\n', &mut stderr_line) => process_stream(out, &mut stderr_line, &mut buffer, &mut self),
                exit = child.wait() => Some(CommandReport::build(exit, &mut buffer, &self)),
            };

            if report.is_some() {
                break;
            }
        }
        drop(permit);
        dependencies.ready(self.name);
        let _ = self.outcome.send(report.unwrap());
        let _ = self.notify.send(()).await;
    }
}

fn process_stream(
    output: Result<usize, io::Error>,
    line: &mut Vec<u8>,
    buffer: &mut Vec<u8>,
    command: &mut Command,
) -> Option<CommandReport> {
    match output {
        Ok(0) => {
            tracing::debug!("Stream for {} EOF", command.name);
            None
        }
        Ok(_) => {
            tracing::debug!(
                "Recieved a line from {}: {}",
                command.name,
                String::from_utf8_lossy(line).strip_suffix('\n').unwrap()
            );
            buffer.extend_from_slice(line);
            let _ = command.watcher.send(Some(Vec::from(line.as_slice())));
            line.clear();
            None
        }
        Err(error) => Some(CommandReport::Error(RailyardError {
            command: command.name.clone(),
            kind: RailyardErrorKind::StreamError(error),
        })),
    }
}

#[derive(Debug)]
struct CommandOutput {
    name: String,
    exit_code: i32,
    buffer: Vec<u8>,
}

#[derive(Debug)]
enum CommandReport {
    Success,
    Error(RailyardError),
    Failed(CommandOutput),
}

impl CommandReport {
    fn build(value: io::Result<ExitStatus>, buffer: &mut Vec<u8>, command: &Command) -> Self {
        match value {
            Ok(status) if status.success() => CommandReport::Success,
            Ok(status) => {
                let output = std::mem::take(buffer);
                CommandReport::Failed(CommandOutput {
                    name: command.name.clone(),
                    exit_code: status.code().unwrap(),
                    buffer: output,
                })
            }
            Err(error) => CommandReport::Error(RailyardError {
                command: command.name.clone(),
                kind: RailyardErrorKind::CommandError(error),
            }),
        }
    }
}

struct CommandReporter {
    name: String,
    outcome: oneshot::Receiver<CommandReport>,
    watcher: watch::Receiver<Option<Vec<u8>>>,
    progress: ProgressBar,
}

impl CommandReporter {
    async fn run(mut self) -> Option<CommandReport> {
        let pb = self.progress.clone();
        let tick = async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                pb.inc(1);
            }
        };

        let report = async {
            loop {
                if let Some(output) = self.report().await {
                    return Some(output);
                }
            }
        };

        let output = tokio::select! {
            _ = tick => None,
            output = report => output,
        };

        output
    }

    #[tracing::instrument(skip(self), fields(name=self.name))]
    async fn report(&mut self) -> Option<CommandReport> {
        let _ = self.watcher.changed().await;

        tracing::debug!("Reproting {:?}", self.watcher.borrow());

        if let Ok(outcome) = self.outcome.try_recv() {
            match &outcome {
                CommandReport::Success => self
                    .progress
                    .finish_with_message(format!("{}", "success".green().bold())),
                CommandReport::Error(RailyardError {
                    kind: RailyardErrorKind::Cancelled,
                    ..
                }) => {
                    self.progress
                        .finish_with_message(format!("{}", "cancelled".dimmed().bold()));
                }
                CommandReport::Error(error) => {
                    self.progress
                        .finish_with_message(format!("{}", "internal error".red()));
                    eprintln!("[{}] {:?}", "INTERNAL ERROR".red(), error);
                }
                CommandReport::Failed(command_output) => {
                    self.progress.finish_with_message(format!(
                        "{} with {}",
                        "exited".red().bold(),
                        command_output.exit_code
                    ));
                }
            };
            tracing::debug!("Outcome {:?}", outcome);
            return Some(outcome);
        }

        let borrow = self.watcher.borrow();
        if let Some(line) = borrow
            .as_ref()
            .and_then(|line| String::from_utf8(line.clone()).ok())
        {
            self.progress.set_message(line);
        }

        None
    }
}
