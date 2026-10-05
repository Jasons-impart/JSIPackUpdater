use eframe::egui;
use minecraft_modpack_updater::updater::ConflictResolution;
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct ConflictPanel {
    pub paths: Vec<String>,
    pub resolutions: BTreeMap<String, ConflictResolution>,
}

impl ConflictPanel {
    pub fn receive(&mut self, paths: Vec<String>) {
        for path in paths {
            if !self.paths.contains(&path) {
                self.paths.push(path);
            }
        }
    }

    pub fn ready(&self) -> bool {
        self.paths
            .iter()
            .all(|path| self.resolutions.contains_key(path))
    }

    fn choose_all(&mut self, choice: ConflictResolution) {
        for path in &self.paths {
            self.resolutions.insert(path.clone(), choice.clone());
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        if self.paths.is_empty() {
            return;
        }
        ui.separator();
        ui.heading("3. 处理文件冲突");
        let remaining = self
            .paths
            .iter()
            .filter(|path| !self.resolutions.contains_key(*path))
            .count();
        ui.label(format!(
            "有 {} 个文件需要确认，{} 个尚未选择。选择完后点继续更新。",
            self.paths.len(),
            remaining
        ));
        ui.horizontal_wrapped(|ui| {
            if ui.button("全部保留我的修改").clicked() {
                self.choose_all(ConflictResolution::KeepLocal);
            }
            if ui.button("全部使用所选版本").clicked() {
                self.choose_all(ConflictResolution::UseTarget);
            }
        });
        egui::ScrollArea::vertical()
            .id_salt("file-conflicts")
            .max_height(240.0)
            .show(ui, |ui| {
                for path in &self.paths {
                    ui.push_id(path, |ui| {
                        ui.group(|ui| {
                            ui.label(path);
                            if matches!(path.as_str(), "pack.toml" | "index.toml") {
                                ui.small("这是生成的索引文件，通常使用所选版本即可。");
                            }
                            let target = matches!(
                                self.resolutions.get(path),
                                Some(ConflictResolution::UseTarget)
                            );
                            let local = matches!(
                                self.resolutions.get(path),
                                Some(ConflictResolution::KeepLocal)
                            );
                            ui.horizontal_wrapped(|ui| {
                                if ui.selectable_label(local, "保留我的修改").clicked() {
                                    self.resolutions
                                        .insert(path.clone(), ConflictResolution::KeepLocal);
                                }
                                if ui.selectable_label(target, "使用所选版本").clicked() {
                                    self.resolutions
                                        .insert(path.clone(), ConflictResolution::UseTarget);
                                }
                                if ui.button("选择合并文件…").clicked()
                                    && let Some(file) = rfd::FileDialog::new()
                                        .set_title("选择在实例外编辑好的合并文件")
                                        .pick_file()
                                {
                                    self.resolutions
                                        .insert(path.clone(), ConflictResolution::Manual(file));
                                }
                            });
                            if let Some(ConflictResolution::Manual(file)) =
                                self.resolutions.get(path)
                            {
                                ui.small(format!("使用已合并文件：{}", file.display()));
                            }
                        });
                    });
                }
            });
    }
}

#[cfg(test)]
mod tests {
    use super::ConflictPanel;
    use minecraft_modpack_updater::updater::ConflictResolution;

    #[test]
    fn batch_choice_only_resolves_conflicts_already_shown() {
        let mut panel = ConflictPanel::default();
        panel.receive(vec!["pack.toml".into(), "config/local=custom.toml".into()]);
        assert!(!panel.ready());
        panel.choose_all(ConflictResolution::KeepLocal);
        assert!(panel.ready());
        panel.receive(vec!["mods/new.jar".into(), "pack.toml".into()]);
        assert_eq!(panel.paths.len(), 3);
        assert!(!panel.ready());
        assert!(matches!(
            panel.resolutions["config/local=custom.toml"],
            ConflictResolution::KeepLocal
        ));
        assert!(!panel.resolutions.contains_key("mods/new.jar"));
    }

    #[test]
    fn manual_choice_survives_next_preparation_phase() {
        let mut panel = ConflictPanel::default();
        panel.receive(vec!["config/game.toml".into()]);
        let file = std::path::PathBuf::from("merged config.toml");
        panel.resolutions.insert(
            "config/game.toml".into(),
            ConflictResolution::Manual(file.clone()),
        );
        panel.receive(vec!["mods/custom.jar".into()]);
        assert!(
            matches!(&panel.resolutions["config/game.toml"], ConflictResolution::Manual(saved) if saved == &file)
        );
        assert!(!panel.ready());
    }
}
