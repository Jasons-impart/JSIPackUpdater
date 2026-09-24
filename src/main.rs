use anyhow::Result;
use anyhow::bail;
use clap::{Parser, Subcommand, ValueEnum};
use jsipack_updater::planner::{Action, plan_roots_checked};
use jsipack_updater::safe_fs::SafeRoot;
use jsipack_updater::version::check_components_roots;
use std::io::{self, Write};
use std::path::PathBuf;

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
}

#[derive(Clone, ValueEnum)]
enum Format {
    Text,
    Json,
}

fn main() -> Result<()> {
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
            let stdout = io::stdout();
            let mut output = stdout.lock();
            match format {
                Format::Json => writeln!(output, "{}", serde_json::to_string_pretty(&plan)?)?,
                Format::Text => {
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
            if plan.has_conflicts() {
                std::process::exit(2);
            }
        }
    }
    Ok(())
}
