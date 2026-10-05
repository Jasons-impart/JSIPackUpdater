use super::{ConflictChoice, PreviewSummary, RemoteArgs, run_remote_preview, run_remote_update};
use anyhow::Result;
use eframe::egui;
use minecraft_modpack_updater::managed::Side;
use minecraft_modpack_updater::safe_fs::{LocalFile, SafeRoot};
use minecraft_modpack_updater::source::GitHub;
use minecraft_modpack_updater::transaction;
use minecraft_modpack_updater::updater::UnresolvedConflicts;
use minecraft_modpack_updater::version::{ReleaseInfo, load_release_info};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

#[path = "gui_conflicts.rs"]
mod conflicts;
use conflicts::ConflictPanel;

#[path = "gui_mod_changes.rs"]
mod mod_changes;
#[path = "gui_tools.rs"]
mod tool_management;

fn failure_hint(error: &str) -> &'static str {
    if error.contains("changes Minecraft/NeoForge") {
        "所选版本更换了 Minecraft 或 NeoForge，需要新建实例。请选择其他版本，或安装新的整合包。"
    } else if error.contains("unfinished update") {
        "上次更新尚未完成，请先恢复更新前的文件，再重试。"
    } else if error.contains("another update is using") {
        "另一个更新器正在使用这个实例，请等待它结束后重试。"
    } else if error.contains("rate limit") {
        "GitHub 请求次数已达上限。请按日志中的时间重试，或在高级设置中填写令牌。"
    } else {
        "操作未完成，可以重试。请展开详细日志查看原因；出现恢复按钮时，请先恢复更新前的文件。"
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Operation {
    Preview,
    Update,
    Rollback,
}

enum Message {
    Progress(String),
    Complete(Result<String, String>),
    Preview {
        summary: PreviewSummary,
        repo: String,
        target: String,
    },
    Conflicts {
        commit: String,
        paths: Vec<String>,
        details: String,
        repo: String,
        target: String,
    },
    WorkerStopped,
}

enum SetupMessage {
    Instances(Vec<PathBuf>),
    Releases(u64, Result<Vec<String>, String>),
    Installed(Inspection),
}

struct Inspection {
    request: u64,
    path: PathBuf,
    resolved: PathBuf,
    announce: bool,
    side_revision: u64,
    result: Result<(ReleaseInfo, Side, Option<Side>), String>,
    pending: Result<bool, String>,
}

struct GuiApp {
    tools: tool_management::ToolPanel,
    instance: String,
    selected_instance: Option<PathBuf>,
    target: String,
    repo: String,
    token: String,
    bkmpw: String,
    selected_bkmpw: Option<PathBuf>,
    side: Side,
    conflicts: ConflictPanel,
    messages: VecDeque<String>,
    receiver: Option<Receiver<Message>>,
    worker: Option<JoinHandle<()>>,
    running: bool,
    status: String,
    initialized: bool,
    setup_tx: mpsc::Sender<SetupMessage>,
    setup_rx: Receiver<SetupMessage>,
    release_request: u64,
    releases_fetching: bool,
    release_repo: String,
    release_status: String,
    releases: Vec<String>,
    instances: Vec<PathBuf>,
    installed: Option<ReleaseInfo>,
    pending: bool,
    inspection_request: u64,
    inspecting: bool,
    side_revision: u64,
    saved_side: Option<Side>,
    operation: Operation,
    failed: bool,
    pinned_target: Option<(String, String, String)>,
    preview: Option<PreviewSummary>,
}

impl Default for GuiApp {
    fn default() -> Self {
        let (setup_tx, setup_rx) = mpsc::channel();
        Self {
            tools: tool_management::ToolPanel::default(),
            instance: String::new(),
            selected_instance: None,
            target: String::new(),
            repo: "Jasons-impart/Create-Delight-Project-Rebirth".into(),
            token: String::new(),
            bkmpw: String::new(),
            selected_bkmpw: None,
            side: Side::Client,
            conflicts: ConflictPanel::default(),
            messages: VecDeque::new(),
            receiver: None,
            worker: None,
            running: false,
            status: "选择实例和目标版本后即可更新".into(),
            initialized: false,
            setup_tx,
            setup_rx,
            release_request: 0,
            releases_fetching: false,
            release_repo: String::new(),
            release_status: String::new(),
            releases: Vec::new(),
            instances: Vec::new(),
            installed: None,
            pending: false,
            inspection_request: 0,
            inspecting: false,
            side_revision: 0,
            saved_side: None,
            operation: Operation::Update,
            failed: false,
            pinned_target: None,
            preview: None,
        }
    }
}

impl GuiApp {
    fn token(&self) -> Option<String> {
        if self.token.trim().is_empty() {
            std::env::var("GITHUB_TOKEN")
                .ok()
                .filter(|value| !value.is_empty())
        } else {
            Some(self.token.trim().to_owned())
        }
    }

    fn fetch_releases(&mut self, ctx: egui::Context) {
        if self.releases_fetching {
            return;
        }
        self.releases_fetching = true;
        self.release_request += 1;
        self.release_repo = self.repo.trim().to_owned();
        self.releases.clear();
        self.release_status = "正在获取可更新版本…".into();
        let (repo, token, request, sender) = (
            self.release_repo.clone(),
            self.token(),
            self.release_request,
            self.setup_tx.clone(),
        );
        std::thread::spawn(move || {
            let result = GitHub::new(&repo, token)
                .and_then(|github| github.release_tags())
                .map_err(|error| format!("{error:#}"));
            let _ = sender.send(SetupMessage::Releases(request, result));
            ctx.request_repaint();
        });
    }

    fn select_instance(&mut self, path: PathBuf, ctx: egui::Context) {
        if self.running || self.tools.mutating() {
            return;
        }
        if self.inspecting {
            return;
        }
        self.instance = path.to_string_lossy().into_owned();
        self.selected_instance = Some(path.clone());
        self.installed = None;
        self.saved_side = None;
        self.pending = false;
        self.reset_decisions();
        self.status = "正在识别实例…".into();
        self.inspect_instance(ctx, true);
    }

    fn inspect_instance(&mut self, ctx: egui::Context, announce: bool) {
        let Some(path) = self.instance_path() else {
            return;
        };
        let sender = self.setup_tx.clone();
        self.inspection_request += 1;
        self.inspecting = true;
        let request = self.inspection_request;
        let side_revision = self.side_revision;
        std::thread::spawn(move || {
            let resolved = [
                path.clone(),
                path.join("minecraft"),
                path.join(".minecraft"),
            ]
            .into_iter()
            .find(|candidate| super::instance_paths::is_instance(candidate))
            .unwrap_or_else(|| path.clone());
            let pending = transaction::pending(&resolved).map_err(|error| format!("{error:#}"));
            let result = (|| -> Result<_> {
                let root = SafeRoot::open(&resolved)?;
                let info = load_release_info(&root)?;
                let saved_side: Option<Side> = match root.read(".jsipack-updater/side.json")? {
                    LocalFile::File(bytes) => Some(serde_json::from_slice(&bytes)?),
                    LocalFile::Missing => None,
                    LocalFile::Obstacle => anyhow::bail!("实例侧别记录不是普通文件"),
                };
                let side = if let Some(side) = saved_side {
                    side
                } else if matches!(root.read("server.properties")?, LocalFile::File(_)) {
                    Side::Server
                } else {
                    Side::Client
                };
                Ok((info, side, saved_side))
            })()
            .map_err(|error| format!("{error:#}"));
            let _ = sender.send(SetupMessage::Installed(Inspection {
                request,
                path,
                resolved,
                announce,
                side_revision,
                result,
                pending,
            }));
            ctx.request_repaint();
        });
    }

    fn poll_setup(&mut self) {
        while let Ok(message) = self.setup_rx.try_recv() {
            match message {
                SetupMessage::Instances(paths) => self.instances = paths,
                SetupMessage::Releases(request, result) if request == self.release_request => {
                    self.releases_fetching = false;
                    if self.release_repo != self.repo.trim() {
                        continue;
                    }
                    match result {
                        Ok(tags) => {
                            self.releases = tags;
                            if self.target.is_empty()
                                && let Some(first) = self.releases.first()
                            {
                                self.target = first.clone();
                            }
                            self.release_status = if self.releases.is_empty() {
                                "没有已发布版本；可在高级设置中指定版本"
                            } else {
                                "可选择更新或回退到任一发布版本"
                            }
                            .into();
                        }
                        Err(error) => {
                            self.release_status =
                                "获取版本失败，请重试；也可在高级设置中指定版本".into();
                            self.append(error);
                        }
                    }
                }
                SetupMessage::Installed(inspection)
                    if inspection.request == self.inspection_request
                        && self.instance_path().as_ref() == Some(&inspection.path)
                        && !self.running =>
                {
                    self.inspecting = false;
                    self.instance = inspection.resolved.to_string_lossy().into_owned();
                    self.selected_instance = Some(inspection.resolved);
                    match inspection.pending {
                        Ok(pending) => self.pending = pending,
                        Err(error) => {
                            self.installed = None;
                            self.status = "无法检查更新事务，请查看详细日志后重试识别实例。".into();
                            self.append(error);
                            continue;
                        }
                    }
                    match inspection.result {
                        Ok((info, side, saved_side)) => {
                            self.installed = Some(info);
                            self.saved_side = saved_side;
                            let previous_side = self.side;
                            if saved_side.is_some()
                                || (inspection.announce
                                    && inspection.side_revision == self.side_revision)
                            {
                                self.side = side;
                            }
                            if self.side != previous_side {
                                self.reset_decisions();
                                self.side_revision += 1;
                            }
                            if inspection.announce {
                                self.status = if self.pending {
                                    "上次更新未完成，请先恢复更新前的文件。"
                                } else {
                                    "实例已识别。请先关闭游戏或停止服务端，再开始更新。"
                                }
                                .into();
                            }
                        }
                        Err(error) => {
                            self.installed = None;
                            self.status = if self.pending {
                                "上次更新未完成，请先恢复更新前的文件。"
                            } else {
                                "无法识别实例。请选择含发布版本标记的游戏目录。"
                            }
                            .into();
                            self.append(error);
                        }
                    }
                }
                _ => {}
            }
        }
    }

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

    fn reset_decisions(&mut self) {
        self.pinned_target = None;
        self.preview = None;
        self.conflicts = ConflictPanel::default();
        self.failed = false;
    }

    fn remote_args(&self) -> Option<RemoteArgs> {
        let instance = self.instance_path()?;
        if self.target.trim().is_empty() {
            return None;
        }
        let requested = self.target.trim().to_owned();
        let repo = self.repo.trim().to_owned();
        let target = self
            .pinned_target
            .as_ref()
            .filter(|(saved_repo, saved_target, _)| {
                saved_repo == &repo && saved_target == &requested
            })
            .map(|(_, _, commit)| commit.clone())
            .unwrap_or(requested);
        Some(RemoteArgs {
            instance,
            target,
            side: self.side,
            bkmpw: self.bkmpw_path(),
            repo,
            token: self.token(),
            conflicts: ConflictChoice::Stop,
            resolutions: self.conflicts.resolutions.clone(),
        })
    }

    fn start(&mut self, preview: bool, ctx: egui::Context) {
        if self.running || self.tools.mutating() {
            return;
        }
        let Some(args) = self.remote_args() else {
            self.status = "请先选择实例和目标版本。".into();
            return;
        };
        let requested_for_worker = self.target.trim().to_owned();
        let repo_for_worker = args.repo.clone();
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.running = true;
        self.operation = if preview {
            Operation::Preview
        } else {
            Operation::Update
        };
        self.failed = false;
        self.inspection_request += 1;
        self.inspecting = false;
        self.status = if preview {
            "正在检查更新内容…"
        } else {
            "正在准备、校验并应用更新…"
        }
        .into();
        self.messages.clear();
        if preview {
            self.preview = None;
        }
        self.worker = Some(std::thread::spawn(move || {
            let result = if preview {
                run_remote_preview(&args).map(|summary| {
                    let text = format!(
                        "源码检查完成：{} 项，{} 个冲突。\n模组与生成文件冲突会在更新准备时检查。\n{} → {}{}",
                        summary.item_count,
                        summary.conflicts.len(),
                        summary.base_commit,
                        summary.target_commit,
                        if summary.conflicts.is_empty() {
                            String::new()
                        } else {
                            format!("\n{}", summary.conflicts.join("\n"))
                        }
                    );
                    let _ = sender.send(Message::Preview {
                        summary,
                        repo: repo_for_worker.clone(),
                        target: requested_for_worker.clone(),
                    });
                    text
                })
            } else {
                let progress = sender.clone();
                let repaint = ctx.clone();
                run_remote_update(args, move |message| {
                    let _ = progress.send(Message::Progress(message));
                    repaint.request_repaint();
                })
                .map(|report| format!("更新完成，实例已切换到 {requested_for_worker}。可以启动游戏或服务端了。\n更新了 {} 个文件，校验了 {} 个受管文件，保留了 {} 项本地修改。\n目标提交：{}\n本地覆盖文件：\n{}", report.changed_files, report.managed_files, report.local_overrides.len(), report.target_commit, report.local_overrides.join("\n")))
            };
            let message = match result {
                Ok(summary) => Message::Complete(Ok(summary)),
                Err(error) => {
                    if let Some(conflicts) = error.downcast_ref::<UnresolvedConflicts>()
                        && let Some(commit) = &conflicts.target_commit
                    {
                        Message::Conflicts {
                            commit: commit.clone(),
                            paths: conflicts.paths.clone(),
                            details: format!("{error:#}"),
                            repo: repo_for_worker,
                            target: requested_for_worker,
                        }
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
        if self.running || self.tools.mutating() {
            return;
        }
        let Some(instance) = self.instance_path() else {
            self.append("请先填写实例目录".into());
            return;
        };
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.running = true;
        self.operation = Operation::Rollback;
        self.failed = false;
        self.status = "正在恢复更新前的文件…".into();
        self.inspection_request += 1;
        self.inspecting = false;
        self.worker = Some(std::thread::spawn(move || {
            let result = (|| -> Result<()> {
                let _guard = minecraft_modpack_updater::tools::tool_lock(
                    minecraft_modpack_updater::tools::Tool::Updater,
                    false,
                )?;
                transaction::rollback(&instance)
            })()
            .map(|()| "已从备份回退未完成更新".to_owned())
            .map_err(|error| format!("{error:#}"));
            let _ = sender.send(Message::Complete(result));
            ctx.request_repaint();
        }));
    }
}

impl GuiApp {
    fn render(&mut self, ctx: &egui::Context) {
        if !self.initialized {
            self.initialized = true;
            self.fetch_releases(ctx.clone());
            let sender = self.setup_tx.clone();
            let repaint = ctx.clone();
            std::thread::spawn(move || {
                let paths = super::instance_paths::directory_suggestions("")
                    .into_iter()
                    .map(|entry| entry.path)
                    .collect();
                let _ = sender.send(SetupMessage::Instances(paths));
                repaint.request_repaint();
            });
        }
        self.poll_setup();
        self.tools.poll(ctx);
        if (self.running || self.tools.mutating())
            && ctx.input(|input| input.viewport().close_requested())
        {
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
                                matches!(message, Message::Complete(_) | Message::Conflicts { .. })
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
                Message::Preview {
                    summary,
                    repo,
                    target,
                } => {
                    self.pinned_target = Some((repo, target, summary.target_commit.clone()));
                    self.preview = Some(summary);
                }
                Message::Complete(result) => {
                    self.running = false;
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                    self.failed = result.is_err();
                    if result.is_ok() && self.operation != Operation::Preview {
                        self.reset_decisions();
                    }
                    self.status = match result {
                        Ok(summary) => {
                            self.append(summary.clone());
                            summary.lines().next().unwrap_or("操作完成").to_owned()
                        }
                        Err(error) => {
                            let hint = failure_hint(&error).to_owned();
                            self.append(error);
                            hint
                        }
                    };
                    self.inspect_instance(ctx.clone(), false);
                }
                Message::Conflicts {
                    commit,
                    paths,
                    details,
                    repo,
                    target,
                } => {
                    self.running = false;
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                    self.pinned_target = Some((repo, target, commit));
                    self.conflicts.receive(paths);
                    self.status = "发现文件冲突，请选择如何处理后继续更新。".into();
                    self.append(details);
                }
                Message::WorkerStopped => {
                    self.running = false;
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                    self.status = "后台任务异常终止，请检查事务状态后重试。".into();
                    self.failed = true;
                    self.inspect_instance(ctx.clone(), false);
                }
            }
        }
        let selection_before = (self.repo.clone(), self.target.clone(), self.side);
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("update-form")
                .show(ui, |ui| {
                    ui.heading("Minecraft Modpack Updater");
                    ui.label(&self.status);
                    ui.label("选择实例、选择版本，然后开始更新");
                    ui.small("更新前请关闭游戏或停止服务端。");
                    ui.separator();
                    ui.heading("1. 选择实例");
                    let mut chosen = None;
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(
                            !self.running && !self.inspecting && !self.tools.mutating(),
                            |ui| {
                                egui::ComboBox::from_id_salt("instances")
                                    .selected_text("检测到的实例")
                                    .show_ui(ui, |ui| {
                                        for path in &self.instances {
                                            if ui
                                                .selectable_label(
                                                    self.selected_instance.as_ref() == Some(path),
                                                    path.to_string_lossy(),
                                                )
                                                .clicked()
                                            {
                                                chosen = Some(path.clone());
                                            }
                                        }
                                        if self.instances.is_empty() {
                                            ui.label("未发现实例，请选择文件夹");
                                        }
                                    });
                            },
                        );
                        if ui
                            .add_enabled(
                                !self.running && !self.inspecting && !self.tools.mutating(),
                                egui::Button::new("选择文件夹…"),
                            )
                            .clicked()
                            && let Some(path) = rfd::FileDialog::new().pick_folder()
                        {
                            chosen = Some(path);
                        }
                    });
                    if let Some(path) = chosen {
                        self.select_instance(path, ctx.clone());
                    }
                    if !self.instance.is_empty() {
                        ui.label(&self.instance);
                    }
                    if let Some(info) = &self.installed {
                        ui.label(format!(
                            "当前版本：{}  ·  Minecraft {}  ·  NeoForge {}",
                            info.version, info.minecraft_version, info.neoforge_version
                        ));
                    }
                    ui.horizontal(|ui| {
                        ui.label("实例类型");
                        ui.add_enabled_ui(!self.running && self.saved_side.is_none(), |ui| {
                            ui.radio_value(&mut self.side, Side::Client, "客户端");
                            ui.radio_value(&mut self.side, Side::Server, "服务端");
                        });
                    });
                    ui.separator();
                    ui.heading("2. 选择版本");
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(!self.running, |ui| {
                            egui::ComboBox::from_id_salt("releases")
                                .selected_text(if self.target.is_empty() {
                                    "请选择发布版本"
                                } else {
                                    &self.target
                                })
                                .show_ui(ui, |ui| {
                                    for tag in &self.releases {
                                        ui.selectable_value(&mut self.target, tag.clone(), tag);
                                    }
                                });
                        });
                        if ui
                            .add_enabled(
                                !self.running && !self.releases_fetching,
                                egui::Button::new("刷新版本"),
                            )
                            .clicked()
                        {
                            self.fetch_releases(ctx.clone());
                        }
                    });
                    ui.label(&self.release_status);
                    let pinned_display = self
                        .pinned_target
                        .as_ref()
                        .filter(|(repo, target, _)| {
                            repo == self.repo.trim() && target == self.target.trim()
                        })
                        .map(|(_, _, commit)| commit.clone());
                    ui.collapsing("高级设置（通常无需修改）", |ui| {
                        ui.add_enabled_ui(!self.running, |ui| {
                            if let Some(commit) = pinned_display {
                                ui.label(format!("本次更新固定到：{commit}"));
                                if ui.button("重新检查所选版本").clicked() {
                                    self.reset_decisions();
                                }
                            }
                            ui.horizontal(|ui| {
                                ui.label("指定 tag / commit");
                                ui.text_edit_singleline(&mut self.target);
                            });
                            ui.horizontal(|ui| {
                                ui.label("GitHub 仓库");
                                ui.add_enabled(
                                    !self.running,
                                    egui::TextEdit::singleline(&mut self.repo),
                                );
                            });
                            ui.horizontal(|ui| {
                                ui.label("GitHub 令牌");
                                ui.add(egui::TextEdit::singleline(&mut self.token).password(true));
                            });
                            ui.horizontal(|ui| {
                                ui.label("bkmpw 路径（留空自动检测）");
                                if ui
                                    .add_enabled(
                                        !self.running,
                                        egui::TextEdit::singleline(&mut self.bkmpw),
                                    )
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
                        });
                    });
                    if self.repo != selection_before.0 {
                        self.releases.clear();
                        self.target.clear();
                        self.release_status = "仓库已变更，请刷新版本列表".into();
                    }
                    if (self.repo.clone(), self.target.clone(), self.side) != selection_before {
                        self.reset_decisions();
                        if self.installed.is_some() {
                            self.status =
                                "已更改更新目标。请关闭游戏或停止服务端，再开始更新。".into();
                        }
                    }
                    if self.side != selection_before.2 {
                        self.side_revision += 1;
                    }
                    let tool_instance = self.instance_path();
                    self.tools.show(
                        ui,
                        tool_instance,
                        !self.running && !self.pending && !self.inspecting,
                    );
                    ui.add_enabled_ui(!self.running && !self.pending, |ui| {
                        self.conflicts.show(ui);
                    });
                    ui.horizontal(|ui| {
                        let ready = !self.running
                            && !self.tools.mutating()
                            && !self.inspecting
                            && self.installed.is_some()
                            && !self.pending
                            && !self.target.trim().is_empty();
                        if ui
                            .add_enabled(
                                ready,
                                egui::Button::new(
                                    if self.failed && self.operation == Operation::Preview {
                                        "重新检查"
                                    } else {
                                        "查看更新内容"
                                    },
                                ),
                            )
                            .clicked()
                        {
                            self.start(true, ctx.clone());
                        }
                        if ui
                            .add_enabled(
                                ready
                                    && self.conflicts.ready()
                                    && (self.bkmpw_path().is_some() || !self.tools.missing()),
                                egui::Button::new(if !self.conflicts.paths.is_empty() {
                                    "继续更新"
                                } else if self.failed && self.operation == Operation::Update {
                                    "重试更新"
                                } else {
                                    "开始更新"
                                }),
                            )
                            .clicked()
                        {
                            self.start(false, ctx.clone());
                        }
                        if self.pending
                            && ui
                                .add_enabled(
                                    !self.running && !self.inspecting && !self.tools.mutating(),
                                    egui::Button::new("恢复更新前的文件"),
                                )
                                .clicked()
                        {
                            self.rollback(ctx.clone());
                        }
                    });
                    if self.running || self.inspecting {
                        ui.spinner();
                    }
                    if let Some(preview) = &self.preview {
                        mod_changes::show(ui, &preview.mods);
                    }
                    ui.separator();
                    ui.collapsing("详细日志", |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(180.0)
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                for message in &self.messages {
                                    ui.label(message);
                                }
                            });
                    });
                });
        });
        if self.running {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod setup_tests {
    use super::{GuiApp, Inspection, SetupMessage};
    use minecraft_modpack_updater::managed::Side;
    use minecraft_modpack_updater::version::ReleaseInfo;

    #[test]
    fn stale_release_requests_cannot_change_selected_version() {
        let mut app = GuiApp {
            release_request: 2,
            release_repo: "owner/repo".into(),
            repo: "owner/repo".into(),
            ..GuiApp::default()
        };
        app.setup_tx
            .send(SetupMessage::Releases(1, Ok(vec!["old".into()])))
            .unwrap();
        app.poll_setup();
        assert!(app.target.is_empty());
        app.setup_tx
            .send(SetupMessage::Releases(2, Ok(vec!["new".into()])))
            .unwrap();
        app.poll_setup();
        assert_eq!(app.target, "new");
        app.repo = "other/repo".into();
        app.setup_tx
            .send(SetupMessage::Releases(2, Ok(vec!["wrong".into()])))
            .unwrap();
        app.poll_setup();
        assert_eq!(app.target, "new");
    }

    #[test]
    fn stale_instance_result_cannot_change_current_instance_state() {
        let mut app = GuiApp {
            selected_instance: Some("current".into()),
            status: "unchanged".into(),
            ..GuiApp::default()
        };
        app.setup_tx
            .send(SetupMessage::Installed(Inspection {
                request: 0,
                path: "previous".into(),
                resolved: "previous".into(),
                announce: true,
                side_revision: 0,
                result: Err("old error".into()),
                pending: Ok(true),
            }))
            .unwrap();
        app.poll_setup();
        assert_eq!(app.status, "unchanged");
        assert!(app.messages.is_empty());
    }

    #[test]
    fn same_path_inspections_preserve_newer_state_and_explicit_side() {
        let mut app = GuiApp {
            selected_instance: Some("current".into()),
            inspection_request: 2,
            side_revision: 1,
            side: Side::Client,
            ..GuiApp::default()
        };
        let inspection = |request, announce, side_revision| {
            SetupMessage::Installed(Inspection {
                request,
                path: "current".into(),
                resolved: "current".into(),
                announce,
                side_revision,
                pending: Ok(true),
                result: Ok((
                    ReleaseInfo {
                        schema_version: 1,
                        version: "test4".into(),
                        commit_id: "a".repeat(40),
                        minecraft_version: "1.21.1".into(),
                        neoforge_version: "21.1.242".into(),
                    },
                    Side::Server,
                    None,
                )),
            })
        };
        app.setup_tx.send(inspection(1, true, 0)).unwrap();
        app.poll_setup();
        assert!(app.installed.is_none());
        assert!(!app.pending);
        app.setup_tx.send(inspection(2, true, 0)).unwrap();
        app.poll_setup();
        assert_eq!(app.side, Side::Client);
        assert!(app.pending);
        app.setup_tx.send(inspection(2, false, 1)).unwrap();
        app.poll_setup();
        assert_eq!(app.side, Side::Client);
    }

    #[test]
    fn corrupted_marker_does_not_hide_pending_transaction() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join(".jsipack-updater")).unwrap();
        std::fs::write(root.path().join(".jsipack-updater/active.json"), b"{}").unwrap();
        let mut app = GuiApp::default();
        app.select_instance(root.path().to_path_buf(), eframe::egui::Context::default());
        let message = app
            .setup_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        app.setup_tx.send(message).unwrap();
        app.poll_setup();
        assert!(app.pending);
        assert!(app.installed.is_none());
        assert!(!app.inspecting);
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.render(ctx);
    }
}

pub fn run() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([760.0, 620.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Minecraft Modpack Updater",
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
            creation.egui_ctx.style_mut(|style| {
                style
                    .text_styles
                    .insert(egui::TextStyle::Body, egui::FontId::proportional(16.0));
                style
                    .text_styles
                    .insert(egui::TextStyle::Button, egui::FontId::proportional(16.0));
                style
                    .text_styles
                    .insert(egui::TextStyle::Heading, egui::FontId::proportional(22.0));
                style.spacing.item_spacing = egui::vec2(8.0, 8.0);
            });
            Ok(Box::<GuiApp>::default())
        }),
    )
    .map_err(|error| anyhow::anyhow!("opening updater window: {error}"))
}

#[cfg(test)]
#[path = "gui_tests.rs"]
mod interaction_tests;

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
