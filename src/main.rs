#![warn(clippy::pedantic)]
#![warn(clippy::cargo)]
#![allow(clippy::semicolon_if_nothing_returned)]
#![allow(clippy::let_underscore_drop)]
#![allow(clippy::single_match_else)]

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process;
use std::str::FromStr;

use chrono::{Date, Duration, NaiveDate, Utc};
use clap::{ArgEnum, Parser, PossibleValue};
use colored::Colorize;
use anyhow::{bail, Context};
use log::debug;
use reqwest::blocking::Client;

mod git;
mod github;
mod least_satisfying;
mod repo_access;
mod toolchains;

use crate::least_satisfying::{least_satisfying, Satisfies};
use crate::repo_access::{AccessViaGithub, AccessViaLocalGit, RustRepositoryAccessor};
use crate::toolchains::{
    DownloadParams, InstallError, NIGHTLY_SERVER, TestOutcome, Toolchain, ToolchainSpec,
    YYYY_MM_DD, download_progress, parse_to_utc_date,
};

#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub date: GitDate,
    pub summary: String,
}

/// The first commit which build artifacts are made available through the CI for
/// bisection.
///
/// Due to our deletion policy which expires builds after 167 days, the build
/// artifacts of this commit itself is no longer available, so this may not be entirely useful;
/// however, it does limit the amount of commits somewhat.
const EPOCH_COMMIT: &str = "927c55d86b0be44337f37cf5b0a76fb8ba86e06c";

const REPORT_HEADER: &str = "\
==================================================================================
= Please file this regression report on the rust-lang/rust GitHub repository     =
=        New issue: https://github.com/rust-lang/rust/issues/new                 =
=     Known issues: https://github.com/rust-lang/rust/issues                     =
= Copy and paste the text below into the issue report thread.  Thanks!           =
==================================================================================";

#[derive(Debug, Parser)]
#[clap(bin_name = "cargo", subcommand_required = true)]
enum Cargo {
    BisectRustc(Opts),
}

#[derive(Debug, Parser)]
#[clap(
    bin_name = "cargo bisect-rustc",
    version,
    about,
    after_help = "EXAMPLES:
    Run a fully automatic nightly bisect doing `cargo check`:
    ```
    cargo bisect-rustc --start 2018-07-07 --end 2018-07-30 --test-dir ../my_project/ -- check
    ```

    Run a PR-based bisect with manual prompts after each run doing `cargo build`:
    ```
    cargo bisect-rustc --start 6a1c0637ce44aeea6c60527f4c0e7fb33f2bcd0d \\
      --end 866a713258915e6cbb212d135f751a6a8c9e1c0a --test-dir ../my_project/ --prompt -- build
    ```"
)]
#[allow(clippy::struct_excessive_bools)]
struct Opts {
    #[clap(
        long,
        help = "Custom regression definition",
        arg_enum,
        default_value_t = RegressOn::ErrorStatus,
    )]
    regress: RegressOn,

    #[clap(short, long, help = "Download the alt build instead of normal build")]
    alt: bool,

    #[clap(
        long,
        help = "Host triple for the compiler",
        default_value_t = option_env!("HOST").unwrap_or("unkown").to_string(),
        validator = validate_host
        )]
    host: String,

    #[clap(long, help = "Cross-compilation target platform")]
    target: Option<String>,

    #[clap(long, help = "Preserve the downloaded artifacts")]
    preserve: bool,

    #[clap(long, help = "Preserve the target directory used for builds")]
    preserve_target: bool,

    #[clap(long, help = "Download rust-src [default: no download]")]
    with_src: bool,

    #[clap(long, help = "Download rustc-dev [default: no download]")]
    with_dev: bool,

    #[clap(short, long = "component", help = "additional components to install")]
    components: Vec<String>,

    #[clap(
        long,
        help = "Root directory for tests",
        default_value = ".",
        parse(from_os_str),
        validator = validate_dir
    )]
    test_dir: PathBuf,

    #[clap(long, help = "Manually evaluate for regression with prompts")]
    prompt: bool,

    #[clap(
        long,
        short,
        help = "Assume failure after specified number of seconds (for bisecting hangs)"
    )]
    timeout: Option<usize>,

    #[clap(short, long = "verbose", parse(from_occurrences))]
    verbosity: usize,

    #[clap(
        help = "Arguments to pass to cargo or the file specified by --script during tests",
        multiple_values = true,
        last = true,
        parse(from_os_str)
    )]
    command_args: Vec<OsString>,

    #[clap(
        long,
        help = "Left bound for search (*without* regression). You can use \
a date (YYYY-MM-DD), git tag name (e.g. 1.58.0) or git commit SHA."
    )]
    start: Option<Bound>,

    #[clap(
        long,
        help = "Right bound for search (*with* regression). You can use \
a date (YYYY-MM-DD), git tag name (e.g. 1.58.0) or git commit SHA."
    )]
    end: Option<Bound>,

    #[clap(long, help = "Bisect via commit artifacts")]
    by_commit: bool,

    #[clap(long, arg_enum, help = "How to access Rust git repository", default_value_t = Access::Checkout)]
    access: Access,

    #[clap(long, help = "Install the given artifact")]
    install: Option<Bound>,

    #[clap(long, help = "Force installation over existing artifacts")]
    force_install: bool,

    #[clap(
        long,
        help = "Script replacement for `cargo build` command",
        parse(from_os_str),
        validator = validate_file,
    )]
    script: Option<PathBuf>,

    #[clap(long, help = "Do not install cargo [default: install cargo]")]
    without_cargo: bool,
}

pub type GitDate = Date<Utc>;

fn validate_dir(s: &str) -> anyhow::Result<()> {
    let path: PathBuf = s.parse()?;
    if path.is_dir() {
        Ok(())
    } else {
        bail!(
            "{} is not an existing directory",
            path.canonicalize()?.display()
        )
    }
}

fn validate_file(s: &str) -> anyhow::Result<()> {
    let path: PathBuf = s.parse()?;
    if path.is_file() {
        Ok(())
    } else {
        bail!("{} is not an existing file", path.canonicalize()?.display())
    }
}

fn validate_host(s: &str) -> anyhow::Result<()> {
    if s == "unknown" {
        bail!(
            "Failed to auto-detect host triple and was not specified. Please provide it via --host"
        )
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug)]
enum Bound {
    Commit(String),
    Date(GitDate),
}

impl FromStr for Bound {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_to_utc_date(s)
            .map(Self::Date)
            .or_else(|_| Ok(Self::Commit(s.to_string())))
    }
}

impl Bound {
    fn sha(&self) -> anyhow::Result<String> {
        match self {
            Bound::Commit(commit) => Ok(commit.clone()),
            Bound::Date(date) => {
                let date_str = date.format(YYYY_MM_DD);
                let url =
                    format!("{NIGHTLY_SERVER}/{date_str}/channel-rust-nightly-git-commit-hash.txt");

                eprintln!("fetching {url}");
                let client = Client::new();
                let name = format!("nightly manifest {date_str}");
                let mut response = download_progress(&client, &name, &url)?;
                let mut commit = String::new();
                response.read_to_string(&mut commit)?;

                eprintln!("converted {date_str} to {commit}");

                Ok(commit)
            }
        }
    }

    fn as_commit(&self) -> anyhow::Result<Self> {
        self.sha().map(Bound::Commit)
    }
}

impl Opts {
    fn emit_cargo_output(&self) -> bool {
        self.verbosity >= 2
    }
}

#[derive(Debug, thiserror::Error)]
struct ExitError(i32);

impl fmt::Display for ExitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exiting with {}", self.0)
    }
}

impl Config {
    fn default_outcome_of_output(&self, output: &process::Output) -> TestOutcome {
        let status = output.status;
        let stdout_utf8 = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr_utf8 = String::from_utf8_lossy(&output.stderr).to_string();

        debug!(
            "status: {:?} stdout: {:?} stderr: {:?}",
            status, stdout_utf8, stderr_utf8
        );

        let saw_ice = stderr_utf8.contains("error: internal compiler error")
            || stderr_utf8.contains("' has overflowed its stack");

        let input = (self.args.regress, status.success());
        let result = match input {
            (RegressOn::ErrorStatus, true) | (RegressOn::SuccessStatus, false) => {
                TestOutcome::Baseline
            }
            (RegressOn::ErrorStatus, false)
            | (RegressOn::SuccessStatus | RegressOn::NonCleanError, true) => TestOutcome::Regressed,
            (RegressOn::IceAlone, _) | (RegressOn::NonCleanError, false) => {
                if saw_ice {
                    TestOutcome::Regressed
                } else {
                    TestOutcome::Baseline
                }
            }
            (RegressOn::NotIce, _) => {
                if saw_ice {
                    TestOutcome::Baseline
                } else {
                    TestOutcome::Regressed
                }
            }
        };
        debug!(
            "default_outcome_of_output: input: {:?} result: {:?}",
            input, result
        );
        result
    }
}

#[derive(ArgEnum, Clone, Debug)]
enum Access {
    Checkout,
    Github,
}

impl Access {
    fn repo(&self) -> Box<dyn RustRepositoryAccessor> {
        match self {
            Self::Checkout => Box::new(AccessViaLocalGit),
            Self::Github => Box::new(AccessViaGithub),
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
/// Customize what is treated as regression.
enum RegressOn {
    /// `ErrorStatus`: Marks test outcome as `Regressed` if and only if
    /// the `rustc` process reports a non-success status. This corresponds to
    /// when `rustc` has an internal compiler error (ICE) or when it detects an
    /// error in the input program.
    ///
    /// This covers the most common use case for `cargo-bisect-rustc` and is
    /// thus the default setting.
    ///
    /// You explicitly opt into this seting via `--regress=error`.
    ErrorStatus,

    /// `SuccessStatus`: Marks test outcome as `Regressed` if and only
    /// if the `rustc` process reports a success status. This corresponds to
    /// when `rustc` believes it has successfully compiled the program. This
    /// covers the use case for when you want to bisect to see when a bug was
    /// fixed.
    ///
    /// You explicitly opt into this seting via `--regress=success`.
    SuccessStatus,

    /// `IceAlone`: Marks test outcome as `Regressed` if and only if
    /// the `rustc` process issues a diagnostic indicating that an internal
    /// compiler error (ICE) occurred. This covers the use case for when you
    /// want to bisect to see when an ICE was introduced pon a codebase that is
    /// meant to produce a clean error.
    ///
    /// You explicitly opt into this seting via `--regress=ice`.
    IceAlone,

    /// `NotIce`: Marks test outcome as `Regressed` if and only if
    /// the `rustc` process does not issue a diagnostic indicating that an
    /// internal compiler error (ICE) occurred. This covers the use case for
    /// when you want to bisect to see when an ICE was fixed.
    ///
    /// You explicitly opt into this setting via `--regress=non-ice`
    NotIce,

    /// `NonCleanError`: Marks test outcome as `Baseline` if and only
    /// if the `rustc` process reports error status and does not issue any
    /// diagnostic indicating that an internal compiler error (ICE) occurred.
    /// This is the use case if the regression is a case where an ill-formed
    /// program has stopped being properly rejected by the compiler.
    ///
    /// (The main difference between this case and `SuccessStatus` is
    /// the handling of ICE: `SuccessStatus` assumes that ICE should be
    /// considered baseline; `NonCleanError` assumes ICE should be
    /// considered a sign of a regression.)
    ///
    /// You explicitly opt into this seting via `--regress=non-error`.
    NonCleanError,
}

impl ArgEnum for RegressOn {
    fn value_variants<'a>() -> &'a [Self] {
        &[
            Self::ErrorStatus,
            Self::SuccessStatus,
            Self::IceAlone,
            Self::NotIce,
            Self::NonCleanError,
        ]
    }
    fn to_possible_value<'a>(&self) -> Option<PossibleValue<'a>> {
        Some(PossibleValue::new(match self {
            Self::ErrorStatus => "error",
            Self::NonCleanError => "non-error",
            Self::IceAlone => "ice",
            Self::NotIce => "non-ice",
            Self::SuccessStatus => "success",
        }))
    }
}

impl RegressOn {
    fn must_process_stderr(self) -> bool {
        match self {
            RegressOn::ErrorStatus | RegressOn::SuccessStatus => false,
            RegressOn::NonCleanError | RegressOn::IceAlone | RegressOn::NotIce => true,
        }
    }
}

struct Config {
    args: Opts,
    rustup_tmp_path: PathBuf,
    toolchains_path: PathBuf,
    target: String,
    is_commit: bool,
    client: Client,
}

impl Config {
    fn from_args(mut args: Opts) -> anyhow::Result<Config> {
        let target = args.target.clone().unwrap_or_else(|| args.host.clone());

        let mut toolchains_path = home::rustup_home()?;

        // We will download and extract the tarballs into this directory before installing.
        // Using `~/.rustup/tmp` instead of $TMPDIR ensures we could always perform installation by
        // renaming instead of copying the whole directory.
        let rustup_tmp_path = toolchains_path.join("tmp");
        if !rustup_tmp_path.exists() {
            fs::create_dir(&rustup_tmp_path)?;
        }

        toolchains_path.push("toolchains");
        if !toolchains_path.is_dir() {
            bail!(
                "`{}` is not a directory. Please install rustup.",
                toolchains_path.display()
            );
        }

        let is_commit = match (args.start.clone(), args.end.clone()) {
            (Some(Bound::Commit(_)) | None, Some(Bound::Commit(_)))
            | (Some(Bound::Commit(_)), None) => Some(true),

            (Some(Bound::Date(_)) | None, Some(Bound::Date(_))) | (Some(Bound::Date(_)), None) => {
                Some(false)
            }

            (None, None) => None,

            (start, end) => bail!(
                "cannot take different types of bounds for start/end, got start: {:?} and end {:?}",
                start,
                end
            ),
        };

        if is_commit == Some(false) && args.by_commit {
            eprintln!("finding commit range that corresponds to dates specified");
            match (args.start, args.end) {
                (Some(b1), Some(b2)) => {
                    args.start = Some(b1.as_commit()?);
                    args.end = Some(b2.as_commit()?);
                }
                _ => unreachable!(),
            }
        }

        Ok(Config {
            is_commit: args.by_commit || is_commit == Some(true),
            args,
            target,
            toolchains_path,
            rustup_tmp_path,
            client: Client::new(),
        })
    }
}

/// Translates a tag-like bound (such as `1.62.0`) to a `Bound::Date` so that
/// bisecting works for versions older than 167 days.
fn fixup_bounds(
    access: &Access,
    start: &mut Option<Bound>,
    end: &mut Option<Bound>,
) -> anyhow::Result<()> {
    let is_tag = |bound: &Option<Bound>| -> bool {
        match bound {
            Some(Bound::Commit(commit)) => commit.contains('.'),
            None | Some(Bound::Date(_)) => false,
        }
    };
    let is_datelike = |bound: &Option<Bound>| -> bool {
        matches!(bound, None | Some(Bound::Date(_))) || is_tag(bound)
    };
    if !(is_datelike(start) && is_datelike(end)) {
        // If the user specified an actual commit for one bound, then don't
        // even try to convert the other bound to a date.
        return Ok(());
    }
    let fixup = |which: &str, bound: &mut Option<Bound>| -> anyhow::Result<()> {
        if is_tag(bound) {
            if let Some(Bound::Commit(tag)) = bound {
                let date = access.repo().bound_to_date(Bound::Commit(tag.clone()))?;
                eprintln!(
                    "translating --{which}={tag} to {date}",
                    date = date.format(YYYY_MM_DD)
                );
                *bound = Some(Bound::Date(date));
            }
        }
        Ok(())
    };
    fixup("start", start)?;
    fixup("end", end)?;
    Ok(())
}

fn check_bounds(start: &Option<Bound>, end: &Option<Bound>) -> anyhow::Result<()> {
    // current UTC date
    let current = Utc::today();
    match start.as_ref().zip(end.as_ref()) {
        // start date is after end date
        Some((Bound::Date(start), Bound::Date(end))) if end < start => {
            bail!(
                "end should be after start, got start: {} and end {}",
                start,
                end
            );
        }
        // start date is after current date
        Some((Bound::Date(start), _)) if start > &current => {
            bail!(
                "start date should be on or before current date, got start date request: {} and current date is {}",
                start,
                current
            );
        }
        // end date is after current date
        Some((_, Bound::Date(end))) if end > &current => {
            bail!(
                "end date should be on or before current date, got start date request: {} and current date is {}",
                end,
                current
            );
        }
        _ => Ok(()),
    }
}

// Application entry point
fn run() -> anyhow::Result<()> {
    env_logger::try_init()?;
    let mut args = match Cargo::try_parse() {
        Ok(Cargo::BisectRustc(args)) => args,
        Err(e) => match e.context().next() {
            None => {
                Cargo::parse();
                unreachable!()
            }
            _ => Opts::parse(),
        },
    };
    fixup_bounds(&args.access, &mut args.start, &mut args.end)?;
    check_bounds(&args.start, &args.end)?;
    let cfg = Config::from_args(args)?;

    if let Some(ref bound) = cfg.args.install {
        cfg.install(bound)
    } else {
        cfg.bisect()
    }
}

impl Config {
    fn install(&self, bound: &Bound) -> anyhow::Result<()> {
        match *bound {
            Bound::Commit(ref sha) => {
                let sha = self.args.access.repo().commit(sha)?.sha;
                let mut t = Toolchain {
                    spec: ToolchainSpec::Ci {
                        commit: sha,
                        alt: self.args.alt,
                    },
                    host: self.args.host.clone(),
                    std_targets: vec![self.args.host.clone(), self.target.clone()],
                };
                t.std_targets.sort();
                t.std_targets.dedup();
                let dl_params = DownloadParams::for_ci(self);
                t.install(&self.client, &dl_params)?;
            }
            Bound::Date(date) => {
                let mut t = Toolchain {
                    spec: ToolchainSpec::Nightly { date },
                    host: self.args.host.clone(),
                    std_targets: vec![self.args.host.clone(), self.target.clone()],
                };
                t.std_targets.sort();
                t.std_targets.dedup();
                let dl_params = DownloadParams::for_nightly(self);
                t.install(&self.client, &dl_params)?;
            }
        }

        Ok(())
    }

    // bisection entry point
    fn bisect(&self) -> anyhow::Result<()> {
        if self.is_commit {
            let bisection_result = self.bisect_ci()?;
            self.print_results(&bisection_result);
        } else {
            let nightly_bisection_result = self.bisect_nightlies()?;
            self.print_results(&nightly_bisection_result);
            let nightly_regression =
                &nightly_bisection_result.searched[nightly_bisection_result.found];

            if let ToolchainSpec::Nightly { date } = nightly_regression.spec {
                let previous_date = date.pred();

                let working_commit = Bound::Date(previous_date).sha()?;
                let bad_commit = Bound::Date(date).sha()?;
                eprintln!(
                    "looking for regression commit between {} and {}",
                    previous_date.format(YYYY_MM_DD),
                    date.format(YYYY_MM_DD),
                );

                let ci_bisection_result = self.bisect_ci_via(&working_commit, &bad_commit)?;

                self.print_results(&ci_bisection_result);
                print_final_report(self, &nightly_bisection_result, &ci_bisection_result);
            }
        }

        Ok(())
    }
}

fn searched_range(
    cfg: &Config,
    searched_toolchains: &[Toolchain],
) -> (ToolchainSpec, ToolchainSpec) {
    let first_toolchain = searched_toolchains.first().unwrap().spec.clone();
    let last_toolchain = searched_toolchains.last().unwrap().spec.clone();

    match (&first_toolchain, &last_toolchain) {
        (ToolchainSpec::Ci { .. }, ToolchainSpec::Ci { .. }) => (first_toolchain, last_toolchain),

        _ => {
            let start_toolchain = if let Some(Bound::Date(date)) = cfg.args.start {
                ToolchainSpec::Nightly { date }
            } else {
                first_toolchain
            };

            (
                start_toolchain,
                ToolchainSpec::Nightly {
                    date: get_end_date(cfg),
                },
            )
        }
    }
}

impl Config {
    fn print_results(&self, bisection_result: &BisectionResult) {
        let BisectionResult {
            searched: toolchains,
            dl_spec,
            found,
        } = bisection_result;

        let (start, end) = searched_range(self, toolchains);

        eprintln!("searched toolchains {} through {}", start, end);

        if toolchains[*found] == *toolchains.last().unwrap() {
            // FIXME: Ideally the BisectionResult would contain the final result.
            // This ends up testing a toolchain that was already tested.
            // I believe this is one of the duplicates mentioned in
            // https://github.com/rust-lang/cargo-bisect-rustc/issues/85
            eprintln!("checking last toolchain to determine final result");
            let t = &toolchains[*found];
            let r = match t.install(&self.client, dl_spec) {
                Ok(()) => {
                    let outcome = t.test(self);
                    remove_toolchain(self, t, dl_spec);
                    // we want to fail, so a successful build doesn't satisfy us
                    match outcome {
                        TestOutcome::Baseline => Satisfies::No,
                        TestOutcome::Regressed => Satisfies::Yes,
                    }
                }
                Err(_) => {
                    let _ = t.remove(dl_spec);
                    Satisfies::Unknown
                }
            };
            match r {
                Satisfies::Yes => {}
                Satisfies::No | Satisfies::Unknown => {
                    eprintln!(
                        "error: The regression was not found. Expanding the bounds may help."
                    );
                    return;
                }
            }
        }

        let tc_found = format!("Regression in {}", toolchains[*found]);
        eprintln!();
        eprintln!();
        eprintln!("{}", "*".repeat(80).dimmed().bold());
        eprintln!("{}", tc_found.red());
        eprintln!("{}", "*".repeat(80).dimmed().bold());
        eprintln!();
    }
}

fn remove_toolchain(cfg: &Config, toolchain: &Toolchain, dl_params: &DownloadParams) {
    if cfg.args.preserve {
        // If `rustup toolchain link` was used to link to nightly, then even
        // with --preserve, the toolchain link should be removed, otherwise it
        // will go stale after 24 hours.
        let toolchain_dir = cfg.toolchains_path.join(toolchain.rustup_name());
        match fs::symlink_metadata(&toolchain_dir) {
            Ok(meta) => {
                #[cfg(windows)]
                let is_junction = {
                    use std::os::windows::fs::MetadataExt;
                    (meta.file_attributes() & 1024) != 0
                };
                #[cfg(not(windows))]
                let is_junction = false;
                if !meta.file_type().is_symlink() && !is_junction {
                    return;
                }
                debug!("removing linked toolchain {}", toolchain);
            }
            Err(e) => {
                debug!(
                    "remove_toolchain: cannot stat toolchain {}: {}",
                    toolchain, e
                );
                return;
            }
        }
    }
    if let Err(e) = toolchain.remove(dl_params) {
        debug!(
            "failed to remove toolchain {} in {}: {}",
            toolchain,
            cfg.toolchains_path.display(),
            e
        );
    }
}

fn print_final_report(
    cfg: &Config,
    nightly_bisection_result: &BisectionResult,
    ci_bisection_result: &BisectionResult,
) {
    let BisectionResult {
        searched: nightly_toolchains,
        found: nightly_found,
        ..
    } = nightly_bisection_result;

    let BisectionResult {
        searched: ci_toolchains,
        found: ci_found,
        ..
    } = ci_bisection_result;

    eprintln!("{}", REPORT_HEADER.dimmed());
    eprintln!();

    let (start, end) = searched_range(cfg, nightly_toolchains);

    eprintln!("searched nightlies: from {} to {}", start, end);

    eprintln!("regressed nightly: {}", nightly_toolchains[*nightly_found],);

    eprintln!(
        "searched commit range: https://github.com/rust-lang/rust/compare/{0}...{1}",
        ci_toolchains.first().unwrap(),
        ci_toolchains.last().unwrap(),
    );

    eprintln!(
        "regressed commit: https://github.com/rust-lang/rust/commit/{}",
        ci_toolchains[*ci_found],
    );

    eprintln!();
    eprintln!("<details>");
    eprintln!(
        "<summary>bisected with <a href='{}'>cargo-bisect-rustc</a> v{}</summary>",
        env!("CARGO_PKG_REPOSITORY"),
        env!("CARGO_PKG_VERSION"),
    );
    eprintln!();
    eprintln!();
    if let Some(host) = option_env!("HOST") {
        eprintln!("Host triple: {}", host);
    }

    eprintln!("Reproduce with:");
    eprintln!("```bash");
    eprint!("cargo bisect-rustc ");
    for (index, arg) in env::args_os().enumerate() {
        if index > 1 {
            eprint!("{} ", arg.to_string_lossy());
        }
    }
    eprintln!();
    eprintln!("```");
    eprintln!("</details>");
}

struct NightlyFinderIter {
    start_date: GitDate,
    current_date: GitDate,
}

impl NightlyFinderIter {
    fn new(start_date: GitDate) -> Self {
        Self {
            start_date,
            current_date: start_date,
        }
    }
}

impl Iterator for NightlyFinderIter {
    type Item = GitDate;

    fn next(&mut self) -> Option<GitDate> {
        let current_distance = self.start_date - self.current_date;

        let jump_length = if current_distance.num_days() < 7 {
            // first week jump by two days
            2
        } else if current_distance.num_days() < 49 {
            // from 2nd to 7th week jump weekly
            7
        } else {
            // from 7th week jump by two weeks
            14
        };

        self.current_date = self.current_date - Duration::days(jump_length);
        Some(self.current_date)
    }
}

impl Config {
    fn install_and_test(
        &self,
        t: &Toolchain,
        dl_spec: &DownloadParams,
    ) -> Result<Satisfies, InstallError> {
        match t.install(&self.client, dl_spec) {
            Ok(()) => {
                let outcome = t.test(self);
                // we want to fail, so a successful build doesn't satisfy us
                let r = match outcome {
                    TestOutcome::Baseline => Satisfies::No,
                    TestOutcome::Regressed => Satisfies::Yes,
                };
                eprintln!("RESULT: {}, ===> {}", t, r);
                remove_toolchain(self, t, dl_spec);
                eprintln!();
                Ok(r)
            }
            Err(error) => {
                remove_toolchain(self, t, dl_spec);
                Err(error)
            }
        }
    }

    fn bisect_to_regression(&self, toolchains: &[Toolchain], dl_spec: &DownloadParams) -> usize {
        least_satisfying(toolchains, |t, remaining, estimate| {
            eprintln!(
                "{remaining} versions remaining to test after this (roughly {estimate} steps)"
            );
            self.install_and_test(t, dl_spec)
                .unwrap_or(Satisfies::Unknown)
        })
    }
}

fn get_start_date(cfg: &Config) -> Date<Utc> {
    if let Some(Bound::Date(date)) = cfg.args.start {
        date
    } else {
        get_end_date(cfg)
    }
}

fn get_end_date(cfg: &Config) -> Date<Utc> {
    if let Some(Bound::Date(date)) = cfg.args.end {
        date
    } else if let Some(date) = Toolchain::default_nightly() {
        date
    } else {
        Utc::today()
    }
}

fn date_is_future(test_date: Date<Utc>) -> bool {
    test_date > Utc::today()
}

impl Config {
    // nightlies branch of bisect execution
    fn bisect_nightlies(&self) -> anyhow::Result<BisectionResult> {
        if self.args.alt {
            bail!("cannot bisect nightlies with --alt: not supported");
        }

        let dl_spec = DownloadParams::for_nightly(self);

        // before this date we didn't have -std packages
        let end_at = Date::from_utc(NaiveDate::from_ymd(2015, 10, 20), Utc);
        let mut first_success = None;

        let mut nightly_date = get_start_date(self);
        let mut last_failure = get_end_date(self);
        let has_start = self.args.start.is_some();

        // validate start and end dates to confirm that they are not future dates
        // start date validation
        if has_start && date_is_future(nightly_date) {
            bail!(
                "start date must be on or before the current date. received start date request {}",
                nightly_date
            )
        }
        // end date validation
        if date_is_future(last_failure) {
            bail!(
                "end date must be on or before the current date. received end date request {}",
                nightly_date
            )
        }

        let mut nightly_iter = NightlyFinderIter::new(nightly_date);

        // this loop tests nightly toolchains to:
        // (1) validate that start date does not have regression (if defined on command line)
        // (2) identify a nightly date range for the bisection routine
        //
        // The tests here must be constrained to dates after 2015-10-20 (`end_at` date)
        // because -std packages were not available prior
        while nightly_date > end_at {
            let mut t = Toolchain {
                spec: ToolchainSpec::Nightly { date: nightly_date },
                host: self.args.host.clone(),
                std_targets: vec![self.args.host.clone(), self.target.clone()],
            };
            t.std_targets.sort();
            t.std_targets.dedup();
            if t.is_current_nightly() {
                eprintln!(
                    "checking {} from the currently installed default nightly \
                       toolchain as the last failure",
                    t
                );
            }

            eprintln!("checking the start range to find a passing nightly");
            match self.install_and_test(&t, &dl_spec) {
                Ok(r) => {
                    // If Satisfies::No, then the regression was not identified in this nightly.
                    // Break out of the loop and use this as the start date for the
                    // bisection range
                    if r == Satisfies::No {
                        first_success = Some(nightly_date);
                        break;
                    } else if has_start {
                        // If this date was explicitly defined on the command line &
                        // has regression, then this is an error in the test definition.
                        // The user must re-define the start date and try again
                        bail!(
                            "the start of the range ({}) must not reproduce the regression",
                            t
                        );
                    }
                    last_failure = nightly_date;
                    nightly_date = nightly_iter.next().unwrap();
                }
                Err(InstallError::NotFound { .. }) => {
                    // go back just one day, presumably missing a nightly
                    nightly_date = nightly_date.pred();
                    eprintln!(
                        "*** unable to install {}. roll back one day and try again...",
                        t
                    );
                    if has_start {
                        bail!("could not find {}", t);
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }

        let first_success = first_success.context("could not find a nightly that built")?;

        // confirm that the end of the date range has the regression
        let mut t_end = Toolchain {
            spec: ToolchainSpec::Nightly { date: last_failure },
            host: self.args.host.clone(),
            std_targets: vec![self.args.host.clone(), self.target.clone()],
        };
        t_end.std_targets.sort();
        t_end.std_targets.dedup();

        eprintln!("checking the end range to verify it does not pass");
        let result_nightly = self.install_and_test(&t_end, &dl_spec)?;
        // The regression was not identified in this nightly.
        if result_nightly == Satisfies::No {
            bail!(
                "the end of the range ({}) does not reproduce the regression",
                t_end
            );
        }

        let toolchains = toolchains_between(
            self,
            ToolchainSpec::Nightly {
                date: first_success,
            },
            ToolchainSpec::Nightly { date: last_failure },
        );

        let found = self.bisect_to_regression(&toolchains, &dl_spec);

        Ok(BisectionResult {
            dl_spec,
            searched: toolchains,
            found,
        })
    }
}

fn toolchains_between(cfg: &Config, a: ToolchainSpec, b: ToolchainSpec) -> Vec<Toolchain> {
    match (a, b) {
        (ToolchainSpec::Nightly { date: a }, ToolchainSpec::Nightly { date: b }) => {
            let mut toolchains = Vec::new();
            let mut date = a;
            let mut std_targets = vec![cfg.args.host.clone(), cfg.target.clone()];
            std_targets.sort();
            std_targets.dedup();
            while date <= b {
                let t = Toolchain {
                    spec: ToolchainSpec::Nightly { date },
                    host: cfg.args.host.clone(),
                    std_targets: std_targets.clone(),
                };
                toolchains.push(t);
                date = date.succ();
            }
            toolchains
        }
        _ => unimplemented!(),
    }
}

impl Config {
    // CI branch of bisect execution
    fn bisect_ci(&self) -> anyhow::Result<BisectionResult> {
        eprintln!("bisecting ci builds");
        let start = if let Some(Bound::Commit(ref sha)) = self.args.start {
            sha
        } else {
            EPOCH_COMMIT
        };

        let end = if let Some(Bound::Commit(ref sha)) = self.args.end {
            sha
        } else {
            "origin/master"
        };

        eprintln!("starting at {}, ending at {}", start, end);

        self.bisect_ci_via(start, end)
    }

    fn bisect_ci_via(&self, start_sha: &str, end_ref: &str) -> anyhow::Result<BisectionResult> {
        let access = self.args.access.repo();
        let end_sha = access.commit(end_ref)?.sha;
        let commits = access.commits(start_sha, &end_sha)?;

        assert_eq!(commits.last().expect("at least one commit").sha, end_sha);

        commits.iter().zip(commits.iter().skip(1)).all(|(a, b)| {
            let sorted_by_date = a.date <= b.date;
            assert!(
                sorted_by_date,
                "commits must chronologically ordered,\
                                 but {:?} comes after {:?}",
                a, b
            );
            sorted_by_date
        });

        for (j, commit) in commits.iter().enumerate() {
            eprintln!(
                "  commit[{}] {}: {}",
                j,
                commit.date,
                commit.summary.split('\n').next().unwrap()
            )
        }

        self.bisect_ci_in_commits(start_sha, &end_sha, commits)
    }

    fn bisect_ci_in_commits(
        &self,
        start: &str,
        end: &str,
        mut commits: Vec<Commit>,
    ) -> anyhow::Result<BisectionResult> {
        let dl_spec = DownloadParams::for_ci(self);
        commits.retain(|c| Utc::today() - c.date < Duration::days(167));

        if commits.is_empty() {
            bail!(
                "no CI builds available between {} and {} within last 167 days",
                start,
                end
            );
        }

        if let Some(c) = commits.last() {
            if end != "origin/master" && !c.sha.starts_with(end) {
                bail!("expected to end with {}, but ended with {}", end, c.sha);
            }
        }

        eprintln!("validated commits found, specifying toolchains");
        eprintln!();

        let toolchains = commits
            .into_iter()
            .map(|commit| {
                let mut t = Toolchain {
                    spec: ToolchainSpec::Ci {
                        commit: commit.sha,
                        alt: self.args.alt,
                    },
                    host: self.args.host.clone(),
                    std_targets: vec![self.args.host.clone(), self.target.clone()],
                };
                t.std_targets.sort();
                t.std_targets.dedup();
                t
            })
            .collect::<Vec<_>>();

        if !toolchains.is_empty() {
            // validate commit at start of range
            eprintln!("checking the start range to verify it passes");
            let start_range_result = self.install_and_test(&toolchains[0], &dl_spec)?;
            if start_range_result == Satisfies::Yes {
                bail!(
                    "the commit at the start of the range ({}) includes the regression",
                    &toolchains[0]
                );
            }

            // validate commit at end of range
            eprintln!("checking the end range to verify it does not pass");
            let end_range_result =
                self.install_and_test(&toolchains[toolchains.len() - 1], &dl_spec)?;
            if end_range_result == Satisfies::No {
                bail!(
                    "the commit at the end of the range ({}) does not reproduce the regression",
                    &toolchains[toolchains.len() - 1]
                );
            }
        }

        let found = self.bisect_to_regression(&toolchains, &dl_spec);

        Ok(BisectionResult {
            searched: toolchains,
            found,
            dl_spec,
        })
    }
}

#[derive(Clone)]
struct BisectionResult {
    searched: Vec<Toolchain>,
    found: usize,
    dl_spec: DownloadParams,
}

fn main() {
    if let Err(err) = run() {
        match err.downcast::<ExitError>() {
            Ok(ExitError(code)) => process::exit(code),
            Err(err) => {
                let error_str = "ERROR:".red().bold();
                eprintln!("{} {}", error_str, err);
                process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Start and end date validations
    #[test]
    fn test_check_bounds_valid_bounds() {
        let date1 = chrono::Utc::today().pred();
        let date2 = chrono::Utc::today().pred();
        assert!(check_bounds(&Some(Bound::Date(date1)), &Some(Bound::Date(date2))).is_ok());
    }

    #[test]
    fn test_check_bounds_invalid_start_after_end() {
        let start = chrono::Utc::today();
        let end = chrono::Utc::today().pred();
        assert!(check_bounds(&Some(Bound::Date(start)), &Some(Bound::Date(end))).is_err());
    }

    #[test]
    fn test_check_bounds_invalid_start_after_current() {
        let start = chrono::Utc::today().succ();
        let end = chrono::Utc::today();
        assert!(check_bounds(&Some(Bound::Date(start)), &Some(Bound::Date(end))).is_err());
    }

    #[test]
    fn test_check_bounds_invalid_end_after_current() {
        let start = chrono::Utc::today();
        let end = chrono::Utc::today().succ();
        assert!(check_bounds(&Some(Bound::Date(start)), &Some(Bound::Date(end))).is_err());
    }

    #[test]
    fn test_nightly_finder_iterator() {
        let start_date = Date::from_utc(NaiveDate::from_ymd(2019, 01, 01), Utc);

        let iter = NightlyFinderIter::new(start_date);

        for (date, i) in iter.zip([2, 4, 6, 8, 15, 22, 29, 36, 43, 50, 64, 78]) {
            assert_eq!(start_date - Duration::days(i), date)
        }
    }

    #[test]
    fn test_validate_dir() {
        let current_dir = ".";
        assert!(validate_dir(current_dir).is_ok());
        let main = "src/main.rs";
        assert!(
            validate_dir(main).is_err(),
            "{}",
            validate_dir(main).unwrap_err()
        )
    }

    #[test]
    fn test_validate_file() {
        let current_dir = ".";
        assert!(validate_file(current_dir).is_err());
        let main = "src/main.rs";
        assert!(
            validate_file(main).is_ok(),
            "{}",
            validate_dir(main).unwrap_err()
        )
    }
}
