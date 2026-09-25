# JSIPackUpdater

Minecraft 整合包更新器。CLI、TUI 和可选的 egui GUI 共用 Rust 更新核心；支持客户端和服务端。它从安装版本的 commit 更新到指定 tag/commit，同步 Git 跟踪文件及 `bkmpw` 管理的模组、资源包和光影。Minecraft 或 NeoForge 版本变化时会停止更新并提示新建实例。

## 使用

先停止游戏或服务端，并备份整个实例。下载与操作系统相符的更新器压缩包，把其中的 `jsipack-updater` 和 `bkmpw` 放在同一目录。实例须含 `config/createdelight_release_info.json`。

```bash
# 预览（不会写入实例）
jsipack-updater preview-remote --instance /path/to/minecraft --target v2.0.0.0-test2

# 更新客户端；服务端改用 --side server
jsipack-updater update-remote --instance /path/to/minecraft --side client --target v2.0.0.0-test2

# 终端界面启动后填写实例和目标，侧别默认客户端；也可继续传入这些参数
jsipack-updater tui
# GUI（GUI 版本需启用 gui 功能构建），实例目录可用系统文件夹选择框选取
jsipack-updater gui

# 应用阶段中断后，从已校验的备份回退
jsipack-updater rollback --instance /path/to/minecraft
```

更新默认访问公开仓库 `Jasons-impart/Create-Delight-Project-Rebirth`；可以用 `--repo owner/name` 指定其他仓库。GitHub 限流时可设置 `GITHUB_TOKEN`，或传 `--token-stdin` 从标准输入/隐藏终端输入读取令牌。令牌不会作为命令行参数传递。

`preview` 与 `preview-remote` 展示源码文件冲突；模组产物和生成文件冲突会在更新准备阶段检查。

无冲突时直接更新。有冲突时，默认停止并列出路径；可逐项指定 `--resolve '路径=target'`、`--resolve '路径=local'` 或 `--resolve '路径=manual:/path/to/merged-file'`。也可用 `--conflicts use-target` 或 `--conflicts keep-local` 批量决定；逐项选择优先。TUI 遇冲突可按 `t` 用新版、`l` 保留本地、`m` 输入手工合并文件路径，记录选择后重试。手动合并文件由用户在实例外编辑，更新器在准备阶段复制其内容。私有 jar 选择保留本地后不加入受管清单。TUI 和 GUI 提供回退入口；GUI 可逐行输入相同的冲突选择。

更新器把侧别、可信缓存、生成文件基线、覆盖状态及事务备份放在实例内 `.jsipack-updater/`。准备和下载失败不会改动实例；应用中断后需先运行 `rollback`，再重新更新。不要在更新时启动游戏或修改实例文件。

## 开发与验收

```bash
cargo test --all-features
cargo clippy --all-features --all-targets -- -D warnings
cargo build --release --features gui
```

`update-local` 和 `preview`/`preview-managed` 可用本地完整源码快照验证更新；快照目录须对应传入的完整 commit SHA。`update-local` 还需 `--base-commit`、`--target-commit`、`--base-tree`、`--target-tree`、`--instance`、`--side`，并可用 `--bkmpw` 指向协议兼容的本地程序。发布工作流在 Linux、Windows、macOS 构建并打包固定版本的原生 `bkmpw`，且验证其 SHA-256 与进度协议。

实施细节和验收边界见 [实施计划](docs/implementation-plan.md)。
