#![deny(missing_docs)]
#![deny(rustdoc::missing_crate_level_docs)]
#![deny(clippy::pedantic)]
#![deny(clippy::missing_docs_in_private_items)]
#![deny(clippy::missing_errors_doc)]
#![deny(clippy::missing_panics_doc)]
#![deny(clippy::missing_assert_message)]
#![deny(clippy::missing_asserts_for_indexing)]
#![deny(clippy::unwrap_used)]

//! Project automation commands.

use clap::{Parser, Subcommand};
use eyre::{Result, WrapErr, eyre};
use std::process::Command;

/// xtask entrypoint.
#[derive(Parser)]
#[command(author, version, about)]
struct Cli {
    /// Command to run.
    #[command(subcommand)]
    command: Task,
}

/// Supported automation commands.
#[derive(Subcommand)]
enum Task {
    /// Run rustfmt.
    Fmt,
    /// Run clippy.
    Clippy,
    /// Run tests.
    Test,
    /// Run benchmarks.
    Bench,
    /// Run documentation checks.
    Doc,
    /// Run coverage.
    Cov,
    /// Run the full CI-equivalent pipeline.
    Ci,
}

/// Execute a child command.
fn run_command(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .wrap_err_with(|| format!("failed to start {program}"))?;
    if status.success() {
        return Ok(());
    }

    Err(eyre!("command failed: {program} {}", args.join(" ")))
}

/// Dispatch tasks.
fn main() -> Result<()> {
    color_eyre::install()?;
    match Cli::parse().command {
        Task::Fmt => run_command("cargo", &["fmt", "--all"]),
        Task::Clippy => run_command(
            "cargo",
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-W",
                "clippy::pedantic",
            ],
        ),
        Task::Test => run_command("cargo", &["test", "--workspace", "--all-features"]),
        Task::Bench => run_command("cargo", &["bench", "--bench", "scan"]),
        Task::Doc => run_command("cargo", &["doc", "--workspace", "--no-deps"]),
        Task::Cov => run_command(
            "cargo",
            &[
                "llvm-cov",
                "--package",
                "codexusage",
                "--lib",
                "--tests",
                "--all-features",
                "--ignore-filename-regex",
                ".*/main\\.rs$",
                "--fail-under-lines",
                "90",
            ],
        ),
        Task::Ci => {
            run_command("cargo", &["fmt", "--all", "--check"])?;
            run_command(
                "cargo",
                &[
                    "clippy",
                    "--workspace",
                    "--all-targets",
                    "--all-features",
                    "--",
                    "-W",
                    "clippy::pedantic",
                ],
            )?;
            run_command("cargo", &["test", "--workspace", "--all-features"])?;
            run_command("cargo", &["bench", "--bench", "scan"])?;
            run_command("cargo", &["doc", "--workspace", "--no-deps"])?;
            run_command(
                "cargo",
                &[
                    "llvm-cov",
                    "--package",
                    "codexusage",
                    "--lib",
                    "--tests",
                    "--all-features",
                    "--ignore-filename-regex",
                    ".*/main\\.rs$",
                    "--fail-under-lines",
                    "90",
                ],
            )?;
            Ok(())
        }
    }
}
