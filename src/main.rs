//! railyard runs multiple commands, maybe in parallel, and nicely shows thier output
//! as they complete, allowing intermediate commands to fail without stopping the
//! whole chain.

use std::{
    io,
    path::PathBuf,
    process::{ExitCode, Termination},
};

use clap::{Arg, ArgAction, arg, value_parser};
use indicatif::{MultiProgress, ProgressDrawTarget, ProgressStyle};

use tracing_subscriber::EnvFilter;

use self::{command::Commands, justfile::read_justfile_for_target, railfile::read_railyard_file};

mod command;
mod justfile;
mod railfile;

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

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<ExitCode, RailyardError> {
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
            arg!(railyard: [RAILYARD] "The railyard configuration to use")
                .value_parser(value_parser!(PathBuf))
                .conflicts_with("justfile"),
        )
        .arg(arg!(--hide "Don't show progress spinners at all"))
        .arg(
            Arg::new("reports")
                .long("no-reports")
                .action(ArgAction::SetFalse)
                .help("Don't show command outputs for failed commands"),
        )
        .arg(
            Arg::new("justfile")
                .short('j')
                .long("just")
                .action(ArgAction::Set)
                .value_name("JUST")
                .num_args(0..=2)
                .conflicts_with("railyard"),
        )
        .author("Alex Rudy <opensource@alexrudy.net>")
        .version("0.1")
        .get_matches();

    let spinner_style = ProgressStyle::with_template("{prefix:10.bold.dim} {spinner} {wide_msg}")
        .unwrap()
        .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ ");

    let m = MultiProgress::with_draw_target(ProgressDrawTarget::stdout());
    if program.get_flag("hide") {
        m.set_draw_target(ProgressDrawTarget::hidden());
    }

    let mut commands = Commands::new(m, spinner_style, program.get_flag("reports"));

    if let Some(railyard) = program.get_one::<PathBuf>("railyard") {
        read_railyard_file(&mut commands, railyard).expect("Valid railyard file");
    }

    if program.contains_id("justfile") {
        let n = program.get_many::<String>("justfile").unwrap().count();

        let (path, target) = match n {
            0 => (None, None),
            1 => (None, program.get_many::<String>("justfile").unwrap().next()),
            2 => {
                let mut iter = program.get_many("justfile").unwrap();
                (iter.next(), iter.next())
            }
            _ => {
                panic!("Wrong number of justfile args")
            }
        };

        read_justfile_for_target(&mut commands, path, target.map(|s| s.as_str()))
            .await
            .expect("Trouble with just");
    }

    Ok(commands.run().await)
}
