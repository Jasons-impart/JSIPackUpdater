use super::{ConflictChoice, RemoteArgs, parse_resolutions, run_remote_preview, run_remote_update};
use anyhow::Result;
use eframe::egui;
use jsipack_updater::managed::Side;
use jsipack_updater::transaction;
use jsipack_updater::updater::UnresolvedConflicts;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

enum Message {
    Progress(String),
    Complete(Result<String, String>),
    Conflicts(String, String, String, String),
    WorkerStopped,
}

struct GuiApp {
    instance: String,
    selected_instance: Option<PathBuf>,
    target: String,
    repo: String,
    token: String,
    bkmpw: String,
    selected_bkmpw: Option<PathBuf>,
    side: Side,
    conflicts: ConflictChoice,
    resolutions: String,
    messages: VecDeque<String>,
    receiver: Option<Receiver<Message>>,
    worker: Option<JoinHandle<()>>,
    running: bool,
    pinned_target: Option<(String, String, String)>,
}

impl Default for GuiApp {
    fn default() -> Self {
        Self {
            instance: String::new(),
            selected_instance: None,
            target: String::new(),
            repo: "Jasons-impart/Create-Delight-Project-Rebirth".into(),
            token: String::new(),
            bkmpw: String::new(),
            selected_bkmpw: None,
            side: Side::Client,
            conflicts: ConflictChoice::Stop,
            resolutions: String::new(),
            messages: VecDeque::new(),
            receiver: None,
            worker: None,
            running: false,
            pinned_target: None,
        }
    }
}

impl GuiApp {
    fn instance_path(&self) -> Option<PathBuf> {
        self.selected_instance
            .clone()
            .or_else(|| (!self.instance.is_empty()).then(|| PathBuf::from(self.instance.as_str())))
    }

    fn bkmpw_path(&self) -> Option<PathBuf> {
        self.selected_bkmpw
            .clone()
            .or_else(|| (!self.bkmpw.is_empty()).then(|| PathBuf::from(self.bkmpw.as_str())))
    }

    fn append(&mut self, message: String) {
        self.messages.push_back(message);
        while self.messages.len() > 100 {
            self.messages.pop_front();
        }
    }

    fn start(&mut self, preview: bool, ctx: egui::Context) {
        let Some(instance) = self.instance_path() else {
            self.append("请选择实例目录并填写目标 tag 或 commit".into());
            return;
        };
        if self.target.trim().is_empty() {
            self.append("请选择实例目录并填写目标 tag 或 commit".into());
            return;
        }
        let token = if self.token.trim().is_empty() {
            std::env::var("GITHUB_TOKEN")
                .ok()
                .filter(|value| !value.is_empty())
        } else {
            Some(self.token.trim().to_owned())
        };
        let values: Vec<_> = self
            .resolutions
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        let resolutions = match parse_resolutions(&values) {
            Ok(value) => value,
            Err(error) => {
                self.append(format!("冲突选择无效：{error}"));
                return;
            }
        };
        let requested = self.target.trim().to_owned();
        let repo = self.repo.trim().to_owned();
        let requested_for_worker = requested.clone();
        let repo_for_worker = repo.clone();
        if preview {
            self.pinned_target = None;
        }
        let target = self
            .pinned_target
            .as_ref()
            .filter(|(saved_repo, saved_target, _)| {
                saved_repo == &repo && saved_target == &requested
            })
            .map(|(_, _, commit)| commit.clone())
            .unwrap_or(requested);
        let args = RemoteArgs {
            instance,
            target,
            side: self.side,
            bkmpw: self.bkmpw_path(),
            repo,
            token,
            conflicts: self.conflicts,
            resolutions,
        };
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.running = true;
        self.messages.clear();
        self.worker = Some(std::thread::spawn(move || {
            let result = if preview {
                run_remote_preview(&args).map(|summary| {
                    format!(
                        "源码文件预览（模组与生成文件冲突在更新准备时检查）\n{} → {}\n{} 项，{} 个冲突{}",
                        summary.base_commit,
                        summary.target_commit,
                        summary.item_count,
                        summary.conflicts.len(),
                        if summary.conflicts.is_empty() {
                            String::new()
                        } else {
                            format!("\n{}", summary.conflicts.join("\n"))
                        }
                    )
                })
            } else {
                let progress = sender.clone();
                let repaint = ctx.clone();
                run_remote_update(args, move |message| {
                    let _ = progress.send(Message::Progress(message));
                    repaint.request_repaint();
                })
                .and_then(|report| serde_json::to_string_pretty(&report).map_err(Into::into))
            };
            let message = match result {
                Ok(summary) => Message::Complete(Ok(summary)),
                Err(error) => {
                    if let Some(conflicts) = error.downcast_ref::<UnresolvedConflicts>()
                        && let Some(commit) = &conflicts.target_commit
                    {
                        Message::Conflicts(
                            commit.clone(),
                            format!("{error:#}"),
                            repo_for_worker,
                            requested_for_worker,
                        )
                    } else {
                        Message::Complete(Err(format!("{error:#}")))
                    }
                }
            };
            let _ = sender.send(message);
            ctx.request_repaint();
        }));
    }

    fn rollback(&mut self, ctx: egui::Context) {
        let Some(instance) = self.instance_path() else {
            self.append("请先填写实例目录".into());
            return;
        };
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.running = true;
        self.worker = Some(std::thread::spawn(move || {
            let result = transaction::rollback(&instance)
                .map(|()| "已从备份回退未完成更新".to_owned())
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(Message::Complete(result));
            ctx.request_repaint();
        }));
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.running && ctx.input(|input| input.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        let mut incoming = Vec::new();
        if let Some(receiver) = &self.receiver {
            loop {
                match receiver.try_recv() {
                    Ok(message) => incoming.push(message),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if self.running
                            && !incoming.iter().any(|message| {
                                matches!(
                                    message,
                                    Message::Complete(_) | Message::Conflicts(_, _, _, _)
                                )
                            })
                        {
                            incoming.push(Message::WorkerStopped);
                        }
                        break;
                    }
                }
            }
        }
        for message in incoming {
            match message {
                Message::Progress(line) => self.append(line),
                Message::Complete(result) => {
                    self.running = false;
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                    if result.is_ok() {
                        self.pinned_target = None;
                    }
                    self.append(match result {
                        Ok(summary) => summary,
                        Err(error) => format!("操作失败：{error}"),
                    });
                }
                Message::Conflicts(commit, details, origin_repo, origin_target) => {
                    self.running = false;
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                    self.pinned_target = Some((origin_repo, origin_target, commit.clone()));
                    self.append(format!(
                        "目标已固定到 {commit}。填写逐项冲突后再点开始更新。\n{details}"
                    ));
                }
                Message::WorkerStopped => {
                    self.running = false;
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                    self.append("后台更新线程异常终止，请检查事务状态".into());
                }
            }
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("JSIPackUpdater");
            ui.label("选择已停止的 Minecraft 客户端或服务端目录，再选择目标版本。");
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("实例目录");
                if ui
                    .add_enabled(
                        !self.running,
                        egui::TextEdit::singleline(&mut self.instance),
                    )
                    .changed()
                {
                    self.selected_instance = None;
                }
                if ui
                    .add_enabled(!self.running, egui::Button::new("选择文件夹…"))
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new().pick_folder()
                {
                    self.instance = path.to_string_lossy().into_owned();
                    self.selected_instance = Some(path);
                }
            });
            ui.horizontal(|ui| {
                ui.label("目标 tag / commit");
                ui.add_enabled(!self.running, egui::TextEdit::singleline(&mut self.target));
            });
            let pinned_display = self
                .pinned_target
                .as_ref()
                .filter(|(repo, target, _)| {
                    repo == self.repo.trim() && target == self.target.trim()
                })
                .map(|(_, _, commit)| commit.clone());
            if let Some(commit) = pinned_display {
                ui.horizontal(|ui| {
                    ui.label(format!("本次冲突重试固定到 {commit}"));
                    if ui.button("重新解析目标 tag").clicked() {
                        self.pinned_target = None;
                    }
                });
            }
            ui.horizontal(|ui| {
                ui.label("GitHub 仓库");
                ui.add_enabled(!self.running, egui::TextEdit::singleline(&mut self.repo));
            });
            ui.horizontal(|ui| {
                ui.label("GitHub 令牌");
                ui.add(egui::TextEdit::singleline(&mut self.token).password(true));
            });
            ui.horizontal(|ui| {
                ui.label("bkmpw 路径（留空使用随附版本）");
                if ui
                    .add_enabled(!self.running, egui::TextEdit::singleline(&mut self.bkmpw))
                    .changed()
                {
                    self.selected_bkmpw = None;
                }
                if ui
                    .add_enabled(!self.running, egui::Button::new("选择文件…"))
                    .clicked()
                    && let Some(path) = rfd::FileDialog::new().pick_file()
                {
                    self.bkmpw = path.to_string_lossy().into_owned();
                    self.selected_bkmpw = Some(path);
                }
            });
            ui.horizontal(|ui| {
                ui.label("实例侧别");
                ui.radio_value(&mut self.side, Side::Client, "客户端");
                ui.radio_value(&mut self.side, Side::Server, "服务端");
            });
            ui.horizontal(|ui| {
                ui.label("文件冲突");
                ui.radio_value(&mut self.conflicts, ConflictChoice::Stop, "暂停");
                ui.radio_value(
                    &mut self.conflicts,
                    ConflictChoice::UseTarget,
                    "全部使用新版",
                );
                ui.radio_value(
                    &mut self.conflicts,
                    ConflictChoice::KeepLocal,
                    "全部保留本地",
                );
            });
            ui.label("逐项冲突（每行 PATH=target、PATH=local 或 PATH=manual:FILE）");
            ui.add(egui::TextEdit::multiline(&mut self.resolutions).desired_rows(3));
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!self.running, egui::Button::new("预览"))
                    .clicked()
                {
                    self.start(true, ctx.clone());
                }
                if ui
                    .add_enabled(!self.running, egui::Button::new("开始更新"))
                    .clicked()
                {
                    self.start(false, ctx.clone());
                }
                if ui
                    .add_enabled(!self.running, egui::Button::new("回退未完成更新"))
                    .clicked()
                {
                    self.rollback(ctx.clone());
                }
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for message in &self.messages {
                        ui.label(message);
                    }
                });
        });
        if self.running {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([760.0, 620.0]),
        ..Default::default()
    };
    eframe::run_native(
        "JSIPackUpdater",
        options,
        Box::new(|creation| {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "droid-cjk".into(),
                egui::FontData::from_static(include_bytes!("../assets/DroidSansFallbackFull.ttf"))
                    .into(),
            );
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push("droid-cjk".into());
            }
            creation.egui_ctx.set_fonts(fonts);
            Ok(Box::<GuiApp>::default())
        }),
    )
    .map_err(|error| anyhow::anyhow!("opening updater window: {error}"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::GuiApp;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    #[test]
    fn selected_paths_keep_non_unicode_bytes() {
        let instance = PathBuf::from(OsString::from_vec(b"/tmp/instance\xff".to_vec()));
        let bkmpw = PathBuf::from(OsString::from_vec(b"/tmp/bkmpw\xfe".to_vec()));
        let app = GuiApp {
            instance: instance.to_string_lossy().into_owned(),
            selected_instance: Some(instance.clone()),
            bkmpw: bkmpw.to_string_lossy().into_owned(),
            selected_bkmpw: Some(bkmpw.clone()),
            ..GuiApp::default()
        };
        assert_eq!(app.instance_path(), Some(instance));
        assert_eq!(app.bkmpw_path(), Some(bkmpw));
    }
}
