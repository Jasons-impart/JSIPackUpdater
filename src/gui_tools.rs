use eframe::egui;
use minecraft_modpack_updater::tools::{
    self, InstallChoice, Installation, Ownership, Progress, Tool,
};
use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver, TryRecvError},
};

enum Action {
    Inspect,
    Check,
    Install(InstallChoice),
    Upgrade(Tool),
}
enum Message {
    Installed(Tool, Result<Option<Installation>, String>),
    Npm(bool),
    Latest(Tool, Result<String, String>),
    Progress(Progress),
    Done(Result<bool, String>),
}

struct Row {
    tool: Tool,
    installed: Result<Option<Installation>, String>,
    latest: String,
}
impl Row {
    fn new(tool: Tool) -> Self {
        Self {
            tool,
            installed: Err("尚未检测".into()),
            latest: String::new(),
        }
    }
}

pub(super) struct ToolPanel {
    instance: Option<PathBuf>,
    receiver: Option<Receiver<Message>>,
    worker: Option<std::thread::JoinHandle<()>>,
    mutating: bool,
    rows: [Row; 2],
    npm: bool,
    progress: Option<Progress>,
    status: String,
    logs: std::collections::VecDeque<String>,
    recovery_error: Option<String>,
}
impl Default for ToolPanel {
    fn default() -> Self {
        Self {
            instance: None,
            receiver: None,
            worker: None,
            mutating: false,
            rows: [Row::new(Tool::Bkmpw), Row::new(Tool::Updater)],
            npm: false,
            progress: None,
            status: String::new(),
            logs: std::collections::VecDeque::new(),
            recovery_error: std::env::var("MMPU_SELF_UPDATE_ERROR").ok(),
        }
    }
}
impl Drop for ToolPanel {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl ToolPanel {
    pub fn busy(&self) -> bool {
        self.receiver.is_some()
    }
    pub fn mutating(&self) -> bool {
        self.busy() && self.mutating
    }
    pub fn missing(&self) -> bool {
        self.rows[0].installed.as_ref().is_ok_and(Option::is_none)
    }
    fn start(&mut self, action: Action, ctx: egui::Context) {
        if self.busy() {
            return;
        }
        let Some(instance) = self.instance.clone() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        self.mutating = !matches!(action, Action::Inspect | Action::Check);
        self.status = "正在处理工具…".into();
        self.progress = None;
        self.worker = Some(std::thread::spawn(move || {
            let emit = |p| {
                let _ = tx.send(Message::Progress(p));
                ctx.request_repaint();
            };
            let result = (|| -> anyhow::Result<bool> {
                match action {
                    Action::Inspect | Action::Check => {}
                    Action::Install(method) => {
                        tools::install(&instance, Tool::Bkmpw, method, emit)?;
                    }
                    Action::Upgrade(Tool::Bkmpw) => {
                        tools::upgrade_bkmpw(&instance, emit)?;
                    }
                    Action::Upgrade(Tool::Updater) => {
                        return tools::prepare_self_update(&instance, emit);
                    }
                }
                if matches!(action, Action::Check) {
                    for tool in [Tool::Bkmpw, Tool::Updater] {
                        let result = tools::latest(tool)
                            .and_then(|r| {
                                r.supported()?;
                                Ok(r.version)
                            })
                            .map_err(|e| format!("{e:#}"));
                        let _ = tx.send(Message::Latest(tool, result));
                        ctx.request_repaint();
                    }
                }
                Ok(false)
            })()
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(Message::Installed(
                Tool::Bkmpw,
                tools::discover(&instance).map_err(|e| format!("{e:#}")),
            ));
            let _ = tx.send(Message::Installed(
                Tool::Updater,
                tools::current_updater()
                    .map(Some)
                    .map_err(|e| format!("{e:#}")),
            ));
            let _ = tx.send(Message::Npm(tools::npm_available()));
            let _ = tx.send(Message::Done(result));
            ctx.request_repaint();
        }));
    }
    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Some(receiver) = &self.receiver {
            let message = match receiver.try_recv() {
                Ok(message) => message,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.receiver = None;
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                    self.status = "工具任务异常终止，可重试".into();
                    break;
                }
            };
            match message {
                Message::Npm(npm) => self.npm = npm,
                Message::Installed(tool, installed) => {
                    self.rows
                        .iter_mut()
                        .find(|r| r.tool == tool)
                        .unwrap()
                        .installed = installed
                }
                Message::Latest(tool, result) => {
                    self.rows
                        .iter_mut()
                        .find(|r| r.tool == tool)
                        .unwrap()
                        .latest = match result {
                        Ok(version) => format!("可用版本：{version}"),
                        Err(error) => format!("在线检查失败：{error}"),
                    }
                }
                Message::Progress(p) => {
                    let text = p.to_string();
                    self.status = text.chars().take(1500).collect();
                    if !matches!(p, Progress::Download { .. }) {
                        for line in text.lines() {
                            if self.logs.len() >= 100 {
                                self.logs.pop_front();
                            }
                            let tail: Vec<_> = line.chars().rev().take(2048).collect();
                            self.logs.push_back(tail.into_iter().rev().collect());
                        }
                    }
                    self.progress = Some(p);
                }
                Message::Done(result) => {
                    self.receiver = None;
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                    self.progress = None;
                    match result {
                        Ok(true) => {
                            self.status = "正在退出并重新启动更新器…".into();
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        Ok(false) => self.status = "工具检查 / 操作完成".into(),
                        Err(error) => self.status = format!("工具操作失败：{error}"),
                    }
                    break;
                }
            }
        }
    }
    pub fn show(&mut self, ui: &mut egui::Ui, instance: Option<PathBuf>, allowed: bool) {
        self.poll(ui.ctx());
        if !self.busy() && self.instance != instance {
            *self = Self::default();
            self.instance = instance;
            if self.instance.is_some() {
                self.start(Action::Inspect, ui.ctx().clone());
            }
        }
        let missing = self.missing();
        egui::CollapsingHeader::new("工具与更新")
            .open((missing || self.busy()).then_some(true))
            .show(ui, |ui| {
                if let Some(error) = &self.recovery_error {
                    ui.colored_label(
                        egui::Color32::RED,
                        format!("本体升级失败，已启动旧版恢复副本：{error}"),
                    );
                }
                for row in &self.rows {
                    let state = match &row.installed {
                        Ok(Some(i)) => format!(
                            "{} · {}",
                            i.version,
                            match i.ownership {
                                Ownership::Local => "实例目录",
                                Ownership::Npm => "npm 全局",
                                Ownership::External => "外部安装",
                                Ownership::Portable => "便携程序",
                            }
                        ),
                        Ok(None) => "未安装，请选择下方安装方式".into(),
                        Err(error) => format!("检测状态：{error}"),
                    };
                    ui.label(format!("{}：{state}", row.tool.name()));
                    if !row.latest.is_empty() {
                        ui.small(&row.latest);
                    }
                }
                let ready = allowed && !self.busy() && self.instance.is_some();
                ui.add_enabled_ui(ready, |ui| {
                    if ui.button("检查两个工具的更新").clicked() {
                        self.start(Action::Check, ui.ctx().clone());
                    }
                    ui.label("bkmpw 安装方式");
                    ui.horizontal(|ui| {
                        if ui.button("下载到实例目录").clicked() {
                            self.start(Action::Install(InstallChoice::Local), ui.ctx().clone());
                        }
                        if ui
                            .add_enabled(self.npm, egui::Button::new("npm 全局安装"))
                            .clicked()
                        {
                            self.start(Action::Install(InstallChoice::Npm), ui.ctx().clone());
                        }
                    });
                    if !self.npm {
                        ui.small("未检测到 npm，可直接下载到实例目录");
                    }
                    ui.horizontal(|ui| {
                        let managed = self.rows[0]
                            .installed
                            .as_ref()
                            .ok()
                            .and_then(|i| i.as_ref())
                            .is_some_and(|i| {
                                matches!(i.ownership, Ownership::Local | Ownership::Npm)
                            });
                        if ui
                            .add_enabled(managed, egui::Button::new("升级 bkmpw"))
                            .clicked()
                        {
                            self.start(Action::Upgrade(Tool::Bkmpw), ui.ctx().clone());
                        }
                        if ui.button("升级更新器并重启").clicked() {
                            self.start(Action::Upgrade(Tool::Updater), ui.ctx().clone());
                        }
                    });
                });
                if self.busy() {
                    ui.spinner();
                }
                if let Some(Progress::Download { .. }) = &self.progress {
                    if let Some(fraction) = self.progress.as_ref().and_then(Progress::fraction) {
                        ui.add(egui::ProgressBar::new(fraction).show_percentage());
                    } else {
                        ui.label("下载中（服务器未提供总大小）");
                    }
                }
                if !self.status.is_empty() {
                    ui.label(&self.status);
                }
                if !self.logs.is_empty() {
                    ui.collapsing("工具操作日志", |ui| {
                        for line in &self.logs {
                            ui.label(line);
                        }
                    });
                }
            });
        if self.rows[0].installed.as_ref().is_ok_and(Option::is_none) {
            ui.label("更新需要 bkmpw，请展开「工具与更新」选择安装方式。");
        }
        if self.busy() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}
