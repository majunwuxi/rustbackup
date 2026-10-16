# 测试矩阵 / P0 验收

## 环境与安全
所有集成测试仅用 `%TEMP%\RustBackupTests\<random-id>`，模拟源目录/本地目标不得触及用户真实路径。Google 测试只可使用用户明确授权的专用测试目录，测试清理必须先核验其应用标记与 folder ID。

Windows 上进行真实 Drive API 冒烟验收：`cargo run -p rustbackup-desktop --example live_drive_smoke -- --confirm-live-drive-writes`。该命令仅读取 RustBackup 配置中的 Google 账号标识和 Windows 凭据管理器中的 OAuth 客户端密钥；源文件、镜像、SQLite 状态和恢复结果均创建在 `%TEMP%\RustBackupTests\<random-id>`。它会在应用专属 `RustBackup` 云端根目录下创建唯一测试任务目录并上传虚构文件；测试文件保留在该目录供检查，不会触碰默认任务、其他 Drive 文件或真实本地目录。运行前必须确认当前 OAuth 构建标记为测试环境。

## 核心 P0
- [ ] 默认任务配置加载、保存、再加载一致；无密钥明文。
- [ ] 初始扫描和全量 reconcile 正确找出新增/更新；重启不会重复备份全部文件。
- [ ] `*.tmp`、`~$*`、`.git`、`node_modules`（含深层目录和大小写变体）被排除。
- [ ] 多次事件聚合，只对真正变化内容产生持久任务。
- [ ] 本地写入中断不破坏之前有效备份。
- [ ] 旧 revision 的完成结果不可覆盖新 revision。
- [ ] 源路径不存在、目标不可用、失去权限时安全暂停/重试，不传播删除。
- [ ] 源删除或排除规则变化，不自动删除镜像和历史。
- [x] 固定 7 天到期处理不删除当前有效版本；配置与运行时都不能延长期限，本地历史引用和进行中的恢复受保护。
- [x] 本机模拟 API：Google 授权缺失时本地照常成功，云端标等待授权，整体非成功。
- [x] 本机模拟 API：Drive 配额重试、resumable 续传/失效重建及不确定结果对账；不会重复创建多份同名文件。
- [x] 本机模拟 API：历史副本、云端恢复下载与 BLAKE3 完整性校验；失败不发布/不标成功。
- [x] 本机模拟 API：只清理超过固定 7 天的 manifest 历史副本；当前文件、未到期副本、恢复中的副本以及标记/父目录不匹配的远端文件均保留。
- [x] 2026-10-15 真实 Drive 冒烟验收：专用应用目录中上传虚构文件、重复运行无重复上传、更新时保存旧版本、恢复旧版本并校验内容；临时源/镜像/恢复目录位于 `%TEMP%`。具体目录名和范围见下方记录。
- [ ] 人工验收：验证 Google 授权访问边界；本轮只验证应用创建的测试文件位于其专用任务 folder ID 下，没有读取或修改其他 Drive 文件。
- [ ] 人工验收：在专用测试账号中验证超过 7 天的托管历史副本会被清理，当前文件和 7 天内副本仍保留；不得使用真实备份目录做此验证。
- [x] M4 隔离桌面/Agent 冒烟：Release 桌面端以 `--login-startup` 在独立 `%TEMP%` profile 启动；默认任务关闭且源路径为空。终止 UI 后 Agent 仍为原 PID/在线，重新启动 UI 后复用同一 Agent；测试结束已停止两个进程。
- [x] 2026-10-15 NSIS 隔离安装/卸载冒烟：包安装到唯一 `%TEMP%` 目录，桌面端和 Agent 可运行；卸载后安装目录及 HKCU 卸载登记消失，测试 profile 中配置、SQLite 和日志仍保留，`RustBackupAgent` 登录启动值未被创建。
- [x] 2026-10-15 登录启动/卸载项隔离验收：设置 `start_with_windows=true` 启动桌面端后，HKCU Run 值指向安装目录并携带 `--login-startup`；卸载后启动项和安装目录移除，隔离 profile 配置与 SQLite 保留。
- [x] 2026-10-15 Release 托盘隔离交互验收：使用 `%TEMP%\RustBackup-M4-Tray-Recheck-1ad49b590f044c29a7ada5957e389bc4`，`start_with_windows=false`、`minimize_to_tray=true`、默认任务关闭且源路径为空。点击 X 后窗口隐藏，托盘图标可见且能重新打开；选择托盘菜单“退出界面（后台备份继续）”后 UI 进程退出，`rustbackup-agent.exe --status` 返回 `online (Idle)`、活动任务数为 0。此前托盘句柄被过早释放，现已保存在 Tauri 应用状态中。
- [x] 2026-10-15 Release 登录启动模拟：独立 `%TEMP%` `APPDATA`/`LOCALAPPDATA` profile 中，`start_with_windows=false`、`start_minimized=true`、默认任务关闭且源路径为空。以 `--login-startup` 启动后，通过 Windows 原生窗口枚举确认 RustBackup 主窗口隐藏，Agent 状态为 `Idle` 且活动任务数为 0；普通启动仍显示主窗口。未扫描或同步文件。
- [ ] 人工验收：实际 Windows 登录事件（注销或重启后由 HKCU Run 自动启动）仍待触发验证。手动运行队列的持久化/中断恢复由 SQLite 单测覆盖。
- [x] Agent IPC 协议单元验收：版本校验、仅状态查询命令、帧长度上限及畸形帧拒绝。
- [x] 2026-10-15 Windows Agent IPC 冒烟：同一用户成功查询状态；启动第二实例后其以退出码 0 结束，原实例仍可响应。隔离 Agent 下，本机名 UNC pipe 路径连接被拒绝（Access Denied），匿名 Windows token 连接本地 pipe 被拒绝（Win32 error 5）；测试配置任务关闭、源路径为空，未扫描或同步文件。
- [x] 2026-10-15 M4 隔离 Agent 运行冒烟：Release Agent 使用 `%TEMP%\RustBackup-M4-Agent-269c232e99c345fdb9e9f3743ce8f382` 下临时 APPDATA/LOCALAPPDATA 启动；默认任务保持关闭且源路径为空。状态为 `Idle`、活动任务数为 0；第二实例退出码 0，原实例仍在线；随后已停止测试进程。临时 profile 仅含测试配置、SQLite 状态与日志。
- [ ] Windows 多进程人工验收：使用另一个 Windows 用户账号及另一台主机分别确认 Named Pipe 连接被拒绝。本机名 UNC 路径和匿名 token 拒绝已通过隔离冒烟验证；同用户单实例和状态查询已通过。
- [x] 2026-10-15 M4 Windows 发布构建：`cargo tauri build --bundles nsis` 成功生成 x64 current-user 安装包；构建前钩子会构建并暂存独立 Agent sidecar。
- [x] 2026-10-16 NSIS 更新安装修复：安装前使用 Tauri NSIS 的当前用户进程检查关闭 RustBackup UI 和后台 Agent，避免覆盖运行中的 `rustbackup-agent.exe`；`scripts/package-windows.ps1` 已成功生成 `RustBackup_0.1.2_x64-setup.exe`。
- [ ] 人工验收：在 Agent 正运行时启动 `0.1.2` 安装包，确认安装器提示关闭进程并完成覆盖更新。
- [x] M4 前端静态检查：`node --check apps/desktop/ui/app.js`。
- [ ] 路径嵌套、junction、symlink 和目录循环被安全阻止。
- [ ] `bidirectional` 报 UnsupportedMode，不能误作 dual。

### 真实 Drive 冒烟记录
- 测试云端目录：`RustBackup-Test-1792046873448226400-43176`（位于 RustBackup 应用专属目录下；由应用标记和保存的 task folder ID 管理）。
- 本地隔离根目录：`%TEMP%\RustBackupTests\drive-1792046873448226400-43176`；默认工作资料任务配置未修改。
- 结果：首次上传 1 个文件；相同内容重复运行上传 0 个新文件；修改后上传 1 个新版本并归档旧版本；下载恢复旧版本后字节校验通过。
- 本轮没有制造真实网络中断；配额、网络错误、续传与不确定完成重试仍由本机模拟 API 测试覆盖。七天后云端清理和授权对其他文件的访问边界仍未验收。

## 回归与非功能
- Windows 10/11；NTFS；UTF-8/中文/空格文件名；长路径；仅大小写变化；重命名；空文件/空目录；大文件；大量小文件。
- 性能数据需实测：空闲内存、扫描 10万文件用时、CPU、上传吞吐；不得虚构宣传指标。
- 日志保留、隐私脱敏、诊断导出；SQLite WAL 修复与异常恢复。

## 发布门槛
`cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace` 与 Windows 打包构建全部通过。若 Google Drive 集成需要人工账号授权，必须单独列出未自动化项。
