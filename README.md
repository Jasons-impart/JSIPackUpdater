# JSIPackUpdater

面向 Minecraft 整合包的版本更新器。目前已开始实现 Rust 核心与 CLI；现阶段提供离线、只读的三方文件计划预览。

```bash
cargo run -- preview --base-commit FULL_A_COMMIT_SHA --base-tree /path/to/commit-A-tree \
  --target-tree /path/to/commit-B-tree --instance /path/to/minecraft
cargo run -- preview --base-commit FULL_A_COMMIT_SHA --base-tree /path/to/commit-A-tree \
  --target-tree /path/to/commit-B-tree --instance /path/to/minecraft --format json
```

两个 tree 目录须包含对应 commit 的全部 Git 跟踪普通文件，包括 `pack/pack.toml`。实例须有 `config/createdelight_release_info.json`；若目标更改 Minecraft 或 NeoForge 版本，预览会停止并提示新建实例。预览会读取实例中跟踪路径的实际内容，列出写入、删除、保留本地、可自动合并和冲突；不会修改实例。退出码 2 表示存在需要处理的冲突。GitHub 获取快照、下载产物、事务执行与更新功能尚未接入。

我的理解是：整合包发布包负责初始安装，并通过 `config/createdelight_release_info.json` 提供对应的源 commit、Minecraft 和 NeoForge 版本；更新器负责把**当前已安装版本**同步到用户选择的目标 commit/tag。玩家无需安装 Git；更新器维护本地版本记录，并通过 GitHub 获取版本信息和文件。模组 jar 通常不在 Git 中；更新器根据目标版本的模组描述文件下载、校验和清理对应 jar。

目标形态是共用一个 Rust 更新核心，提供 CLI、TUI 和基于 `egui/eframe` 的简洁 GUI 更新窗口；游戏启动由现有启动器负责。玩家端默认匿名访问公开 GitHub 仓库，遇到 API 限流可输入令牌重试；模组同步直接调用随更新器交付的 `bkmpw` 原生程序，不要求安装 npm。作者发布前用 `devtool set-version` 修改并提交版本信息；玩家更新时以 commit SHA 为准，通过事务记录识别和恢复未完成的更新。Minecraft 或 NeoForge 版本变化时，首版提示新建实例，协助升级留待后续。

**新对话请先读 [实施交接文档](docs/implementation-plan.md)**：包含已定决策、发包契约、冲突处理、首次 `packwiz.json` 建立、落地顺序和验收条件。框架比较与研究依据见 [设计背景](docs/design.md)。

参考整合包：`/home/halo/.local/share/PrismLauncher/instances/CDPR/minecraft`。该路径只用于分析现有工作流，不应成为程序中的固定路径。
