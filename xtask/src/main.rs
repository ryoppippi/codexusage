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
use eyre::{Context, Result, eyre};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Root crate manifest used for release tagging.
const ROOT_MANIFEST: &str = "Cargo.toml";
/// `cargo fmt` invocation shared by multiple tasks.
const CARGO_FMT_ARGS: &[&str] = &["fmt", "--all"];
/// `cargo test` invocation shared by multiple tasks.
const CARGO_TEST_ARGS: &[&str] = &["test", "--workspace", "--all-features"];
/// `cargo doc` invocation shared by multiple tasks.
const CARGO_DOC_ARGS: &[&str] = &["doc", "--workspace", "--no-deps"];
/// `cargo bench` invocation shared by multiple tasks.
const CARGO_BENCH_ARGS: &[&str] = &["bench", "--bench", "scan"];
/// `cargo clippy` invocation shared by multiple tasks.
const CARGO_CLIPPY_ARGS: &[&str] = &[
    "clippy",
    "--workspace",
    "--all-targets",
    "--all-features",
    "--",
    "-W",
    "clippy::pedantic",
];
/// `cargo llvm-cov` invocation shared by multiple tasks.
const CARGO_COV_ARGS: &[&str] = &[
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
];

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
    /// Publish the crate and create a local git tag from the package version.
    Publish,
}

/// Abstraction over process execution so release orchestration stays testable.
trait CommandRunner {
    /// Execute a command and fail when it exits unsuccessfully.
    fn run(&mut self, program: &str, args: &[&str], cwd: &Path) -> Result<()>;

    /// Execute a command and return trimmed standard output.
    fn output(&mut self, program: &str, args: &[&str], cwd: &Path) -> Result<String>;
}

/// System command runner used in production.
struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&mut self, program: &str, args: &[&str], cwd: &Path) -> Result<()> {
        run_command(program, args, cwd)
    }

    fn output(&mut self, program: &str, args: &[&str], cwd: &Path) -> Result<String> {
        command_output(program, args, cwd)
    }
}

/// Execute a child command.
fn run_command(program: &str, args: &[&str], cwd: &Path) -> Result<()> {
    let status = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .status()
        .wrap_err_with(|| format!("failed to start {program}"))?;
    if status.success() {
        return Ok(());
    }

    Err(eyre!("command failed: {program} {}", args.join(" ")))
}

/// Capture a child command's trimmed stdout.
fn command_output(program: &str, args: &[&str], cwd: &Path) -> Result<String> {
    let output = Command::new(program)
        .current_dir(cwd)
        .args(args)
        .output()
        .wrap_err_with(|| format!("failed to start {program}"))?;
    if !output.status.success() {
        return Err(eyre!("command failed: {program} {}", args.join(" ")));
    }

    Ok(String::from_utf8(output.stdout)
        .wrap_err_with(|| format!("command {program} returned non-UTF-8 stdout"))?
        .trim()
        .to_owned())
}

/// Run the full CI-equivalent pipeline.
fn run_ci(runner: &mut impl CommandRunner, repo_root: &Path) -> Result<()> {
    runner.run("cargo", &["fmt", "--all", "--check"], repo_root)?;
    runner.run("cargo", CARGO_CLIPPY_ARGS, repo_root)?;
    runner.run("cargo", CARGO_TEST_ARGS, repo_root)?;
    runner.run("cargo", CARGO_BENCH_ARGS, repo_root)?;
    runner.run("cargo", CARGO_DOC_ARGS, repo_root)?;
    runner.run("cargo", CARGO_COV_ARGS, repo_root)
}

/// Publish the crate and tag the published version locally.
fn publish_release(runner: &mut impl CommandRunner, repo_root: &Path) -> Result<()> {
    ensure_clean_worktree(runner, repo_root)?;
    let version = package_version(runner, repo_root)?;
    ensure_tag_absent(runner, repo_root, &version)?;

    run_ci(runner, repo_root)?;
    runner.run("cargo", &["publish", "--dry-run"], repo_root)?;
    runner.run("cargo", &["publish"], repo_root)?;
    runner.run("git", &["tag", version.as_str()], repo_root)
}

/// Fail when the worktree has uncommitted changes.
fn ensure_clean_worktree(runner: &mut impl CommandRunner, repo_root: &Path) -> Result<()> {
    let status = runner.output("git", &["status", "--porcelain"], repo_root)?;
    if is_clean_worktree(&status) {
        return Ok(());
    }

    Err(eyre!(
        "publish requires a clean worktree; commit or stash outstanding changes first"
    ))
}

/// Determine whether `git status --porcelain` reports a clean worktree.
fn is_clean_worktree(status: &str) -> bool {
    status.trim().is_empty()
}

/// Fail when the release tag already exists.
fn ensure_tag_absent(runner: &mut impl CommandRunner, repo_root: &Path, tag: &str) -> Result<()> {
    let existing = runner.output("git", &["tag", "--list", tag], repo_root)?;
    if existing.is_empty() {
        return Ok(());
    }

    Err(eyre!("release tag {tag} already exists"))
}

/// Read the crate version from Cargo's package metadata.
fn package_version(runner: &mut impl CommandRunner, repo_root: &Path) -> Result<String> {
    let manifest_path = repo_root.join(ROOT_MANIFEST);
    let manifest_arg = manifest_path.to_str().ok_or_else(|| {
        eyre!(
            "manifest path {} is not valid UTF-8",
            manifest_path.display()
        )
    })?;
    let pkgid = runner.output(
        "cargo",
        &["pkgid", "--quiet", "--manifest-path", manifest_arg],
        repo_root,
    )?;
    parse_pkgid_version(&pkgid)
        .map(str::to_owned)
        .wrap_err_with(|| {
            format!("failed to parse package version from cargo pkgid output {pkgid}")
        })
}

/// Parse one `cargo pkgid` output into a package version.
fn parse_pkgid_version(pkgid: &str) -> Result<&str> {
    let Some((_, fragment)) = pkgid.rsplit_once('#') else {
        return Err(eyre!(
            "cargo pkgid output does not contain a package fragment"
        ));
    };
    let version = fragment
        .rsplit_once('@')
        .map_or(fragment, |(_, version)| version);
    if version.is_empty() {
        return Err(eyre!("cargo pkgid output contains an empty version"));
    }
    Ok(version)
}

/// Resolve the repository root from the current xtask working directory.
fn repo_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir().wrap_err("failed to read current directory")?;
    let output = Command::new("cargo")
        .current_dir(&cwd)
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .output()
        .wrap_err("failed to start cargo locate-project")?;
    if !output.status.success() {
        return Err(eyre!(
            "command failed: cargo locate-project --workspace --message-format plain"
        ));
    }

    let manifest_path = String::from_utf8(output.stdout)
        .wrap_err("cargo locate-project returned non-UTF-8 stdout")?
        .trim()
        .to_owned();
    workspace_root_from_manifest_path(&manifest_path)
}

/// Resolve the workspace root from a workspace manifest path.
fn workspace_root_from_manifest_path(manifest_path: &str) -> Result<PathBuf> {
    let manifest = Path::new(manifest_path);
    let Some(parent) = manifest.parent() else {
        return Err(eyre!(
            "workspace manifest path {manifest_path} does not have a parent directory"
        ));
    };
    parent.canonicalize().wrap_err_with(|| {
        format!(
            "failed to resolve repository root from workspace manifest {}",
            manifest.display()
        )
    })
}

/// Dispatch tasks.
fn main() -> Result<()> {
    color_eyre::install()?;
    let repo_root = repo_root()?;
    let mut runner = SystemRunner;
    match Cli::parse().command {
        Task::Fmt => runner.run("cargo", CARGO_FMT_ARGS, &repo_root),
        Task::Clippy => runner.run("cargo", CARGO_CLIPPY_ARGS, &repo_root),
        Task::Test => runner.run("cargo", CARGO_TEST_ARGS, &repo_root),
        Task::Bench => runner.run("cargo", CARGO_BENCH_ARGS, &repo_root),
        Task::Doc => runner.run("cargo", CARGO_DOC_ARGS, &repo_root),
        Task::Cov => runner.run("cargo", CARGO_COV_ARGS, &repo_root),
        Task::Ci => run_ci(&mut runner, &repo_root),
        Task::Publish => publish_release(&mut runner, &repo_root),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CARGO_BENCH_ARGS, CARGO_CLIPPY_ARGS, CARGO_COV_ARGS, CARGO_DOC_ARGS, CARGO_TEST_ARGS,
        CommandRunner, ensure_clean_worktree, ensure_tag_absent, is_clean_worktree,
        parse_pkgid_version, publish_release, run_ci, workspace_root_from_manifest_path,
    };
    use eyre::{Result, eyre};
    use std::collections::VecDeque;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// One expected fake runner call.
    #[derive(Debug)]
    struct FakeCall {
        /// Whether the call expects captured stdout.
        kind: CallKind,
        /// Program name.
        program: String,
        /// Command arguments.
        args: Vec<String>,
        /// Expected current directory.
        cwd: PathBuf,
        /// Result payload for the call.
        result: FakeResult,
    }

    /// Supported fake runner call kinds.
    #[derive(Debug, PartialEq, Eq)]
    enum CallKind {
        /// Command whose exit status is inspected.
        Run,
        /// Command whose stdout is captured.
        Output,
    }

    /// Supported fake runner responses.
    #[derive(Debug)]
    enum FakeResult {
        /// Successful status call.
        RunOk,
        /// Successful stdout call.
        OutputOk(String),
        /// Command failure.
        Err(String),
    }

    /// Queue-driven command runner for deterministic publish tests.
    #[derive(Debug, Default)]
    struct FakeRunner {
        /// Remaining expected calls.
        expected: VecDeque<FakeCall>,
    }

    impl FakeRunner {
        /// Build a fake runner from an ordered call list.
        fn new(expected: Vec<FakeCall>) -> Self {
            Self {
                expected: expected.into(),
            }
        }

        /// Assert that every expected call was consumed.
        fn assert_complete(&self) {
            assert!(
                self.expected.is_empty(),
                "unconsumed calls: {:?}",
                self.expected
            );
        }

        /// Pop and validate the next expected call.
        fn next(
            &mut self,
            kind: &CallKind,
            program: &str,
            args: &[&str],
            cwd: &Path,
        ) -> Result<FakeResult> {
            let call = self
                .expected
                .pop_front()
                .ok_or_else(|| eyre!("unexpected call: {program} {:?}", args))?;
            assert_eq!(&call.kind, kind, "call kind mismatch");
            assert_eq!(call.program, program, "program mismatch");
            assert_eq!(
                call.args,
                args.iter()
                    .map(|value| (*value).to_owned())
                    .collect::<Vec<_>>(),
                "argument mismatch"
            );
            assert_eq!(call.cwd, cwd, "cwd mismatch");
            Ok(call.result)
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&mut self, program: &str, args: &[&str], cwd: &Path) -> Result<()> {
            match self.next(&CallKind::Run, program, args, cwd)? {
                FakeResult::RunOk => Ok(()),
                FakeResult::Err(message) => Err(eyre!(message)),
                FakeResult::OutputOk(_) => Err(eyre!("expected run result for {program}")),
            }
        }

        fn output(&mut self, program: &str, args: &[&str], cwd: &Path) -> Result<String> {
            match self.next(&CallKind::Output, program, args, cwd)? {
                FakeResult::OutputOk(output) => Ok(output),
                FakeResult::Err(message) => Err(eyre!(message)),
                FakeResult::RunOk => Err(eyre!("expected output result for {program}")),
            }
        }
    }

    #[test]
    fn parse_pkgid_version_reads_fragment_only_version() {
        let pkgid = "path+file:///repo#1.2.3";

        let version = parse_pkgid_version(pkgid).expect("package version");

        assert_eq!(version, "1.2.3", "must read the package version");
    }

    #[test]
    fn parse_pkgid_version_reads_named_fragment_version() {
        let pkgid = "path+file:///repo#codexusage@1.2.3";

        let version = parse_pkgid_version(pkgid).expect("package version");

        assert_eq!(version, "1.2.3", "must read the package version");
    }

    #[test]
    fn parse_pkgid_version_rejects_missing_fragment() {
        let error = parse_pkgid_version("path+file:///repo").expect_err("invalid pkgid");

        assert!(
            error
                .to_string()
                .contains("does not contain a package fragment"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn workspace_root_from_manifest_path_uses_manifest_parent() {
        let repo_root = unique_temp_repo("workspace_root_from_manifest_path_uses_manifest_parent");
        let manifest = repo_root.join("Cargo.toml");
        fs::write(&manifest, "[workspace]\nmembers = [\"xtask\"]\n").expect("write manifest");

        let resolved =
            workspace_root_from_manifest_path(manifest.to_str().expect("manifest path utf-8"))
                .expect("workspace root");

        assert_eq!(resolved, repo_root, "must return manifest parent");
        fs::remove_dir_all(&repo_root).expect("cleanup repo");
    }

    #[test]
    fn is_clean_worktree_only_accepts_empty_status() {
        assert!(is_clean_worktree(""));
        assert!(is_clean_worktree("\n"));
        assert!(!is_clean_worktree(" M README.md"));
    }

    #[test]
    fn ensure_clean_worktree_rejects_dirty_status() {
        let repo_root = Path::new("/repo");
        let mut runner = FakeRunner::new(vec![FakeCall {
            kind: CallKind::Output,
            program: "git".to_owned(),
            args: vec!["status".to_owned(), "--porcelain".to_owned()],
            cwd: repo_root.to_path_buf(),
            result: FakeResult::OutputOk(" M README.md".to_owned()),
        }]);

        let error = ensure_clean_worktree(&mut runner, repo_root).expect_err("dirty worktree");

        assert!(
            error.to_string().contains("clean worktree"),
            "unexpected error: {error}"
        );
        runner.assert_complete();
    }

    #[test]
    fn ensure_tag_absent_rejects_existing_tag() {
        let repo_root = Path::new("/repo");
        let mut runner = FakeRunner::new(vec![FakeCall {
            kind: CallKind::Output,
            program: "git".to_owned(),
            args: vec!["tag".to_owned(), "--list".to_owned(), "1.2.3".to_owned()],
            cwd: repo_root.to_path_buf(),
            result: FakeResult::OutputOk("1.2.3".to_owned()),
        }]);

        let error = ensure_tag_absent(&mut runner, repo_root, "1.2.3").expect_err("tag exists");

        assert!(
            error.to_string().contains("already exists"),
            "unexpected error: {error}"
        );
        runner.assert_complete();
    }

    #[test]
    fn run_ci_executes_the_full_pipeline_in_order() {
        let repo_root = Path::new("/repo");
        let mut runner = FakeRunner::new(vec![
            fake_run("cargo", &["fmt", "--all", "--check"], repo_root),
            fake_run("cargo", CARGO_CLIPPY_ARGS, repo_root),
            fake_run("cargo", CARGO_TEST_ARGS, repo_root),
            fake_run("cargo", CARGO_BENCH_ARGS, repo_root),
            fake_run("cargo", CARGO_DOC_ARGS, repo_root),
            fake_run("cargo", CARGO_COV_ARGS, repo_root),
        ]);

        run_ci(&mut runner, repo_root).expect("ci pipeline");

        runner.assert_complete();
    }

    #[test]
    fn publish_release_runs_checks_publish_and_tag_in_order() {
        let repo_root = Path::new("/repo");

        let mut runner = FakeRunner::new(vec![
            fake_output("git", &["status", "--porcelain"], repo_root, ""),
            fake_output(
                "cargo",
                &["pkgid", "--quiet", "--manifest-path", "/repo/Cargo.toml"],
                repo_root,
                "path+file:///repo#codexusage@1.2.3",
            ),
            fake_output("git", &["tag", "--list", "1.2.3"], repo_root, ""),
            fake_run("cargo", &["fmt", "--all", "--check"], repo_root),
            fake_run("cargo", CARGO_CLIPPY_ARGS, repo_root),
            fake_run("cargo", CARGO_TEST_ARGS, repo_root),
            fake_run("cargo", CARGO_BENCH_ARGS, repo_root),
            fake_run("cargo", CARGO_DOC_ARGS, repo_root),
            fake_run("cargo", CARGO_COV_ARGS, repo_root),
            fake_run("cargo", &["publish", "--dry-run"], repo_root),
            fake_run("cargo", &["publish"], repo_root),
            fake_run("git", &["tag", "1.2.3"], repo_root),
        ]);

        publish_release(&mut runner, repo_root).expect("publish");

        runner.assert_complete();
    }

    #[test]
    fn publish_release_does_not_tag_when_publish_fails() {
        let repo_root = Path::new("/repo");

        let mut runner = FakeRunner::new(vec![
            fake_output("git", &["status", "--porcelain"], repo_root, ""),
            fake_output(
                "cargo",
                &["pkgid", "--quiet", "--manifest-path", "/repo/Cargo.toml"],
                repo_root,
                "path+file:///repo#codexusage@1.2.3",
            ),
            fake_output("git", &["tag", "--list", "1.2.3"], repo_root, ""),
            fake_run("cargo", &["fmt", "--all", "--check"], repo_root),
            fake_run("cargo", CARGO_CLIPPY_ARGS, repo_root),
            fake_run("cargo", CARGO_TEST_ARGS, repo_root),
            fake_run("cargo", CARGO_BENCH_ARGS, repo_root),
            fake_run("cargo", CARGO_DOC_ARGS, repo_root),
            fake_run("cargo", CARGO_COV_ARGS, repo_root),
            fake_run("cargo", &["publish", "--dry-run"], repo_root),
            FakeCall {
                kind: CallKind::Run,
                program: "cargo".to_owned(),
                args: vec!["publish".to_owned()],
                cwd: repo_root.to_path_buf(),
                result: FakeResult::Err("publish failed".to_owned()),
            },
        ]);

        let error = publish_release(&mut runner, repo_root).expect_err("publish failure");

        assert!(
            error.to_string().contains("publish failed"),
            "unexpected error: {error}"
        );
        runner.assert_complete();
    }

    /// Build one successful fake run call.
    fn fake_run(program: &str, args: &[&str], cwd: &Path) -> FakeCall {
        FakeCall {
            kind: CallKind::Run,
            program: program.to_owned(),
            args: args.iter().map(|value| (*value).to_owned()).collect(),
            cwd: cwd.to_path_buf(),
            result: FakeResult::RunOk,
        }
    }

    /// Build one successful fake output call.
    fn fake_output(program: &str, args: &[&str], cwd: &Path, stdout: &str) -> FakeCall {
        FakeCall {
            kind: CallKind::Output,
            program: program.to_owned(),
            args: args.iter().map(|value| (*value).to_owned()).collect(),
            cwd: cwd.to_path_buf(),
            result: FakeResult::OutputOk(stdout.to_owned()),
        }
    }

    /// Create one unique temporary repository root for filesystem-based tests.
    fn unique_temp_repo(test_name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("xtask-{test_name}-{unique}"));
        fs::create_dir_all(&path).expect("create temp repo");
        path
    }
}
