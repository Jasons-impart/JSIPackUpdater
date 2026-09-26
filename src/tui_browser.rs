use super::Screen;
use super::paths::{BrowserDirectory, browser_start, child_directories, home_dir};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

type ScanResult = std::io::Result<(bool, Vec<BrowserDirectory>)>;

struct CancelOnDrop(Arc<AtomicU64>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

pub(super) struct BrowserService {
    generation: Arc<AtomicU64>,
    resolve_tx: mpsc::Sender<(u64, String, Option<PathBuf>)>,
    resolved_rx: mpsc::Receiver<(u64, PathBuf)>,
    scan_tx: mpsc::Sender<(u64, PathBuf)>,
    scan_rx: mpsc::Receiver<(u64, PathBuf, ScanResult)>,
}

impl BrowserService {
    pub(super) fn new() -> Self {
        let generation = Arc::new(AtomicU64::new(0));
        let (resolve_tx, resolve_rx) = mpsc::channel::<(u64, String, Option<PathBuf>)>();
        let (resolved_tx, resolved_rx) = mpsc::channel::<(u64, PathBuf)>();
        let resolver_generation = Arc::clone(&generation);
        std::thread::spawn(move || {
            while let Ok((mut id, mut shown, mut selected)) = resolve_rx.recv() {
                while let Ok((new_id, new_shown, new_selected)) = resolve_rx.try_recv() {
                    (id, shown, selected) = (new_id, new_shown, new_selected);
                }
                if resolver_generation.load(Ordering::Acquire) != id {
                    continue;
                }
                let path = browser_start(&shown, selected.as_ref());
                if resolver_generation.load(Ordering::Acquire) == id
                    && resolved_tx.send((id, path)).is_err()
                {
                    break;
                }
            }
        });
        let (scan_tx, scan_requests) = mpsc::channel::<(u64, PathBuf)>();
        let (scan_results, scan_rx) = mpsc::channel::<(u64, PathBuf, ScanResult)>();
        let scanner_generation = Arc::clone(&generation);
        std::thread::spawn(move || {
            while let Ok((mut id, mut path)) = scan_requests.recv() {
                while let Ok((new_id, new_path)) = scan_requests.try_recv() {
                    (id, path) = (new_id, new_path);
                }
                if scanner_generation.load(Ordering::Acquire) != id {
                    continue;
                }
                let result = child_directories(&path, &scanner_generation, id);
                if scanner_generation.load(Ordering::Acquire) == id
                    && scan_results.send((id, path, result)).is_err()
                {
                    break;
                }
            }
        });
        Self {
            generation,
            resolve_tx,
            resolved_rx,
            scan_tx,
            scan_rx,
        }
    }

    fn next_id(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub(super) fn browse_instance(
        &mut self,
        screen: &mut Screen,
        shown: String,
        selected_path: Option<PathBuf>,
    ) -> Result<Option<PathBuf>> {
        let _cancel = CancelOnDrop(Arc::clone(&self.generation));
        let mut request_id = self.next_id();
        self.resolve_tx.send((request_id, shown, selected_path))?;
        let mut current = PathBuf::new();
        let mut selected = 0usize;
        let mut error = String::new();
        let mut children = Vec::new();
        let mut current_instance = false;
        let mut loading = true;
        let mut locating = true;
        loop {
            loop {
                match self.resolved_rx.try_recv() {
                    Ok((id, path)) if id == request_id => {
                        current = path;
                        locating = false;
                        loading = true;
                        children.clear();
                        selected = 0;
                        request_id = self.next_id();
                        self.scan_tx.send((request_id, current.clone()))?;
                    }
                    Ok(_) => continue,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if locating {
                            locating = false;
                            loading = false;
                            error = "目录定位线程已停止，请返回重试".into();
                        }
                        break;
                    }
                }
            }
            loop {
                match self.scan_rx.try_recv() {
                    Ok((id, path, result)) if id == request_id && path == current => {
                        loading = false;
                        match result {
                            Ok((marked, found)) => {
                                children = found;
                                current_instance = marked;
                            }
                            Err(cause) => error = format!("无法读取目录：{cause}"),
                        }
                    }
                    Ok(_) => continue,
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if loading {
                            loading = false;
                            error = "目录读取线程已停止，请返回重试".into();
                        }
                        break;
                    }
                }
            }
            let choices = if locating {
                0
            } else {
                children.len() + usize::from(current.parent().is_some())
            };
            selected = selected.min(choices.saturating_sub(1));
            let shown_path = if locating {
                "正在定位目录…".to_owned()
            } else {
                current.to_string_lossy().into_owned()
            };
            screen.terminal.draw(|frame| {
            let areas = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(3),
                    Constraint::Length(3),
                    Constraint::Length(3),
                ])
                .split(frame.area());
            frame.render_widget(
                Paragraph::new(shown_path.as_str())
                    .block(Block::default().title("选择实例目录").borders(Borders::ALL)),
                areas[0],
            );
            let visible = areas[1].height.saturating_sub(2) as usize;
            let offset = selected.saturating_sub(visible.saturating_sub(1));
            let items: Vec<_> = (offset..choices.min(offset + visible))
                .map(|index| {
                    let name = if current.parent().is_some() && index == 0 {
                        "..".to_owned()
                    } else {
                        let child_index = index - usize::from(current.parent().is_some());
                        let entry = &children[child_index];
                        let label = entry
                            .path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        format!("{} {label}", if entry.is_instance { "✓" } else { " " })
                    };
                    let style = if index == selected {
                        Style::default().fg(Color::Yellow)
                    } else {
                        Style::default()
                    };
                    ListItem::new(format!("{} {name}", if index == selected { ">" } else { " " }))
                        .style(style)
                })
                .collect();
            frame.render_widget(
                List::new(items).block(Block::default().borders(Borders::ALL)),
                areas[1],
            );
            let status = if !error.is_empty() {
                error.clone()
            } else if locating {
                "正在定位目录…".to_owned()
            } else if loading {
                "正在读取目录…".to_owned()
            } else if current_instance {
                "当前目录是可用实例，按 s 选择".to_owned()
            } else {
                String::new()
            };
            frame.render_widget(
                Paragraph::new(status).block(Block::default().borders(Borders::ALL)),
                areas[2],
            );
            frame.render_widget(
                Paragraph::new("↑↓ 选择 · Enter 打开/选中 ✓ · Backspace 上级 · s 选当前目录 · h 主目录 · p 启动器 · Esc 返回")
                    .block(Block::default().borders(Borders::ALL)),
                areas[3],
            );
        })?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                error.clear();
                let previous = current.clone();
                match key.code {
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(None);
                    }
                    KeyCode::Up if choices > 0 => selected = (selected + choices - 1) % choices,
                    KeyCode::Down if choices > 0 => selected = (selected + 1) % choices,
                    KeyCode::Backspace => {
                        if let Some(parent) = current.parent() {
                            current = parent.to_path_buf();
                            selected = 0;
                        }
                    }
                    KeyCode::Char('h') if key.modifiers.is_empty() => {
                        if let Some(home) = home_dir() {
                            current = home;
                            locating = false;
                            selected = 0;
                        }
                    }
                    KeyCode::Char('p') if key.modifiers.is_empty() => {
                        request_id = self.next_id();
                        self.resolve_tx.send((request_id, String::new(), None))?;
                        current = PathBuf::new();
                        locating = true;
                        loading = true;
                        children.clear();
                        current_instance = false;
                        selected = 0;
                    }
                    KeyCode::Char('s') if key.modifiers.is_empty() => {
                        if current_instance {
                            return Ok(Some(current));
                        }
                        error = if loading {
                            "目录仍在读取，请稍候".into()
                        } else {
                            "当前目录缺少版本标记，请进入 minecraft 目录".into()
                        };
                    }
                    KeyCode::Enter if choices > 0 => {
                        if current.parent().is_some() && selected == 0 {
                            current = current.parent().unwrap().to_path_buf();
                        } else {
                            let index = selected - usize::from(current.parent().is_some());
                            let next = &children[index];
                            if next.is_instance {
                                return Ok(Some(next.path.clone()));
                            }
                            current = next.path.clone();
                        }
                        selected = 0;
                    }
                    _ => {}
                }
                if current != previous && !locating {
                    children.clear();
                    current_instance = false;
                    loading = true;
                    selected = 0;
                    request_id = self.next_id();
                    self.scan_tx.send((request_id, current.clone()))?;
                }
            }
        }
    }
}
