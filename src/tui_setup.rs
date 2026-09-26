use super::{RemoteArgs, Screen};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use jsipack_updater::managed::Side;
use jsipack_updater::source::GitHub;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use std::path::PathBuf;
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

#[path = "tui_paths.rs"]
mod paths;
use paths::{is_instance, selected_path, suggestion_worker};

#[path = "tui_browser.rs"]
mod browser;
use browser::BrowserService;

fn tab_completion(
    selected: Option<&PathBuf>,
    suggestion: Option<&PathBuf>,
    input_is_directory: bool,
) -> Option<PathBuf> {
    if selected.is_some() || input_is_directory {
        None
    } else {
        suggestion.cloned()
    }
}

struct PendingValidation {
    receiver: mpsc::Receiver<std::result::Result<(), String>>,
    path: PathBuf,
    target: String,
}

pub(super) fn configure(screen: &mut Screen, args: &mut RemoteArgs) -> Result<bool> {
    let mut chosen_instance =
        (!args.instance.as_os_str().is_empty()).then(|| args.instance.clone());
    let mut instance = args.instance.to_string_lossy().into_owned();
    let mut target = args.target.clone();
    let mut target_from_list = false;
    let mut field = if instance.is_empty() { 0 } else { 1 };
    let mut suggestion_index = 0usize;
    let mut release_index = 0usize;
    let mut releases = Vec::new();
    let mut status = if target.is_empty() {
        "正在获取最近发布版本…".to_owned()
    } else {
        String::new()
    };
    let mut validation: Option<PendingValidation> = None;
    let mut release_rx = if target.is_empty() {
        let (sender, receiver) = mpsc::channel();
        let repo = args.repo.clone();
        let token = args.token.clone();
        std::thread::spawn(move || {
            let result = GitHub::new(&repo, token)
                .and_then(|github| github.release_tags())
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(result);
        });
        Some(receiver)
    } else {
        None
    };
    let (suggest_tx, suggest_rx) = suggestion_worker();
    let mut browser = BrowserService::new();
    let _ = suggest_tx.send(instance.clone());
    let mut suggestions = Vec::new();
    let mut input_is_directory = false;
    loop {
        if let Some(pending) = validation.take() {
            match pending.receiver.try_recv() {
                Ok(Ok(())) => {
                    args.instance = pending.path;
                    args.target = pending.target;
                    return Ok(true);
                }
                Ok(Err(message)) => {
                    status = message;
                    field = 0;
                }
                Err(TryRecvError::Empty) => {
                    validation = Some(pending);
                }
                Err(TryRecvError::Disconnected) => {
                    status = "实例目录检查中断，请重试".into();
                }
            }
        }
        while let Ok((shown, result)) = suggest_rx.try_recv() {
            if shown == instance {
                suggestions = result.matches;
                input_is_directory = result.input_is_directory;
                suggestion_index = suggestion_index.min(suggestions.len().saturating_sub(1));
            }
        }
        if let Some(receiver) = release_rx.take() {
            match receiver.try_recv() {
                Ok(Ok(tags)) => {
                    releases = tags;
                    if target.is_empty()
                        && let Some(first) = releases.first()
                    {
                        target = first.clone();
                        target_from_list = true;
                    }
                    status = if releases.is_empty() {
                        "仓库没有可选发布版本；可手动输入 tag 或 commit".into()
                    } else {
                        "可用 ↑↓ 切换发布版本，也可直接输入 tag 或 commit".into()
                    };
                }
                Ok(Err(error)) => status = format!("获取发布版本失败：{error}；可手动输入"),
                Err(TryRecvError::Disconnected) => {
                    status = "发布版本请求中断；可手动输入 tag 或 commit".into();
                }
                Err(TryRecvError::Empty) => release_rx = Some(receiver),
            }
        }
        suggestion_index = suggestion_index.min(suggestions.len().saturating_sub(1));
        screen.terminal.draw(|frame| {
            let areas = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(5),
                    Constraint::Min(3),
                    Constraint::Length(3),
                    Constraint::Length(3),
                ])
                .split(frame.area());
            frame.render_widget(
                Paragraph::new("JSIPackUpdater · 选择实例和版本")
                    .block(Block::default().borders(Borders::ALL)),
                areas[0],
            );
            let side = match args.side {
                Side::Client => "客户端",
                Side::Server => "服务端",
            };
            let fields = [
                format!("实例目录：{instance}"),
                format!("目标版本：{target}"),
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
            let visible = areas[2].height.saturating_sub(2) as usize;
            let candidates: Vec<_> = if field == 0 {
                let offset = suggestion_index.saturating_sub(visible.saturating_sub(1));
                suggestions
                    .iter()
                    .enumerate()
                    .skip(offset)
                    .take(visible)
                    .map(|(index, entry)| {
                        format!(
                            "{} {}{}",
                            if index == suggestion_index { ">" } else { " " },
                            entry.path.to_string_lossy(),
                            if entry.is_instance { " ✓" } else { "" }
                        )
                    })
                    .collect()
            } else if field == 1 {
                let offset = release_index.saturating_sub(visible.saturating_sub(1));
                releases
                    .iter()
                    .enumerate()
                    .skip(offset)
                    .take(visible)
                    .map(|(index, tag)| {
                        format!("{} {tag}", if index == release_index { ">" } else { " " })
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let title = if field == 0 {
                "目录建议（Tab 补全，F2 浏览）"
            } else if field == 1 {
                "最近发布版本（↑↓ 选择）"
            } else {
                "侧别"
            };
            frame.render_widget(
                List::new(candidates.into_iter().map(ListItem::new))
                    .block(Block::default().title(title).borders(Borders::ALL)),
                areas[2],
            );
            frame.render_widget(
                Paragraph::new(status.as_str()).block(Block::default().borders(Borders::ALL)),
                areas[3],
            );
            frame.render_widget(
                Paragraph::new(
                    "Tab 切换/补全 · F2 浏览目录 · ↑↓ 选择建议/版本 · Enter 开始更新 · Esc 退出",
                )
                .block(Block::default().borders(Borders::ALL)),
                areas[4],
            );
        })?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            if validation.is_some()
                && key.code != KeyCode::Esc
                && !(key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                continue;
            }
            let previous_instance = instance.clone();
            match key.code {
                KeyCode::Esc => return Ok(false),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(false);
                }
                KeyCode::F(2) if field == 0 => {
                    if let Some(path) = browser.browse_instance(
                        screen,
                        instance.clone(),
                        chosen_instance.clone(),
                    )? {
                        instance = path.to_string_lossy().into_owned();
                        chosen_instance = Some(path);
                        field = 1;
                    }
                }
                KeyCode::Tab => {
                    if field == 0
                        && let Some(path) = tab_completion(
                            chosen_instance.as_ref(),
                            suggestions.get(suggestion_index).map(|entry| &entry.path),
                            input_is_directory,
                        )
                    {
                        instance = path.to_string_lossy().into_owned();
                        chosen_instance = Some(path);
                    }
                    field = (field + 1) % 3;
                }
                KeyCode::BackTab => field = (field + 2) % 3,
                KeyCode::Up | KeyCode::Down if field == 0 && !suggestions.is_empty() => {
                    suggestion_index = if key.code == KeyCode::Down {
                        (suggestion_index + 1) % suggestions.len()
                    } else {
                        (suggestion_index + suggestions.len() - 1) % suggestions.len()
                    };
                }
                KeyCode::Up | KeyCode::Down if field == 1 && !releases.is_empty() => {
                    release_index = if key.code == KeyCode::Down {
                        (release_index + 1) % releases.len()
                    } else {
                        (release_index + releases.len() - 1) % releases.len()
                    };
                    target = releases[release_index].clone();
                    target_from_list = true;
                }
                KeyCode::Up | KeyCode::Down => {
                    field = if key.code == KeyCode::Down {
                        (field + 1) % 3
                    } else {
                        (field + 2) % 3
                    };
                }
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
                    chosen_instance = None;
                    suggestion_index = 0;
                }
                KeyCode::Backspace if field == 1 => {
                    if target_from_list {
                        target.clear();
                        target_from_list = false;
                    } else {
                        target.pop();
                    }
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    match field {
                        0 => {
                            instance.clear();
                            chosen_instance = None;
                            suggestion_index = 0;
                        }
                        1 => {
                            target.clear();
                            target_from_list = false;
                        }
                        _ => {}
                    }
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    match field {
                        0 => {
                            instance.push(ch);
                            chosen_instance = None;
                            suggestion_index = 0;
                        }
                        1 => {
                            if target_from_list {
                                target.clear();
                                target_from_list = false;
                            }
                            target.push(ch);
                        }
                        _ => {}
                    }
                }
                KeyCode::Enter => {
                    let path = selected_path(chosen_instance.as_ref(), &instance);
                    if path.as_os_str().is_empty() {
                        status = "请选择实例目录；按 F2 可浏览文件夹".into();
                        field = 0;
                    } else if target.trim().is_empty() {
                        status = "请选择发布版本或输入目标 tag / commit".into();
                        field = 1;
                    } else {
                        let (sender, receiver) = mpsc::channel();
                        let check_path = path.clone();
                        std::thread::spawn(move || {
                            let result = if !check_path.is_dir() {
                                Err("实例目录不存在；按 Tab 补全或 F2 浏览".into())
                            } else if !is_instance(&check_path) {
                                Err("目录缺少 config/createdelight_release_info.json，请选择 minecraft 实例目录".into())
                            } else {
                                Ok(())
                            };
                            let _ = sender.send(result);
                        });
                        validation = Some(PendingValidation {
                            receiver,
                            path,
                            target: target.trim().to_owned(),
                        });
                        status = "正在检查实例目录…".into();
                    }
                }
                _ => {}
            }
            if instance != previous_instance {
                suggestions.clear();
                input_is_directory = false;
                suggestion_index = 0;
                let _ = suggest_tx.send(instance.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tab_completion;
    use std::path::PathBuf;

    #[test]
    fn tab_keeps_existing_directory_when_advancing_field() {
        let child = PathBuf::from("nested");
        assert_eq!(tab_completion(None, Some(&child), true), None);
        assert_eq!(tab_completion(None, Some(&child), false), Some(child));
        let selected = PathBuf::from("original");
        assert_eq!(tab_completion(Some(&selected), None, false), None);
    }
}
