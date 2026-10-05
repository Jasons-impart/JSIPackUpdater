use super::{RemoteArgs, Screen};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use minecraft_modpack_updater::tools::{self, InstallChoice, Progress, Tool};
use ratatui::{
    layout::{Constraint, Layout},
    widgets::{Block, Borders, Gauge, Paragraph},
};
use std::{
    sync::mpsc::{self, TryRecvError},
    time::{Duration, Instant},
};

enum Message {
    Progress(Progress),
    Complete(Result<bool, String>),
    Checked(String, bool),
    Npm(bool),
    Restart,
}

/// Tool selection uses keys in raw mode; workers never read stdin.
pub(super) fn ensure(screen: &mut Screen, args: &RemoteArgs) -> Result<bool> {
    if args.bkmpw.is_some() {
        return Ok(true);
    }
    run(screen, args.instance.clone(), false)
}

pub(super) fn run(
    screen: &mut Screen,
    instance: std::path::PathBuf,
    management: bool,
) -> Result<bool> {
    let (tx, rx) = mpsc::channel();
    let path = instance.clone();
    let mut worker = Some(std::thread::spawn(move || {
        let result = tools::discover(&path)
            .map(|i| i.is_some())
            .map_err(|e| format!("{e:#}"));
        if !management && result.as_ref().is_ok_and(|ready| *ready) {
            let _ = tx.send(Message::Complete(result));
            return;
        }
        let _ = tx.send(Message::Npm(tools::npm_available()));
        let _ = tx.send(Message::Complete(result));
    }));
    let mut busy = true;
    let mut status = "正在检测已安装的 bkmpw…".to_owned();
    let mut progress = None;
    let mut receiver = rx;
    let mut npm = false;
    let mut installed = false;
    let start = Instant::now();
    let result = (|| -> Result<bool> {
        loop {
            loop {
                match receiver.try_recv() {
                    Ok(Message::Restart) => return Ok(false),
                    Ok(Message::Progress(p)) => {
                        status = p.to_string().chars().take(1500).collect();
                        progress = Some(p);
                    }
                    Ok(Message::Npm(available)) => npm = available,
                    Ok(Message::Checked(text, ready)) => {
                        installed = ready;
                        if ready && !management {
                            return Ok(true);
                        }
                        busy = false;
                        progress = None;
                        status = text;
                        break;
                    }
                    Ok(Message::Complete(result)) => {
                        busy = false;
                        progress = None;
                        match result {
                            Ok(true) => {
                                installed = true;
                                if !management {
                                    return Ok(true);
                                }
                                status = "bkmpw 已就绪，可检查或升级工具".into();
                            }
                            Ok(false) => status = "未找到 bkmpw，请选择安装方式".into(),
                            Err(error) => status = format!("工具操作失败，可重试：{error}"),
                        }
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if busy {
                            status = "工具任务异常终止，可重试".into();
                            busy = false;
                        }
                        break;
                    }
                }
            }
            screen.terminal.draw(|frame| {
            let areas = Layout::vertical([Constraint::Min(5), Constraint::Length(3)]).split(frame.area());
            frame.render_widget(Paragraph::new(format!("{status}\n\n{}\n\nc：检查两个工具版本   b：升级 bkmpw   u：升级更新器并重启\nEnter：继续   q：返回", if npm { "l：下载到实例目录   n：npm 全局安装 bkmpw" } else { "l：下载到实例目录（未检测到 npm）" })).block(Block::default().title("工具管理").borders(Borders::ALL)), areas[0]);
            if let Some(fraction) = progress.as_ref().and_then(Progress::fraction) {
                frame.render_widget(Gauge::default().block(Block::default().borders(Borders::ALL)).ratio(fraction as f64), areas[1]);
            } else { frame.render_widget(Paragraph::new(if busy { format!("{} 处理中…", ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"][(start.elapsed().as_millis() / 100 % 8) as usize]) } else { "请选择操作".into() }).block(Block::default().borders(Borders::ALL)), areas[1]); }
        })?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && !busy
            {
                let action = match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(false),
                    KeyCode::Enter if installed => return Ok(true),
                    KeyCode::Char('l') => Action::Install(InstallChoice::Local),
                    KeyCode::Char('n') if npm => Action::Install(InstallChoice::Npm),
                    KeyCode::Char('c') => Action::Check,
                    KeyCode::Char('b') if installed => Action::Upgrade(Tool::Bkmpw),
                    KeyCode::Char('u') => Action::Upgrade(Tool::Updater),
                    _ => continue,
                };
                let (tx, rx) = mpsc::channel();
                receiver = rx;
                busy = true;
                let instance = instance.clone();
                if let Some(previous) = worker.take() {
                    let _ = previous.join();
                }
                worker = Some(std::thread::spawn(move || {
                    let progress = |p| {
                        let _ = tx.send(Message::Progress(p));
                    };
                    let result = match action {
                        Action::Install(method) => {
                            tools::install(&instance, Tool::Bkmpw, method, progress).map(|_| true)
                        }
                        Action::Upgrade(Tool::Bkmpw) => {
                            tools::upgrade_bkmpw(&instance, progress).map(|_| true)
                        }
                        Action::Upgrade(Tool::Updater) => {
                            match tools::prepare_self_update(&instance, progress) {
                                Ok(true) => {
                                    let _ = tx.send(Message::Restart);
                                    return;
                                }
                                Ok(false) => Ok(installed),
                                Err(error) => Err(error),
                            }
                        }
                        Action::Check => {
                            let mut statuses = Vec::new();
                            let mut ready = false;
                            for tool in [Tool::Bkmpw, Tool::Updater] {
                                let current = match tool {
                                    Tool::Bkmpw => tools::discover(&instance).map(|i| {
                                        ready = i.is_some();
                                        i.map(|i| i.version).unwrap_or("未安装".into())
                                    }),
                                    Tool::Updater => tools::current_updater().map(|i| i.version),
                                };
                                statuses.push(format!(
                                    "{} 当前：{}",
                                    tool.name(),
                                    current.unwrap_or_else(|e| format!("{e:#}"))
                                ));
                                statuses.push(
                                    match tools::latest(tool).and_then(|r| {
                                        r.supported()?;
                                        Ok(r)
                                    }) {
                                        Ok(r) => format!("最新：{}", r.version),
                                        Err(e) => format!("在线检查失败：{e:#}"),
                                    },
                                );
                            }
                            let _ = tx.send(Message::Checked(statuses.join("\n"), ready));
                            return;
                        }
                    }
                    .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Message::Complete(result));
                }));
            }
        }
    })();
    if result.is_err() {
        screen.restore();
    }
    if let Some(worker) = worker {
        let _ = worker.join();
    }
    result
}

enum Action {
    Check,
    Install(InstallChoice),
    Upgrade(Tool),
}
