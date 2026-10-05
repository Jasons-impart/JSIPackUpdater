use eframe::egui;
use minecraft_modpack_updater::managed::{ModChange, ModChangeKind, ModFile};

fn file_label(file: &ModFile) -> String {
    let selection = if file.optional {
        if file.default_enabled {
            "（可选，默认启用）"
        } else {
            "（可选，默认关闭）"
        }
    } else {
        ""
    };
    format!("{}{}", file.path, selection)
}

pub(super) fn show(ui: &mut egui::Ui, changes: &[ModChange]) {
    let groups = [
        (ModChangeKind::Added, "新增"),
        (ModChangeKind::Removed, "移除"),
        (ModChangeKind::Changed, "变更"),
    ];
    ui.separator();
    ui.heading("模组变更清单");
    ui.label(
        groups
            .iter()
            .map(|(kind, label)| {
                format!(
                    "{label} {}",
                    changes.iter().filter(|c| c.kind == *kind).count()
                )
            })
            .collect::<Vec<_>>()
            .join("  ·  "),
    );
    ui.small("比较当前发布版本与所选版本的模组元数据，文件名用于辨认版本。可选模组标注发布默认值，更新会保留已有安装选择。实际文件冲突在更新准备时检查。");
    if changes.is_empty() {
        ui.label("这两个发布版本没有模组变更。");
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("mod-changes")
        .max_height(240.0)
        .show(ui, |ui| {
            for (kind, label) in groups {
                for change in changes.iter().filter(|c| c.kind == kind) {
                    if let Some(file) = change.after.as_ref().or(change.before.as_ref()) {
                        ui.strong(format!("{label}：{}", file.name));
                    }
                    if let Some(file) = &change.before {
                        ui.label(format!("原版本：{}", file_label(file)));
                    }
                    if let Some(file) = &change.after {
                        ui.label(format!("所选版本：{}", file_label(file)));
                    }
                    if change.content_changed
                        && change
                            .before
                            .as_ref()
                            .zip(change.after.as_ref())
                            .is_some_and(|(a, b)| a.path == b.path)
                    {
                        ui.small("文件名相同，校验值已变化。");
                    }
                    ui.add_space(5.0);
                }
            }
        });
}
