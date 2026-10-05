# Minecraft Modpack Updater

Minecraft 整合包更新器。CLI、TUI 和可选的 egui GUI 共用 Rust 更新核心；支持客户端和服务端。它从安装版本的 commit 更新到指定 tag/commit，同步 Git 跟踪文件及 `bkmpw` 管理的模组、资源包和光影。Minecraft 或 NeoForge 版本变化时会停止更新并提示新建实例。

## 使用

先停止游戏或服务端，并备份整个实例。下载与操作系统相符的更新器压缩包，把其中的 `minecraft-modpack-updater` 和 `bkmpw` 放在同一目录。实例须含 `config/createdelight_release_info.json`。

```bash
# 预览（不会写入实例）
minecraft-modpack-updater preview-remote --instance /path/to/minecraft --target v2.0.0.0-test2

# 更新客户端；服务端改用 --side server
minecraft-modpack-updater update-remote --instance /path/to/minecraft --side client --target v2.0.0.0-test2

# 终端界面：选择检测到的实例与最近发布版本；Tab 补全路径，F2 浏览目录
# 侧别默认客户端；也可继续通过参数预填
minecraft-modpack-updater tui
# GUI（需启用 gui 功能构建）：选实例、选发布版本、点更新
minecraft-modpack-updater gui

# 应用阶段中断后，从已校验的备份回退
minecraft-modpack-updater rollback --instance /path/to/minecraft
```

带 GUI 的构建不传参数时直接打开窗口，也可使用 `gui` 子命令。GUI 会列出检测到的实例，或用系统文件夹选择框选取；选择启动器实例外层目录时，会识别其中的 `minecraft` / `.minecraft`。识别后显示当前整合包、Minecraft 和 NeoForge 版本，并自动读取保存的实例类型（首次根据服务端文件推荐，可自行调整）。发布版本在后台加载，可直接从下拉列表选择。仓库、令牌、工具路径和手动指定 commit 收在高级设置中。

更新默认访问公开仓库 `Jasons-impart/Create-Delight-Project-Rebirth`；可以用 `--repo owner/name` 指定其他仓库。GitHub 限流时可设置 `GITHUB_TOKEN`，或传 `--token-stdin` 从标准输入/隐藏终端输入读取令牌。令牌不会作为命令行参数传递。

`preview` 与 `preview-remote` 展示源码文件冲突；模组产物和生成文件冲突会在更新准备阶段检查。

无冲突时直接更新。有冲突时，默认停止并列出路径；CLI 可逐项指定 `--resolve '路径=target'`、`--resolve '路径=local'` 或 `--resolve '路径=manual:/path/to/merged-file'`。也可用 `--conflicts use-target` 或 `--conflicts keep-local` 批量决定；逐项选择优先。TUI 遇冲突可按 `t` 用新版、`l` 保留本地、`m` 输入手工合并文件路径，记录选择后重试。GUI 提供逐项“保留我的修改 / 使用所选版本”按钮，或通过文件选择框指定合并文件，无需输入规则；全部选择只作用于已展示的冲突，准备阶段发现新冲突时仍须确认。手动合并文件由用户在实例外编辑，更新器在准备阶段复制其内容。私有 jar 选择保留本地后不加入受管清单。TUI 和 GUI 提供回退入口；GUI 检测到未完成事务时会显示“恢复更新前的文件”。

更新器把侧别、可信缓存、生成文件基线、覆盖状态及事务备份放在实例内 `.jsipack-updater/`。准备和下载失败不会改动实例；应用中断后需先运行 `rollback`，再重新更新。不要在更新时启动游戏或修改实例文件。

## 开发与验收

```bash
cargo test --all-features
cargo clippy --all-features --all-targets -- -D warnings
cargo build --release --features gui
```

`update-local` 和 `preview`/`preview-managed` 可用本地完整源码快照验证更新；快照目录须对应传入的完整 commit SHA。`update-local` 还需 `--base-commit`、`--target-commit`、`--base-tree`、`--target-tree`、`--instance`、`--side`，并可用 `--bkmpw` 指向协议兼容的本地程序。发布工作流在 Linux、Windows、macOS 构建并打包固定版本的原生 `bkmpw`，且验证其 SHA-256 与进度协议。

实施细节和验收边界见 [实施计划](docs/implementation-plan.md)。
