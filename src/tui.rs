use super::{RemoteArgs, run_remote_update};
use anyhow::{Result, anyhow};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use minecraft_modpack_updater::transaction;
use minecraft_modpack_updater::updater::{ConflictResolution, UnresolvedConflicts};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

enum Message {
    Progress(String),
    Complete(Result<String, String>),
    NeedChoices(Vec<String>, String),
}

#[path = "tui_setup.rs"]
mod setup;

fn spawn_update(args: RemoteArgs) -> (Receiver<Message>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let progress = sender.clone();
        let result = run_remote_update(args, move |message| {
            let _ = progress.send(Message::Progress(message));
        });
        match result {
            Ok(report) => {
                let summary =
                    serde_json::to_string_pretty(&report).map_err(|error| error.to_string());
                let _ = sender.send(Message::Complete(summary));
            }
            Err(error) => {
                if let Some(conflicts) = error.downcast_ref::<UnresolvedConflicts>() {
                    if let Some(target_commit) = &conflicts.target_commit {
                        let _ = sender.send(Message::NeedChoices(
                            conflicts.paths.clone(),
                            target_commit.clone(),
                        ));
                    } else {
                        let _ = sender.send(Message::Complete(Err(format!("{error:#}"))));
                    }
                } else {
                    let _ = sender.send(Message::Complete(Err(format!("{error:#}"))));
                }
            }
        }
    });
    (receiver, worker)
}

struct Screen {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Screen {
    fn open() -> Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = crossterm::execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        let terminal = match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = disable_raw_mode();
                let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
                return Err(error.into());
            }
        };
        Ok(Self { terminal })
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub fn run(mut args: RemoteArgs) -> Result<()> {
    let mut screen = Screen::open()?;
    if (args.instance.as_os_str().is_empty() || args.target.is_empty())
        && !setup::configure(&mut screen, &mut args)?
    {
        return Ok(());
    }
    if transaction::pending(&args.instance)? {
        loop {
            screen.terminal.draw(|frame| {
                frame.render_widget(
                    Paragraph::new("检测到未完成更新。按 r 从备份回退，按 q 退出。")
                        .block(Block::default().borders(Borders::ALL)),
                    frame.area(),
                );
            })?;
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                match key.code {
                    KeyCode::Char('r') => {
                        transaction::rollback(&args.instance)?;
                        break;
                    }
                    KeyCode::Char('q') => return Ok(()),
                    _ => {}
                }
            }
        }
    }
    let (mut receiver, initial_worker) = spawn_update(args.clone());
    let mut worker = Some(initial_worker);
    let mut lines = VecDeque::from(["正在准备更新…".to_owned()]);
    let mut done = false;
    let mut failure = None;
    let mut final_summary = None;
    let mut pending = VecDeque::<String>::new();
    let mut manual_input: Option<String> = None;
    let screen_result = (|| -> Result<()> {
        loop {
            loop {
                let message = match receiver.try_recv() {
                    Ok(message) => message,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if !done && pending.is_empty() {
                            done = true;
                            failure =
                                Some("update worker stopped before reporting a result".into());
                            lines.push_back("更新进程异常终止".into());
                        }
                        break;
                    }
                };
                match message {
                    Message::Progress(line) => {
                        lines.push_back(line);
                        while lines.len() > 100 {
                            lines.pop_front();
                        }
                    }
                    Message::NeedChoices(paths, target_commit) => {
                        args.target = target_commit;
                        pending.extend(paths);
                        if let Some(path) = pending.front() {
                            lines.push_back(format!("需要处理冲突：{path}"));
                        } else {
                            done = true;
                            failure = Some("updater reported an empty conflict list".into());
                        }
                    }
                    Message::Complete(result) => {
                        done = true;
                        lines.push_back(match result {
                            Ok(summary) => {
                                final_summary = Some(summary.clone());
                                format!("更新完成\n{summary}")
                            }
                            Err(error) => {
                                failure = Some(error.clone());
                                format!("更新失败：{error}")
                            }
                        });
                    }
                }
            }
            screen.terminal.draw(|frame| {
                let areas = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(3),
                        Constraint::Min(1),
                        Constraint::Length(3),
                    ])
                    .split(frame.area());
                frame.render_widget(
                    Paragraph::new("Minecraft Modpack Updater · Minecraft 整合包更新")
                        .block(Block::default().borders(Borders::ALL)),
                    areas[0],
                );
                let visible = (areas[1].height as usize).saturating_sub(2);
                let items: Vec<_> = lines
                    .iter()
                    .rev()
                    .take(visible)
                    .rev()
                    .map(|line| ListItem::new(line.as_str()))
                    .collect();
                frame.render_widget(
                    List::new(items).block(Block::default().title("进度").borders(Borders::ALL)),
                    areas[1],
                );
                let footer = if let Some(input) = &manual_input {
                    format!("手工合并文件路径：{input}（Enter 确认，Esc 返回）")
                } else if let Some(path) = pending.front() {
                    format!("冲突：{path}｜t 新版，l 本地，m 手工合并，q 退出")
                } else if done && failure.is_some() {
                    "更新失败；按 q 退出并查看完整错误".to_owned()
                } else if done {
                    "更新完成；按 q 退出并查看完整结果".to_owned()
                } else {
                    "更新运行中；完成后可退出".to_owned()
                };
                frame.render_widget(
                    Paragraph::new(footer).block(Block::default().borders(Borders::ALL)),
                    areas[2],
                );
            })?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                if let Some(path) = pending.front().cloned() {
                    if let Some(input) = manual_input.as_mut() {
                        match key.code {
                            KeyCode::Char(ch) => input.push(ch),
                            KeyCode::Backspace => {
                                input.pop();
                            }
                            KeyCode::Esc => manual_input = None,
                            KeyCode::Enter if !input.trim().is_empty() => {
                                args.resolutions.insert(
                                    path,
                                    ConflictResolution::Manual(PathBuf::from(input.trim())),
                                );
                                pending.pop_front();
                                manual_input = None;
                            }
                            _ => {}
                        }
                    } else {
                        match key.code {
                            KeyCode::Char('t') => {
                                args.resolutions.insert(path, ConflictResolution::UseTarget);
                                pending.pop_front();
                            }
                            KeyCode::Char('l') => {
                                args.resolutions.insert(path, ConflictResolution::KeepLocal);
                                pending.pop_front();
                            }
                            KeyCode::Char('m') => manual_input = Some(String::new()),
                            KeyCode::Char('q') => {
                                pending.clear();
                                failure = Some("update cancelled with unresolved conflicts".into());
                                done = true;
                                break;
                            }
                            _ => {}
                        }
                    }
                    if pending.is_empty() && !done {
                        if let Some(previous) = worker.take() {
                            previous
                                .join()
                                .map_err(|_| anyhow!("update worker panicked"))?;
                        }
                        lines.push_back("已记录冲突选择，重新准备更新…".into());
                        (receiver, worker) = {
                            let (receiver, worker) = spawn_update(args.clone());
                            (receiver, Some(worker))
                        };
                    }
                } else if done && key.code == KeyCode::Char('q') {
                    break;
                }
            }
        }
        Ok(())
    })();
    drop(screen);
    let worker_result = worker
        .take()
        .map(|worker| worker.join().map_err(|_| anyhow!("update worker panicked")));
    screen_result?;
    if let Some(result) = worker_result {
        result?;
    }
    if let Some(error) = failure {
        return Err(anyhow!(error));
    }
    if let Some(summary) = final_summary {
        println!("更新完成\n{summary}");
    }
    Ok(())
}
