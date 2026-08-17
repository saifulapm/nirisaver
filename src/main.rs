//! Entry point: resolve the configuration, take the single-instance lock, and
//! either put an overlay on screen or render headlessly.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use nirisaver::cli::Cli;
use nirisaver::config::{self, Env, Settings};
use nirisaver::{engine, headless, wayland};

fn main() -> ExitCode {
    let cli = Cli::parse();

    if cli.list_effects {
        for name in engine::available_effects() {
            println!("{name}");
        }
        return ExitCode::SUCCESS;
    }

    match dispatch(&cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("nirisaver: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: &Cli) -> Result<ExitCode> {
    // Everything the machine has to say is read here and nowhere else.
    let settings = config::resolve(cli, &Env::from_process())?;

    if cli.headless {
        return run_headless(cli, &settings).map(|()| ExitCode::SUCCESS);
    }

    // One screensaver at a time. A second launch — the idle service firing
    // again, a keybinding pressed twice — is not an error and must not look
    // like one: the caller asked for the screensaver to be up, and it is.
    let _lock = match settings.lock_path.as_deref() {
        Some(path) => match take_lock(path)? {
            Some(lock) => Some(lock),
            None => return Ok(ExitCode::SUCCESS),
        },
        None => None,
    };

    wayland::run(&settings)?;
    Ok(ExitCode::SUCCESS)
}

fn run_headless(cli: &Cli, settings: &Settings) -> Result<()> {
    let cols = cli.cols.unwrap_or(96).max(1);
    let rows = cli.rows.unwrap_or(28).max(1);
    let frames = cli.frames.unwrap_or(120);
    let (report, grid) = headless::run(settings, cols, rows, frames)?;
    println!("{report}");
    if cli.dump_grid {
        print!("{grid}");
    }
    Ok(())
}

/// An advisory exclusive lock held for the life of the process. Returns `None`
/// when another instance already holds it.
fn take_lock(path: &Path) -> Result<Option<std::fs::File>> {
    use rustix::fs::{flock, FlockOperation};

    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening the lock file {}", path.display()))?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}
