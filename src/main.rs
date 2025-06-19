//! railyard runs multiple commands, maybe in parallel, and nicely shows thier output
//! as they complete, allowing intermediate commands to fail without stopping the
//! whole chain.

use std::{
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio, Termination},
    time::Duration,
};

use clap::{arg, value_parser};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use owo_colors::OwoColorize as _;
use tokio::{
    io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader},
    sync::{oneshot, watch},
};
use tracing_subscriber::EnvFilter;

#[derive(Debug)]
#[allow(dead_code)]
enum RailyardErrorKind {
    InvalidArguments,
    SpawnError(io::Error),
    CommandError(io::Error),
    StreamError(io::Error),
}

const USAGE_ERROR: u8 = 2u8;
const INTERNAL_ERROR: u8 = 3u8;

impl RailyardErrorKind {
    fn report(&self) -> std::process::ExitCode {
        match self {
            RailyardErrorKind::InvalidArguments => USAGE_ERROR.into(),
            RailyardErrorKind::SpawnError(_) => USAGE_ERROR.into(),
            RailyardErrorKind::CommandError(_) => INTERNAL_ERROR.into(),
            RailyardErrorKind::StreamError(_) => INTERNAL_ERROR.into(),
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
struct RailyardError {
    command: String,
    kind: RailyardErrorKind,
}

impl Termination for RailyardError {
    fn report(self) -> std::process::ExitCode {
        self.kind.report()
    }
}

fn read_railyard_file<P: AsRef<Path>>(commands: &Commands, path: P) -> io::Result<()> {
    let raw = fs::read_to_string(path)?;
    let document: toml_edit::DocumentMut = raw.parse().unwrap();

    for (name, args) in document.as_table().iter() {
        if let Some(array) = args.as_array() {
            let args: Vec<_> = array.iter().filter_map(|item| item.as_str()).collect();
            commands.command(name, &args);
        }
    }

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), RailyardError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(tracing::Level::ERROR.into())
                .from_env_lossy(),
        )
        .init();

    let program = clap::Command::new("railyard")
        .about("A tool for running multiple tasks in parallel with pretty output")
        .arg(
            arg!(railyard: <RAILYARD> "The railyard configuration to use")
                .value_parser(value_parser!(PathBuf)),
        )
        .arg(arg!(--hide "Don't show progress spinners at all"))
        .author("Alex Rudy <opensource@alexrudy.net>")
        .version("0.1")
        .get_matches();

    let railyard: &PathBuf = program.get_one::<PathBuf>("railyard").unwrap();

    let spinner_style = ProgressStyle::with_template("{prefix:10.bold.dim} {spinner} {wide_msg}")
        .unwrap()
        .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");

    let m = MultiProgress::with_draw_target(ProgressDrawTarget::stdout());
    if program.get_flag("hide") {
        m.set_draw_target(ProgressDrawTarget::hidden());
    }

    let commands = Commands::new(m, spinner_style);
    read_railyard_file(&commands, railyard).expect("Valid railyard file");

    commands.run().await;

    Ok(())
}

struct Commands {
    local_set: tokio::task::LocalSet,
    progress: MultiProgress,
    style: ProgressStyle,
}

impl Commands {
    fn new(progress: MultiProgress, style: ProgressStyle) -> Self {
        Self {
            local_set: tokio::task::LocalSet::new(),
            progress,
            style,
        }
    }

    fn command<A: AsRef<OsStr>>(&self, name: impl Into<String>, args: &[A]) {
        let name = name.into();
        let pb = self.progress.add(
            ProgressBar::new_spinner()
                .with_style(self.style.clone())
                .with_prefix(name.clone()),
        );
        let (cmd, reporter) = Command::new(name, args.into_iter().map(Into::into).collect(), pb);
        self.local_set.spawn_local(cmd.run());
        self.local_set.spawn_local(reporter.run());
    }

    async fn run(self) {
        self.local_set.await
    }
}

struct Command {
    name: String,
    args: Vec<OsString>,
    outcome: oneshot::Sender<CommandReport>,
    watcher: watch::Sender<Option<Vec<u8>>>,
}

impl Command {
    fn new(
        name: impl Into<String>,
        args: Vec<OsString>,
        progress: ProgressBar,
    ) -> (Command, CommandReporter) {
        let (out_tx, out_rx) = oneshot::channel();
        let (watch_tx, watch_rx) = watch::channel(None);
        let name = name.into();
        let command = Command {
            name: name.clone(),
            args,
            outcome: out_tx,
            watcher: watch_tx,
        };

        let reporter = CommandReporter {
            name,
            outcome: out_rx,
            watcher: watch_rx,
            progress,
        };

        (command, reporter)
    }

    async fn run(mut self) {
        let Some((prog, args)) = self.args.split_first() else {
            let _ = self.outcome.send(CommandReport::Error(RailyardError {
                command: self.name.clone(),
                kind: RailyardErrorKind::InvalidArguments,
            }));
            return;
        };

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

        if let Err(_) = self.outcome.send(report.unwrap()) {
            eprintln!(
                "{}: {}",
                "INTERNAL ERROR".red(),
                "Unable to send command outcome"
            );
        }
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
            buffer.extend_from_slice(&line);
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
    async fn run(mut self) {
        let pb = self.progress.clone();
        let tick = async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                pb.inc(1);
            }
        };

        let report = async { while self.report().await.is_none() {} };

        tokio::select! {
            _ = tick => {},
            _ = report => {},
        };
    }

    #[tracing::instrument(skip(self), fields(name=self.name))]
    async fn report(&mut self) -> Option<CommandReport> {
        let _ = self.watcher.changed().await;

        tracing::debug!("Reproting {:?}", self.watcher.borrow());

        if let Ok(outcome) = self.outcome.try_recv() {
            match &outcome {
                CommandReport::Success => self
                    .progress
                    .finish_with_message(format!("{}", "success".green())),
                CommandReport::Error(error) => {
                    self.progress
                        .finish_with_message(format!("{}", "internal error".red()));
                    eprintln!("[{}] {:?}", "INTERNAL ERROR".red(), error);
                }
                CommandReport::Failed(command_output) => {
                    self.progress
                        .finish_with_message(format!("exited with {}", command_output.exit_code));

                    let _ = tokio::io::stdout().write_all(&command_output.buffer).await;
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
