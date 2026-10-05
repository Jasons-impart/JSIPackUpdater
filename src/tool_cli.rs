use super::*;
use minecraft_modpack_updater::tools::{self, InstallChoice, Tool};

#[derive(Subcommand)]
pub enum ToolCommand {
    /// Manage tool versions and installation choices interactively.
    Manage {
        #[arg(long)]
        instance: PathBuf,
    },
    /// Check both tool versions independently; never installs anything.
    Check {
        #[arg(long)]
        instance: PathBuf,
    },
    /// Install a tool using the explicitly selected source.
    Install {
        tool: Tool,
        #[arg(long)]
        instance: PathBuf,
        #[arg(long, value_enum)]
        method: InstallChoice,
    },
    /// Upgrade through the recorded source. Updater upgrade exits and restarts this process.
    Upgrade {
        tool: Tool,
        #[arg(long)]
        instance: PathBuf,
    },
}

pub fn run(command: ToolCommand) -> Result<()> {
    match command {
        ToolCommand::Manage { instance } => tui::manage_tools(instance)?,
        ToolCommand::Check { instance } => {
            let mut failed = false;
            for tool in [Tool::Bkmpw, Tool::Updater] {
                let installed = match tool {
                    Tool::Bkmpw => tools::discover(&instance),
                    Tool::Updater => tools::current_updater().map(Some),
                };
                match installed {
                    Ok(Some(i)) => println!("{}: {} ({:?})", tool.name(), i.version, i.ownership),
                    Ok(None) => println!("{}: 未安装", tool.name()),
                    Err(error) => {
                        failed = true;
                        println!("{}: 检测失败：{error:#}", tool.name());
                    }
                }
                match tools::latest(tool).and_then(|r| {
                    r.supported()?;
                    Ok(r)
                }) {
                    Ok(r) => println!("  可用版本：{}", r.version),
                    Err(error) => {
                        failed = true;
                        println!("  在线检查失败：{error:#}");
                    }
                }
            }
            if failed {
                bail!("one or more tool checks failed; see the independent results above");
            }
        }
        ToolCommand::Install {
            tool,
            instance,
            method,
        } => {
            if tool == Tool::Updater {
                bail!("use tools upgrade updater to safely update the running updater");
            }
            tools::install(&instance, tool, method, |p| eprintln!("{p}"))?;
        }
        ToolCommand::Upgrade {
            tool: Tool::Bkmpw,
            instance,
        } => {
            let installed = tools::upgrade_bkmpw(&instance, |p| eprintln!("{p}"))?;
            println!("bkmpw {} 已就绪", installed.version);
        }
        ToolCommand::Upgrade {
            tool: Tool::Updater,
            instance,
        } => {
            tools::prepare_self_update(&instance, |p| eprintln!("{p}"))?;
        }
    }
    Ok(())
}

pub fn ensure(
    instance: &std::path::Path,
    explicit: &Option<PathBuf>,
    method: Option<InstallChoice>,
) -> Result<()> {
    if transaction::pending(instance)? {
        bail!("unfinished update exists; rollback before installing tools");
    }
    if explicit.is_some() || tools::discover(instance)?.is_some() {
        return Ok(());
    }
    let method = if let Some(method) = method {
        method
    } else {
        if !io::stdin().is_terminal() {
            bail!(
                "bkmpw is missing; pass --install-bkmpw local|npm, or run minecraft-modpack-updater tools install bkmpw --instance <folder> --method local"
            );
        }
        let npm = tools::npm_available();
        eprintln!(
            "未找到 bkmpw。输入 l 下载到实例目录{}；输入 q 取消。",
            if npm {
                "，输入 n 通过 npm 全局安装"
            } else {
                "（未检测到 npm）"
            }
        );
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        match input.trim() {
            "l" => InstallChoice::Local,
            "n" if npm => InstallChoice::Npm,
            _ => bail!("tool installation cancelled"),
        }
    };
    tools::install(instance, Tool::Bkmpw, method, |p| eprintln!("{p}"))?;
    Ok(())
}
