# 架构与正确性要求

## 进程划分
- `rustbackup-agent.exe`：登录用户上下文运行，唯一实例，执行扫描、监视、SQLite 队列、本地/云端备份；主 UI 退出不影响运行。
- `rustbackup.exe`：Tauri 2 操作界面，使用限制当前用户访问的 Windows Named Pipes 与 Agent 通信。协议版本化、长度上限、请求验证，严禁随意执行命令。
- 两端接口模型独立，`Destination::Local` 和 `Destination::Drive` 分别拥有 committed revision/error/status。

## 端到端流程
1. 验证源/目标及重解析点，不允许同一路径或互相包含；权限不足应报错。
2. 首次全盘扫描（目录排除剪枝）；记录元数据与必要内容 BLAKE3。使用稳定相对路径映射。
3. `notify` 监听递归变化（只当作提示），按路径去抖 2000ms，必要时重扫受影响路径。
4. 每 60min 与启动/休眠唤醒/设备重新出现时补偿扫描。监听溢出必须标记 full reconciliation。
5. 快照源文件内容到受控临时文件；完成写入后再次检查源快照前后元数据，不稳定即重试。无法保证一致性的打开文件标记失败并重试；首版不宣称支持 VSS。
6. 对每个目标创建独立持久任务；对应 source_revision（单调递增/唯一），较旧任务不可覆盖较新提交。
7. 本地先写同卷临时目标，flush/sync、内容校验，再替换为目标文件；替换前旧内容必须先安全写入历史仓库。尽可能原子提交，平台不支持时明确恢复步骤。
8. Drive 用 resumable upload；持久化会话/过期重建；结果不确定先查询，不能盲目重复创建；根据 Google file ID 跟踪文件。
9. 两目标分别验证并记录提交，单路故障为 partial_success，不影响另一路继续。

## 删除语义与可恢复性
- `propagate_deletions=false`：源文件删除时目标当前版本仍保留，记录 source_missing；不能将目标端视为过期删除。
- `keep_latest=true`：保留每条文件路径最后一个有效版本，即使超 7 天；历史超过 7 天者仅在非最新、无引用且无进行中恢复时才可 GC。
- 当前镜像与历史仓库物理分离，恢复可选另存为，不默认覆盖源文件。
- 排除项变更不触发删除；目标临时目录及自身备份仓库不能被源监听递归卷入。
- Google 版本仓库应由本软件自行管理；不能假设 Drive 原生修订版本永久存在。对云端历史先实现经独立对象/manifest 保存的普通文件副本，并在成功后提交索引；不支持的能力不得标记已实现。
- 盘符被拔除、D 盘不存在或权限失败时暂停任务，禁止把整个目录误判删除。

## Google Drive 注意事项
- Desktop OAuth 2.0 Authorization Code + PKCE，loopback redirect，使用系统浏览器；refresh token 保存在 Windows Credential Manager/DPAPI；禁止日志泄露。
- 优先受限 `drive.file` scope。首版由应用自己创建专用目录，不保证可浏览任意现有 Drive 目录。
- Drive 允许同名文件，必须持久化 ID；目录路径到 ID 映射防重复；首次执行前校验绑定账号与文件夹。
- 支持指数退避、Retry-After、配额/403/429/5xx 分类；离线入队；使用有效期内 resumable session，过期重建。
- 上传完整性比较：本地 BLAKE3 仅作本地索引；可使用 Drive 提供的适用校验元数据（例如二进制文件的 md5Checksum）或下载验证。不能把 BLAKE3 与 Drive MD5 直接比较。云端未可验证则报告校验范围而非假定强验证。
- 对未完成或失败上传保留旧云端有效版本；任务重试应具备幂等控制或操作后对账。

## 安全与效率
- 配置可读写，原子写配置文件，并保留 schema version；状态 SQLite WAL 模式、外键、事务。
- 扫描流式遍历、哈希流式计算，不一次装载大型文件；限制同时传输数 2；吞吐与 CPU 占用在实测中调整。
- 禁止跟随 symlink/junction（含祖先路径规范化检查），避免循环或越界读取；Windows 长路径与大小写冲突应测试。
- 恢复、历史清理、覆盖均须有明确 UI 操作与审计事件。
