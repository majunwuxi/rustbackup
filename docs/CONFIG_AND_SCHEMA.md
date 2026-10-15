# 配置契约及 SQLite 设计

## JSON 契约
规范样例：`config/work-documents.json`。`schema_version=1`；路径字符串为 Windows 原样路径，使用 serde `PathBuf`；`mode` 枚举 `local|cloud|dual|bidirectional`；`schedule.type` 为 `realtime|hourly|daily|manual`。

配置对象：`AppConfig { schema_version:u32, application:ApplicationConfig, tasks:Vec<BackupTask> }`；`BackupTask { id:String, name:String, enabled:bool, source:SourceConfig, mode:BackupMode, schedule:ScheduleConfig, destinations:DestinationsConfig, versioning:VersioningConfig, filters:FilterConfig, safety:SafetyConfig }`。
- `SourceConfig { path:PathBuf, recursive:bool, follow_symlinks:bool }`
- `ScheduleConfig { #[serde(rename="type")] schedule_type:ScheduleType, debounce_ms:u64, reconcile_interval_minutes:u32, scan_on_startup:bool }`
- `DestinationsConfig { local:LocalDestination, google_drive:GoogleDriveDestination }`
- `LocalDestination { enabled:bool, path:PathBuf }`
- `GoogleDriveDestination { enabled:bool, account_id:Option<String>, folder_id:Option<String>, folder_name:String }`
- `VersioningConfig { enabled:bool, retention_days:u32, keep_latest:bool }`
- `FilterConfig { exclude:Vec<String>, case_insensitive:bool }`
- `SafetyConfig { propagate_deletions:bool, preserve_deleted_files:bool, verify_before_commit:bool, on_source_unavailable:SourceUnavailablePolicy, on_permission_denied:PermissionDeniedPolicy }`。策略使用 serde snake_case enum，严禁任意字符串。

## 校验
- 任务 ID 唯一、合法；versioning enabled 时 retention_days >=1；max_parallel_transfers 在可控范围。
- 目标同源或嵌套关系、相同物理文件夹、无法规范化路径、符号链接/重解析点绕过校验均应拒绝或停止任务。
- `dual` 必须启用两个目标，云端未授权时状态 `waiting_auth`，本地分支仍执行。未授权不能把 whole task 计为 complete。
- `bidirectional` 首版明确返回 UnsupportedMode。
- `*.tmp` 和 `~$*` 匹配每层文件名；`.git` 和 `node_modules` 为目录组件精确匹配并剪枝，case insensitive；规则改变只重新扫描，不自动清理旧备份。

## SQLite 迁移
见 `sql/001_init.sql`；正式代码使用嵌入式版本化迁移。关键不变量：
- `files` 唯一 `(task_id,relative_path_key)`；key 需规范化 Windows 不区分大小写，保留 display path。
- `file_versions` 可恢复状态和存储引用；保留最新有效版本。
- `transfer_jobs` 每个 `(task_id, relative_path_key, source_revision, destination)` 应幂等；旧 revision 不覆盖新 revision。
- `destination_states` 各目标拥有自己提交的 revision 与远端 ID；禁止使用整任务一个 bool 表示已备份。
- `backup_runs` 记录扫描/备份统计和错误，时间统一 UTC。
- 状态变更依托事务；启动时 running 恢复为需对账/重试，不得直接 succeeded。

## 状态机
任务：`idle,scanning,backing_up,partial_success,waiting_auth,paused,error`。
队列：`pending -> running -> succeeded`；`running -> retry_wait -> running`；终止 `failed`；崩溃恢复优先 reconcile。

## 设置路径
- `%APPDATA%\RustBackup\config.json`
- `%LOCALAPPDATA%\RustBackup\state.db`
- 托管历史仓库示例 `%USERPROFILE%\RustBackup\Backups\.rustbackup\work-documents\...`，云端另建隐含命名空间并以 file ID 管理。

## C 盘优先与首次引导（强制实现）
- 默认 `source.path` 为空字符串；仅当 `enabled=false` 时允许此状态。启用前必须弹出源文件夹选择器并验证存在、为目录、可读取。空值不得解析为 `.`、当前工作目录或 `C:\`。
- 默认 `local.path="%USERPROFILE%\\RustBackup\\Backups\\Work"`（JSON 内反斜线需转义）；Windows `%USERPROFILE%` 不会被 `PathBuf` 自动展开，必须在应用自定义路径解析层明确展开，仅允许受支持的变量并拒绝未解析变量。
- 源目录可以在任何盘符；绝不能假设有 D 盘、E 盘。目标仅在用户确认任务和验证无嵌套关系后创建。路径检查须处理 Windows junction / reparse point，优先获取真实路径并比较。
- C 盘上源与目标共处同一物理盘时，在 UI 标出“同盘备份不防磁盘故障”；无 Google 授权时进一步提示缺少异地副本。
- 未设置源目录时，任务保持 `needs_setup`（UI/校验层状态），不可启动 watcher/scan/upload。
