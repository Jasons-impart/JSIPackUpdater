use super::{GuiApp, Message};
use eframe::egui::{self, Event, PointerButton};
use jsipack_updater::managed::Side;
use jsipack_updater::managed::{ModChange, ModChangeKind, ModFile};
use jsipack_updater::updater::ConflictResolution;
use jsipack_updater::version::ReleaseInfo;

fn app() -> GuiApp {
    GuiApp {
        initialized: true,
        selected_instance: Some("fixture-instance".into()),
        target: "new".into(),
        releases: vec!["new".into(), "old".into()],
        installed: Some(ReleaseInfo {
            schema_version: 1,
            version: "installed".into(),
            commit_id: "a".repeat(40),
            minecraft_version: "1.21.1".into(),
            neoforge_version: "21.1.242".into(),
        }),
        ..GuiApp::default()
    }
}

fn preview() -> super::PreviewSummary {
    let file = |name: &str, path: &str| ModFile {
        name: name.into(),
        path: path.into(),
        optional: false,
        default_enabled: true,
    };
    super::PreviewSummary {
        base_commit: "a".repeat(40),
        target_commit: "f".repeat(40),
        item_count: 3,
        conflicts: Vec::new(),
        mods: vec![
            ModChange {
                kind: ModChangeKind::Added,
                content_changed: false,
                before: None,
                after: Some(file("New Mod", "mods/new.jar")),
            },
            ModChange {
                kind: ModChangeKind::Removed,
                content_changed: false,
                before: Some(file("Removed Mod", "mods/removed.jar")),
                after: None,
            },
            ModChange {
                kind: ModChangeKind::Changed,
                content_changed: true,
                before: Some(file("Updated Mod", "mods/v1.jar")),
                after: Some(file("Updated Mod", "mods/v2.jar")),
            },
        ],
    }
}

fn frame(app: &mut GuiApp, ctx: &egui::Context, events: Vec<Event>) -> egui::FullOutput {
    ctx.run(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 900.0),
            )),
            events,
            ..Default::default()
        },
        |ctx| app.render(ctx),
    )
}

fn labels(output: &egui::FullOutput) -> Vec<(String, egui::Pos2)> {
    output
        .shapes
        .iter()
        .filter_map(|shape| {
            if let egui::Shape::Text(text) = &shape.shape {
                Some((
                    text.galley.job.text.clone(),
                    text.pos + text.galley.size() / 2.0,
                ))
            } else {
                None
            }
        })
        .collect()
}

fn click(app: &mut GuiApp, ctx: &egui::Context, label: &str) {
    let output = frame(app, ctx, Vec::new());
    let position = labels(&output)
        .into_iter()
        .find(|(text, _)| text == label)
        .unwrap_or_else(|| panic!("missing visible control: {label}"))
        .1;
    for pressed in [true, false] {
        frame(
            app,
            ctx,
            vec![
                Event::PointerMoved(position),
                Event::PointerButton {
                    pos: position,
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
    }
}

#[test]
fn normal_window_hides_technical_fields_and_conflict_rules() {
    let mut app = app();
    let ctx = egui::Context::default();
    let output = frame(&mut app, &ctx, Vec::new());
    let text = labels(&output)
        .into_iter()
        .map(|(text, _)| text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("当前版本：installed"));
    assert!(text.contains("开始更新"));
    assert!(!text.contains("GitHub 令牌"));
    assert!(!text.contains("bkmpw 路径"));
    assert!(!text.contains("PATH="));
}

#[test]
fn selecting_release_clears_previous_conflict_decisions() {
    let mut app = app();
    app.pinned_target = Some((app.repo.clone(), "new".into(), "f".repeat(40)));
    app.preview = Some(preview());
    app.conflicts.receive(vec!["config/game.toml".into()]);
    app.conflicts
        .resolutions
        .insert("config/game.toml".into(), ConflictResolution::KeepLocal);
    let ctx = egui::Context::default();
    frame(&mut app, &ctx, Vec::new());
    click(&mut app, &ctx, "new");
    click(&mut app, &ctx, "old");
    assert_eq!(app.target, "old");
    assert!(app.pinned_target.is_none());
    assert!(app.preview.is_none());
    assert!(app.conflicts.paths.is_empty());
    assert!(app.conflicts.resolutions.is_empty());
}

#[test]
fn conflict_buttons_build_structured_choices_for_pinned_commit() {
    let mut app = app();
    let (sender, receiver) = std::sync::mpsc::channel();
    app.receiver = Some(receiver);
    app.running = true;
    sender
        .send(Message::Conflicts {
            commit: "f".repeat(40),
            paths: vec!["config/game=custom.toml".into(), "mods/custom.jar".into()],
            details: "fixture conflict".into(),
            repo: app.repo.clone(),
            target: app.target.clone(),
        })
        .unwrap();
    let ctx = egui::Context::default();
    frame(&mut app, &ctx, Vec::new());
    assert!(!app.conflicts.ready());
    click(&mut app, &ctx, "使用所选版本");
    assert!(matches!(
        app.conflicts.resolutions["config/game=custom.toml"],
        ConflictResolution::UseTarget
    ));
    click(&mut app, &ctx, "继续更新");
    assert!(!app.running, "unresolved conflicts must disable updating");
    click(&mut app, &ctx, "全部保留我的修改");
    assert!(app.conflicts.ready());
    let args = app.remote_args().unwrap();
    assert_eq!(args.target, "f".repeat(40));
    assert!(matches!(args.conflicts, super::ConflictChoice::Stop));
    assert!(matches!(
        args.resolutions["config/game=custom.toml"],
        ConflictResolution::KeepLocal
    ));
    assert!(matches!(
        args.resolutions["mods/custom.jar"],
        ConflictResolution::KeepLocal
    ));
}

#[test]
fn saved_side_and_pending_transaction_disable_unsafe_actions() {
    let mut app = app();
    app.saved_side = Some(Side::Client);
    let ctx = egui::Context::default();
    click(&mut app, &ctx, "服务端");
    assert_eq!(app.side, Side::Client);
    app.pending = true;
    click(&mut app, &ctx, "开始更新");
    assert!(!app.running);
    let output = frame(&mut app, &ctx, Vec::new());
    assert!(
        labels(&output)
            .iter()
            .any(|(text, _)| text == "恢复更新前的文件")
    );
}

#[test]
fn completed_preview_shows_mod_changes_and_pins_the_update_to_viewed_commit() {
    let mut app = app();
    app.operation = super::Operation::Preview;
    app.running = true;
    let (sender, receiver) = std::sync::mpsc::channel();
    app.receiver = Some(receiver);
    sender
        .send(Message::Preview {
            summary: preview(),
            repo: app.repo.clone(),
            target: app.target.clone(),
        })
        .unwrap();
    sender
        .send(Message::Complete(Ok("检查完成".into())))
        .unwrap();
    drop(sender);
    let ctx = egui::Context::default();
    let output = frame(&mut app, &ctx, Vec::new());
    assert!(!app.running);
    assert!(!app.failed);
    assert_eq!(app.remote_args().unwrap().target, "f".repeat(40));
    let text = labels(&output)
        .into_iter()
        .map(|(text, _)| text)
        .collect::<Vec<_>>()
        .join("\n");
    for expected in [
        "模组变更清单",
        "新增 1",
        "移除 1",
        "变更 1",
        "New Mod",
        "Removed Mod",
        "mods/v1.jar",
        "mods/v2.jar",
    ] {
        assert!(
            text.contains(expected),
            "missing preview text: {expected}\n{text}"
        );
    }
}

#[test]
fn empty_preview_is_explicit_and_side_changes_clear_it() {
    let mut app = app();
    let mut summary = preview();
    summary.mods.clear();
    app.preview = Some(summary);
    let ctx = egui::Context::default();
    let output = frame(&mut app, &ctx, Vec::new());
    assert!(
        labels(&output)
            .iter()
            .any(|(text, _)| text == "这两个发布版本没有模组变更。")
    );
    click(&mut app, &ctx, "服务端");
    assert!(app.preview.is_none());
}

#[test]
fn same_filename_changes_and_optional_defaults_are_visible() {
    let mut summary = preview();
    let changed = summary.mods.last_mut().unwrap();
    changed.after.as_mut().unwrap().path = "mods/v1.jar".into();
    changed.after.as_mut().unwrap().optional = true;
    changed.after.as_mut().unwrap().default_enabled = false;
    summary.mods = vec![summary.mods.pop().unwrap()];
    let mut app = app();
    app.preview = Some(summary);
    let ctx = egui::Context::default();
    let output = frame(&mut app, &ctx, Vec::new());
    let text = labels(&output)
        .into_iter()
        .map(|(text, _)| text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("文件名相同，校验值已变化。"));
    assert!(text.contains("可选，默认关闭"));
}
