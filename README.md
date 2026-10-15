# RustBackup / Windows 本地 + Google Drive 双重备份

这是交给 Codex 的**实施规格包**，不是已完成的软件。以轻量、可靠、安全恢复为第一优先级。

## 已确认的默认任务
- 名称：工作资料；ID：`work-documents`
- 源目录：初始未指定（配置为空字符串，首次运行必须让用户选取）；本地目标：`%USERPROFILE%\RustBackup\Backups\Work`
- 模式：`dual`（两路单向备份，不是双向同步）
- 调度：`realtime`，2s 防抖 + 启动全量核对 + 每 60 分钟核对
- 保留历史：固定 7 天，最新有效版本永不按到期自动删除；启用版本控制时配置必须为 7 天
- 排除：`*.tmp`、`~$*`、`.git`、`node_modules`，对 Windows 大小写不敏感
- Google Drive：初始未授权，用户登录后绑定专用目标文件夹；不得在配置中存放令牌
- 安全策略：不传播删除、源目录失联暂停、目标端分别维护成功状态

## 首版边界
支持：本地文件级增量、Google Drive 单向上传、双路独立重试、历史版本、恢复、实时监听、全量校验、Tauri 设置与托盘、用户登录后启动 Agent。
不支持：双向同步/冲突自动解决、块级差分、Windows VSS、云端推送 Webhook、多设备跨端同步、原生 Google Docs 编辑语义、绕过 OS 权限。

## 推荐 Rust Workspace
```
crates/backup-core          配置、扫描、过滤、事件防抖、任务调度
crates/backup-storage       SQLite schema、索引、队列
crates/backup-local         原子写入、快照仓库、恢复
crates/backup-drive         OAuth、Drive API、重试、校验
apps/agent                  单实例用户态进程、IPC、后台任务
apps/desktop                Tauri 2 UI / tray
config/                     默认配置
```

## 如何让 Codex 执行
在仓库根目录启动 Codex，先让它阅读 `AGENTS.md` 和 `docs/IMPLEMENTATION_PLAN.md`，从 M0 开始。每个里程碑先列要改的文件和测试，再写代码、运行测试并汇报。只有用户明确允许时才能对真实路径进行首次试运行。

## 只有 C 盘的 Windows 电脑：首次设置
- **不假设存在 D: 或 E:**。任务源目录在示例 JSON 中故意设为空字符串 `"path": ""`，禁用状态下为合法待配置值。用户必须通过「选择文件夹」指定真实已有目录，软件才允许启用该任务；不得把空路径解释为当前工作目录或整个 C 盘。
- **建议本地目标**：`%USERPROFILE%\RustBackup\Backups\Work`；例如 `C:\Users\Alice\RustBackup\Backups\Work`。应用必须明确解析环境变量后执行规范化和源/目标包含关系校验。目标目录在用户确认后可创建，不能在安装时擅自创建。
- 若源目录选择 `%USERPROFILE%` 或包含上述目标目录，须拒绝或要求更换本地目标，避免备份自我递归。
- 同一 C 盘上的本地副本只能防范部分误删或误改，**不能防范整盘损坏、设备丢失或整盘勒索攻击**；Google Drive 完成授权且上传成功后才提供异地备份。推荐日后接入独立硬盘。
