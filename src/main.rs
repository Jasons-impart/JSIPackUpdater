use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use minecraft_modpack_updater::bkmpw;
use minecraft_modpack_updater::managed::{Side, adopt_installed};
use minecraft_modpack_updater::planner::{Action, Plan, plan_roots_checked};
use minecraft_modpack_updater::safe_fs::SafeRoot;
use minecraft_modpack_updater::source::{GitHub, SourceLimits};
use minecraft_modpack_updater::transaction;
use minecraft_modpack_updater::updater::{self, ConflictPolicy, ConflictResolution};
use minecraft_modpack_updater::version::{check_components_roots, load_release_info};
use std::collections::BTreeMap;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(feature = "gui")]
mod gui;
mod instance_paths;
mod tui;

#[derive(Parser)]
#[command(
    name = "minecraft-modpack-updater",
    version,
    about = "Update an installed Minecraft modpack client or server instance"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show update progress in a terminal window.
    Tui {
        #[arg(long)]
        instance: Option<PathBuf>,
        #[arg(long)]
        target: Option<String>,
        #[arg(long, value_enum, default_value = "client")]
        side: Side,
        #[arg(long)]
        bkmpw: Option<PathBuf>,
        #[arg(long, default_value = "Jasons-impart/Create-Delight-Project-Rebirth")]
        repo: String,
        #[arg(long)]
        token_stdin: bool,
        #[arg(long, value_enum, default_value = "stop")]
        conflicts: ConflictChoice,
        #[arg(long = "resolve", value_name = "PATH=target|local|manual:FILE")]
        resolve: Vec<String>,
    },
    #[cfg(feature = "gui")]
    /// Open the graphical updater window.
    Gui,
    /// Apply a fixed GitHub tag or commit to a stopped instance.
    UpdateRemote {
        #[arg(long)]
        instance: PathBuf,
        #[arg(long)]
        target: String,
        #[arg(long, value_enum)]
        side: Side,
        #[arg(long)]
        bkmpw: Option<PathBuf>,
        #[arg(long, default_value = "Jasons-impart/Create-Delight-Project-Rebirth")]
        repo: String,
        #[arg(long)]
        token_stdin: bool,
        #[arg(long, value_enum, default_value = "stop")]
        conflicts: ConflictChoice,
        #[arg(long = "resolve", value_name = "PATH=target|local|manual:FILE")]
        resolve: Vec<String>,
    },
    /// Apply local full source snapshots for testing or offline use.
    UpdateLocal {
        #[arg(long)]
        base_tree: PathBuf,
        #[arg(long)]
        base_commit: String,
        #[arg(long)]
        target_tree: PathBuf,
        #[arg(long)]
        target_commit: String,
        #[arg(long)]
        instance: PathBuf,
        #[arg(long, value_enum)]
        side: Side,
        #[arg(long)]
        bkmpw: Option<PathBuf>,
        #[arg(long, value_enum, default_value = "stop")]
        conflicts: ConflictChoice,
        #[arg(long = "resolve", value_name = "PATH=target|local|manual:FILE")]
        resolve: Vec<String>,
    },
    /// Restore an interrupted update from its verified backups.
    Rollback {
        #[arg(long)]
        instance: PathBuf,
    },
    /// Preview the first verified bkmpw manifest for an installed client or server.
    PreviewManaged {
        #[arg(long)]
        base_tree: PathBuf,
        /// Full commit SHA represented by the supplied base snapshot.
        #[arg(long)]
        base_commit: String,
        #[arg(long)]
        instance: PathBuf,
        #[arg(long, value_enum)]
        side: Side,
        #[arg(long, value_enum, default_value = "text")]
        format: Format,
    },
    /// Compare complete A/B source snapshots with the installed instance; never writes files.
    Preview {
        #[arg(long)]
        base_tree: PathBuf,
        /// Full commit SHA represented by the supplied base snapshot.
        #[arg(long)]
        base_commit: String,
        #[arg(long)]
        target_tree: PathBuf,
        #[arg(long)]
        instance: PathBuf,
        #[arg(long, value_enum, default_value = "text")]
        format: Format,
    },
    /// Resolve a GitHub tag/commit to a fixed SHA and preview an installed instance.
    PreviewRemote {
        #[arg(long)]
        instance: PathBuf,
        #[arg(long)]
        target: String,
        #[arg(long, default_value = "Jasons-impart/Create-Delight-Project-Rebirth")]
        repo: String,
        /// Read a GitHub token from hidden terminal input or standard input.
        #[arg(long)]
        token_stdin: bool,
        #[arg(long, value_enum, default_value = "text")]
        format: Format,
        #[arg(long, default_value_t = 1024)]
        max_snapshot_mib: u64,
        #[arg(long, default_value_t = 100_000)]
        max_source_files: usize,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Text,
    Json,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ConflictChoice {
    Stop,
    UseTarget,
    KeepLocal,
}

impl From<ConflictChoice> for ConflictPolicy {
    fn from(choice: ConflictChoice) -> Self {
        match choice {
            ConflictChoice::Stop => Self::Stop,
            ConflictChoice::UseTarget => Self::UseTarget,
            ConflictChoice::KeepLocal => Self::KeepLocal,
        }
    }
}

fn parse_resolutions(values: &[String]) -> Result<BTreeMap<String, ConflictResolution>> {
    let mut output = BTreeMap::new();
    for value in values {
        let (path, choice) = value
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("resolution must be PATH=target|local|manual:FILE"))?;
        let resolution = match choice {
            "target" => ConflictResolution::UseTarget,
            "local" => ConflictResolution::KeepLocal,
            manual if manual.starts_with("manual:") && manual.len() > 7 => {
                ConflictResolution::Manual(PathBuf::from(&manual[7..]))
            }
            _ => bail!("invalid resolution for {path}"),
        };
        if output.insert(path.to_owned(), resolution).is_some() {
            bail!("duplicate resolution for {path}");
        }
    }
    Ok(output)
}

fn main() -> Result<ExitCode> {
    #[cfg(feature = "gui")]
    if std::env::args_os().nth(1).is_none() {
        gui::run()?;
        return Ok(ExitCode::SUCCESS);
    }
    let Cli { command } = Cli::parse();
    match command {
        Command::Tui {
            instance,
            target,
            side,
            bkmpw,
            repo,
            token_stdin,
            conflicts,
            resolve,
        } => {
            tui::run(RemoteArgs {
                instance: instance.unwrap_or_default(),
                target: target.unwrap_or_default(),
                side,
                bkmpw,
                repo,
                token: read_token(token_stdin)?,
                conflicts,
                resolutions: parse_resolutions(&resolve)?,
            })?;
        }
        #[cfg(feature = "gui")]
        Command::Gui => gui::run()?,
        Command::Rollback { instance } => {
            transaction::rollback(&instance)?;
            println!("interrupted update rolled back");
        }
        Command::UpdateLocal {
            base_tree,
            base_commit,
            target_tree,
            target_commit,
            instance,
            side,
            bkmpw,
            conflicts,
            resolve,
        } => {
            let installed = load_release_info(&SafeRoot::open(&instance)?)?;
            if !base_commit.eq_ignore_ascii_case(&installed.commit_id) {
                bail!("base snapshot commit does not match installed release marker");
            }
            let _tool_guard = minecraft_modpack_updater::tools::tool_lock(
                minecraft_modpack_updater::tools::Tool::Bkmpw,
                false,
            )?;
            let executable = bkmpw_path(bkmpw)?;
            let report = updater::update(updater::UpdateRequest {
                base_path: &base_tree,
                target_path: &target_tree,
                instance_path: &instance,
                base_commit: &base_commit,
                target_commit: &target_commit,
                side,
                bkmpw_executable: &executable,
                policy: conflicts.into(),
                resolutions: parse_resolutions(&resolve)?,
            })?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::UpdateRemote {
            instance,
            target,
            side,
            bkmpw,
            repo,
            token_stdin,
            conflicts,
            resolve,
        } => {
            let report = run_remote_update(
                RemoteArgs {
                    instance,
                    target,
                    side,
                    bkmpw,
                    repo,
                    token: read_token(token_stdin)?,
                    conflicts,
                    resolutions: parse_resolutions(&resolve)?,
                },
                |_| {},
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::PreviewManaged {
            base_tree,
            base_commit,
            instance,
            side,
            format,
        } => {
            let base_root = SafeRoot::open(&base_tree)?;
            let instance_root = SafeRoot::open(&instance)?;
            let check = check_components_roots(&base_root, &base_root, &instance_root)?;
            if !base_commit.eq_ignore_ascii_case(&check.info.commit_id) {
                bail!("base snapshot commit does not match the installed release marker");
            }
            let adoption = adopt_installed(&base_root, &instance_root, side)?;
            let stdout = io::stdout();
            let mut output = stdout.lock();
            match format {
                Format::Json => writeln!(
                    output,
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"manifest": adoption.manifest, "conflicts": adoption.conflicts})
                    )?
                )?,
                Format::Text => {
                    writeln!(
                        output,
                        "{} verified managed file(s), {} conflict(s)",
                        adoption.manifest.files.len(),
                        adoption.conflicts.len()
                    )?;
                    for conflict in &adoption.conflicts {
                        writeln!(output, "conflict\t{}\t{}", conflict.path, conflict.reason)?;
                    }
                }
            }
            output.flush()?;
            if !adoption.conflicts.is_empty() {
                return Ok(ExitCode::from(2));
            }
        }
        Command::Preview {
            base_tree,
            base_commit,
            target_tree,
            instance,
            format,
        } => {
            let base_root = SafeRoot::open(&base_tree)?;
            let target_root = SafeRoot::open(&target_tree)?;
            let instance_root = SafeRoot::open(&instance)?;
            let info = check_components_roots(&base_root, &target_root, &instance_root)?;
            if !base_commit.eq_ignore_ascii_case(&info.info.commit_id) {
                bail!("base snapshot commit does not match the installed release marker");
            }
            let plan = plan_roots_checked(&base_root, &target_root, &instance_root, &info)?;
            print_plan(&plan, format, None)?;
            if plan.has_conflicts() {
                return Ok(ExitCode::from(2));
            }
        }
        Command::PreviewRemote {
            instance,
            target,
            repo,
            token_stdin,
            format,
            max_snapshot_mib,
            max_source_files,
        } => {
            let instance_root = SafeRoot::open(&instance)?;
            let installed = load_release_info(&instance_root)?;
            let mut github = GitHub::new(&repo, read_token(token_stdin)?)?;
            github.set_limits(SourceLimits {
                max_snapshot_bytes: max_snapshot_mib
                    .checked_mul(1024 * 1024)
                    .ok_or_else(|| anyhow::anyhow!("snapshot size limit is too large"))?,
                max_files: max_source_files,
            })?;
            let base_snapshot = github.fetch_snapshot(&installed.commit_id)?;
            let target_snapshot = github.fetch_snapshot(&target)?;
            let base_root = SafeRoot::open(&base_snapshot.root.path().canonicalize()?)?;
            let target_root = SafeRoot::open(&target_snapshot.root.path().canonicalize()?)?;
            let check = check_components_roots(&base_root, &target_root, &instance_root)?;
            if !check
                .info
                .commit_id
                .eq_ignore_ascii_case(&base_snapshot.commit_id)
            {
                bail!("installed release marker does not match the fetched base commit");
            }
            let plan = plan_roots_checked(&base_root, &target_root, &instance_root, &check)?;
            print_plan(
                &plan,
                format,
                Some((&base_snapshot.commit_id, &target_snapshot.commit_id)),
            )?;
            if plan.has_conflicts() {
                return Ok(ExitCode::from(2));
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Clone)]
struct RemoteArgs {
    instance: PathBuf,
    target: String,
    side: Side,
    bkmpw: Option<PathBuf>,
    repo: String,
    token: Option<String>,
    conflicts: ConflictChoice,
    resolutions: BTreeMap<String, ConflictResolution>,
}

#[cfg(feature = "gui")]
struct PreviewSummary {
    base_commit: String,
    target_commit: String,
    item_count: usize,
    conflicts: Vec<String>,
    mods: Vec<minecraft_modpack_updater::managed::ModChange>,
}

fn run_remote_update(
    args: RemoteArgs,
    mut progress: impl FnMut(String),
) -> Result<updater::UpdateReport> {
    if transaction::pending(&args.instance)? {
        bail!("unfinished update exists; run rollback before another update");
    }
    let instance_root = SafeRoot::open(&args.instance)?;
    let installed = load_release_info(&instance_root)?;
    let _tool_guard = minecraft_modpack_updater::tools::tool_lock(
        minecraft_modpack_updater::tools::Tool::Bkmpw,
        false,
    )?;
    let executable = bkmpw_path(args.bkmpw)?;
    instance_root.ensure_dir(".jsipack-updater/cache")?;
    let cache = args.instance.join(".jsipack-updater/cache");
    let github = GitHub::new(&args.repo, args.token)?;
    progress("Fetching installed commit".into());
    let base = github.fetch_snapshot_cached(&installed.commit_id, &cache)?;
    progress("Fetching target commit".into());
    let destination = github.fetch_snapshot_cached(&args.target, &cache)?;
    updater::update_with_progress(
        updater::UpdateRequest {
            base_path: base.root.path(),
            target_path: destination.root.path(),
            instance_path: &args.instance,
            base_commit: &base.commit_id,
            target_commit: &destination.commit_id,
            side: args.side,
            bkmpw_executable: &executable,
            policy: args.conflicts.into(),
            resolutions: args.resolutions,
        },
        progress,
    )
    .map_err(|error| {
        if let Some(conflicts) = error.downcast_ref::<updater::UnresolvedConflicts>() {
            anyhow::Error::new(updater::UnresolvedConflicts {
                paths: conflicts.paths.clone(),
                target_commit: Some(destination.commit_id.clone()),
            })
            .context(error.to_string())
        } else {
            error
        }
    })
}

#[cfg(feature = "gui")]
fn run_remote_preview(args: &RemoteArgs) -> Result<PreviewSummary> {
    let instance_root = SafeRoot::open(&args.instance)?;
    let installed = load_release_info(&instance_root)?;
    let github = GitHub::new(&args.repo, args.token.clone())?;
    let base = github.fetch_snapshot(&installed.commit_id)?;
    let destination = github.fetch_snapshot(&args.target)?;
    let base_root = SafeRoot::open(base.root.path())?;
    let target_root = SafeRoot::open(destination.root.path())?;
    let check = check_components_roots(&base_root, &target_root, &instance_root)?;
    if !check.info.commit_id.eq_ignore_ascii_case(&base.commit_id) {
        bail!("installed commit changed during preview");
    }
    let plan = plan_roots_checked(&base_root, &target_root, &instance_root, &check)?;
    Ok(PreviewSummary {
        mods: minecraft_modpack_updater::managed::mod_changes(&base_root, &target_root, args.side)?,
        base_commit: base.commit_id,
        target_commit: destination.commit_id,
        item_count: plan.items.len(),
        conflicts: plan
            .items
            .into_iter()
            .filter(|item| item.action == Action::Conflict)
            .map(|item| format!("{} ({})", item.path, item.reason))
            .collect(),
    })
}

fn bkmpw_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    let executable = std::env::current_exe()?;
    let name = if cfg!(windows) { "bkmpw.exe" } else { "bkmpw" };
    let path = executable.with_file_name(name);
    bkmpw::verify_pinned_binary(&path)?;
    Ok(path)
}

fn read_token(from_stdin: bool) -> Result<Option<String>> {
    let token = if from_stdin {
        if io::stdin().is_terminal() {
            rpassword::prompt_password("GitHub token: ")?
        } else {
            read_token_line(io::stdin().lock())?
        }
    } else {
        std::env::var("GITHUB_TOKEN").unwrap_or_default()
    };
    let token = token.trim().to_owned();
    if from_stdin && token.is_empty() {
        bail!("GitHub token is empty");
    }
    Ok((!token.is_empty()).then_some(token))
}

fn read_token_line(mut reader: impl BufRead) -> Result<String> {
    let mut input = Vec::new();
    reader.by_ref().take(4097).read_until(b'\n', &mut input)?;
    if input.len() > 4096 {
        bail!("GitHub token input exceeds 4 KiB");
    }
    Ok(String::from_utf8(input)?)
}

fn print_plan(plan: &Plan, format: Format, commits: Option<(&str, &str)>) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    match format {
        Format::Json => {
            if let Some((base, target)) = commits {
                let document = serde_json::json!({"base_commit":base,"target_commit":target,"scope":"source_files_only","plan":plan});
                writeln!(output, "{}", serde_json::to_string_pretty(&document)?)?;
            } else {
                writeln!(output, "{}", serde_json::to_string_pretty(plan)?)?;
            }
        }
        Format::Text => {
            writeln!(
                output,
                "Source file preview only; managed artifacts and generated files are checked during update preparation."
            )?;
            if let Some((base, target)) = commits {
                writeln!(output, "{base} -> {target}")?;
            }
            for item in &plan.items {
                writeln!(output, "{:?}\t{}\t{}", item.action, item.path, item.reason)?;
            }
            writeln!(
                output,
                "{} item(s), {} conflict(s)",
                plan.items.len(),
                plan.items
                    .iter()
                    .filter(|item| item.action == Action::Conflict)
                    .count()
            )?;
        }
    }
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_reader_stops_at_newline_and_rejects_oversize() {
        let mut reader = io::BufReader::new(&b"secret\nremaining"[..]);
        assert_eq!(read_token_line(&mut reader).unwrap(), "secret\n");
        assert_eq!(read_token_line(&mut reader).unwrap(), "remaining");
        assert!(read_token_line(io::Cursor::new(vec![b'x'; 4097])).is_err());
    }
}
