use super::{RemoteArgs, run_remote_update};
use anyhow::{Result, anyhow};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use jsipack_updater::managed::Side;
use jsipack_updater::transaction;
use jsipack_updater::updater::{ConflictResolution, UnresolvedConflicts};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
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

fn instance_from_setup(original: &std::path::Path, displayed: &str, edited: bool) -> PathBuf {
    if edited {
        PathBuf::from(displayed)
    } else {
        original.to_path_buf()
    }
}

fn configure(screen: &mut Screen, args: &mut RemoteArgs) -> Result<bool> {
    let original_instance = args.instance.clone();
    let mut instance = args.instance.to_string_lossy().into_owned();
    let mut instance_edited = false;
    let mut target = args.target.clone();
    let mut field = if instance.is_empty() { 0 } else { 1 };
    let mut error = String::new();
    loop {
        screen.terminal.draw(|frame| {
            let areas = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(5),
                    Constraint::Length(3),
                    Constraint::Min(1),
                ])
                .split(frame.area());
            frame.render_widget(
                Paragraph::new("JSIPackUpdater · 设置更新")
                    .block(Block::default().borders(Borders::ALL)),
                areas[0],
            );
            let side = match args.side {
                Side::Client => "客户端",
                Side::Server => "服务端",
            };
            let fields = [
                format!("实例目录：{instance}"),
                format!("目标 tag / commit：{target}"),
                format!("实例侧别：{side}（←/→ 切换）"),
            ];
            let items: Vec<_> = fields
                .into_iter()
                .enumerate()
                .map(|(index, value)| {
                    let style = if index == field {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default()
                    };
                    ListItem::new(format!(
                        "{} {value}",
                        if index == field { ">" } else { " " }
                    ))
                    .style(style)
                })
                .collect();
            frame.render_widget(
                List::new(items).block(Block::default().borders(Borders::ALL)),
                areas[1],
            );
            frame.render_widget(
                Paragraph::new(error.as_str()).block(Block::default().borders(Borders::ALL)),
                areas[2],
            );
            frame.render_widget(
                Paragraph::new("Tab/↑/↓ 切换字段 · Enter 开始更新 · Esc 退出")
                    .block(Block::default().borders(Borders::ALL)),
                areas[3],
            );
        })?;
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            error.clear();
            match key.code {
                KeyCode::Esc => return Ok(false),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(false);
                }
                KeyCode::Tab | KeyCode::Down => field = (field + 1) % 3,
                KeyCode::BackTab | KeyCode::Up => field = (field + 2) % 3,
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if field == 2 => {
                    args.side = match args.side {
                        Side::Client => Side::Server,
                        Side::Server => Side::Client,
                    };
                }
                KeyCode::Char('c') if field == 2 && key.modifiers.is_empty() => {
                    args.side = Side::Client;
                }
                KeyCode::Char('s') if field == 2 && key.modifiers.is_empty() => {
                    args.side = Side::Server;
                }
                KeyCode::Backspace if field == 0 => {
                    instance.pop();
                    instance_edited = true;
                }
                KeyCode::Backspace if field == 1 => {
                    target.pop();
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    match field {
                        0 => {
                            instance.clear();
                            instance_edited = true;
                        }
                        1 => target.clear(),
                        _ => {}
                    }
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    match field {
                        0 => {
                            instance.push(ch);
                            instance_edited = true;
                        }
                        1 => target.push(ch),
                        _ => {}
                    }
                }
                KeyCode::Enter => {
                    let instance_path =
                        instance_from_setup(&original_instance, &instance, instance_edited);
                    if instance_path.as_os_str().is_empty() {
                        error = "请选择实例目录".into();
                        field = 0;
                    } else if !instance_path.is_dir() {
                        error = "实例目录不存在".into();
                        field = 0;
                    } else if target.trim().is_empty() {
                        error = "请输入目标 tag 或 commit".into();
                        field = 1;
                    } else {
                        args.instance = instance_path;
                        args.target = target.trim().to_owned();
                        return Ok(true);
                    }
                }
                _ => {}
            }
        }
    }
}

pub fn run(mut args: RemoteArgs) -> Result<()> {
    let mut screen = Screen::open()?;
    if (args.instance.as_os_str().is_empty() || args.target.is_empty())
        && !configure(&mut screen, &mut args)?
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
                    Paragraph::new("JSIPackUpdater · Minecraft 整合包更新")
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

#[cfg(all(test, unix))]
mod setup_tests {
    use super::instance_from_setup;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    #[test]
    fn untouched_instance_path_keeps_non_unicode_bytes_and_trailing_spaces() {
        let original = PathBuf::from(OsString::from_vec(b"/tmp/instance\xff ".to_vec()));
        let shown = original.to_string_lossy();
        assert_eq!(instance_from_setup(&original, &shown, false), original);
        assert_ne!(instance_from_setup(&original, &shown, true), original);
    }
}
