# RustBackup — Codex 开发指令

## 项目目标
开发 Windows 10/11 轻量备份软件：Rust 核心 + Tauri 2 UI + 独立用户态 Agent + SQLite + Google Drive API v3。首个默认任务为 用户在 UI 中选择的源目录 → `%USERPROFILE%\RustBackup\Backups\Work` 和 Google Drive 的双重**单向**备份（dual），支持实时监听、增量备份、7 天历史版本与恢复。

## 必读顺序
1. `README.md` — 范围与里程碑
2. `docs/ARCHITECTURE.md` — 架构和正确性约束
3. `docs/CONFIG_AND_SCHEMA.md` — 配置、数据库与状态机
4. `docs/IMPLEMENTATION_PLAN.md` — 按阶段执行的任务清单
5. `docs/TEST_AND_ACCEPTANCE.md` — 测试、验收
6. `config/work-documents.json` — 默认配置

## 执行纪律
- 严格按里程碑逐步交付可编译、可测试代码，每阶段运行 `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`；前端加入后运行其静态检查与构建。
- 修改后说明文件、命令、测试结果及未完成事项；不要虚构测试结果。
- **不要**在开发或测试时读取、清理、覆盖真实 用户实际选择的源目录或目标目录。测试仅使用临时目录；默认任务需用户手动启用、确认扫描范围。
- 不得把双向同步当作 dual。`bidirectional` 是预留枚举，首版执行时明确拒绝；绝不隐式删除源或目标文件。
- 未接入 Google 授权时本地备份可独立运行；整项任务不得标记为全面成功。
- 不能因监听事件、源盘失联或排除规则变化自动判定大批删除；禁止静默跳过错误。
- 所有外部输入（路径、配置、API 响应）必须验证。认证凭据不得明文存储在 JSON/SQLite/日志。
- 不擅自增加云端服务器、订阅服务、管理员权限依赖。

## 路径迁移特别要求
用户电脑仅有 C 盘。示例配置源目录为空（必须用户主动选择），本地目标使用 `%USERPROFILE%\RustBackup\Backups\Work`。不得在代码、测试、界面中假定 D/E 盘存在；不得将空源目录解析为当前目录。先验证并确认，再创建目标目录。同盘备份风险须明确提示。
