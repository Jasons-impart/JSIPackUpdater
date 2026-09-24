use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use jsipack_updater::planner::{Action, Plan, plan_roots_checked};
use jsipack_updater::safe_fs::SafeRoot;
use jsipack_updater::source::{GitHub, SourceLimits};
use jsipack_updater::version::{check_components_roots, load_release_info};
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "jsipack-updater",
    about = "Preview an update for an installed JSIPack instance"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
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

fn main() -> Result<ExitCode> {
    let Cli { command } = Cli::parse();
    match command {
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
                let document =
                    serde_json::json!({"base_commit":base,"target_commit":target,"plan":plan});
                writeln!(output, "{}", serde_json::to_string_pretty(&document)?)?;
            } else {
                writeln!(output, "{}", serde_json::to_string_pretty(plan)?)?;
            }
        }
        Format::Text => {
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
