# Codex 分阶段实施计划

## M0 工程骨架与配置
+ [x] 创建 Cargo workspace：backup-core、backup-storage、backup-local、backup-drive、agent、desktop。
+ [x] serde 数据类型、校验、JSON 加载保存（写入临时文件 + 原子替换）及配置迁移接口。
+ [x] 使用 `config/work-documents.json` 作模板而不是自动开启真实目录。
+ [x] 单元测试：配置 roundtrip、枚举错误、排除规则和路径嵌套校验。
**完成条件：** 配置解析/验证在 Windows CI 通过，无真实数据触碰。

## M1 本地备份可靠内核
- [x] SQLite 初始化/迁移；文件索引、源修订、任务队列与目标提交状态。
- [x] 目录流式扫描、globset、大小写路径 key、symlink/junction 防护。
- [x] notify 防抖 2s、启动 reconcile、60min reconcile，监听失败时标记扫描。
- [x] 受控快照 + 原子目标替换 + BLAKE3，失败保持旧有效备份。
- [x] 处理创建、修改、移动、删除（默认不传播）及源盘失联。
**完成条件：** 本地增量可运行，断电模拟/进程终止恢复、异常保护测试通过。

**M1 交付记录：** `backup-storage` 嵌入并版本化执行 SQLite schema，按内容 hash 递增源修订，独立记录目标提交与失败；进程重启时将 running 任务恢复为 retry/reconcile 状态。`backup-core` 提供流式扫描与大小写归一化 key。`backup-local` 提供 `backup_once` 和可关闭的实时/周期调度器；源文件快照与 hash 校验通过后才以同卷临时文件原子替换。源缺失不删除目标，排除规则变化不记作删除；一次发现至少 10 个且占跟踪文件一半以上的缺失会要求复核。双目标任务当前只执行本地分支并返回 `partial_success`，Google Drive 留待 M3。测试使用隔离临时目录，备份目标根目录必须已存在，不由内核自动创建。

## M2 7 天历史与恢复
- [x] 历史对象仓库、manifest 和引用关系；写入完整后才记为 recoverable。
- [x] 最新有效版本永保留，其他版本到期后做引用检查 GC。
- [x] 恢复到新目录为默认；覆盖恢复需显式确认；记录恢复审计。
**完成条件：** 能恢复不同历史版本，GC 不误删最新/引用中对象。

**M2 交付记录：** SQLite 从 schema v1 迁移到 v2，新增内容寻址历史对象、逐文件版本 manifest、对象引用和恢复审计。`backup-local` 在本地镜像提交前持久化并校验源快照；变更时先校验并归档旧镜像。仓库位于目标目录旁的 `.rustbackup/<task-id>`，扫描会剪枝整个托管仓库。保留期按任务配置执行，最新版本及仍被其他版本引用的对象受保护；运行中的恢复会阻止 GC。恢复默认要求一个不存在的新目录，写入完成后再原子发布；已有目录仅在调用方显式确认时才替换同路径文件。M2 暂不含 UI 与 Drive 历史，分别留待 M4、M3。

## M3 Google Drive 单向目标
- [x] Google OAuth PKCE、账号标识和安全凭据；令牌生命周期管理。
- [x] Drive 专属目录创建与稳定 folder ID / file ID 映射。
- [x] Upload（可续传）、网络/配额重试、会话恢复/失效重建、重复上传对账。
- [x] 云端历史对象与 manifest（与本地备份独立提交）；云端恢复下载。
- [x] 未授权时本地分支照常运行，整体状态 partial/waiting_auth。
**完成条件：** 测试账号 + 测试专用目录验证上传、更新、重试与恢复；无权限越界。

**M3 交付记录：** `backup-drive` 实现桌面 OAuth authorization code + PKCE loopback、账号身份查询、刷新令牌与上传会话 URI 的 Windows Credential Manager 存储，以及受 `drive.file` scope 限制的 Drive v3 客户端。目录和文件以私有 `appProperties` 标记并按 ID 复用；上传通过 resumable session 分块，配额/网络退避、重启续传、失效重建及不确定完成对账均覆盖本机模拟服务测试。`backup-local::backup_once_with_drive` 先完成本地分支，再独立提交 Drive 目标状态；更新前把旧云端文件复制到任务历史目录，cloud manifest 存在 SQLite v3 的独立表中。云端恢复先下载并校验 BLAKE3，再将完整目录原子发布到新的目标目录并记录审计。未授权时返回 `waiting_auth`，保留本地成果。所有自动测试使用临时文件和 loopback 模拟 API；按项目路径安全要求未连接真实 Google 账号，因此 M3 的测试账号/专用目录验收条件仍待人工完成，不能据此标记整项 M3 验收完成。

## M4 Agent + Windows GUI
- [ ] 独立 Agent 登录后自启动、单实例、用户态；安全 Named Pipes IPC。
- [ ] Tauri 2 UI：任务列表、添加/编辑、授权、查看进度、暂停/恢复、历史/恢复、日志。
- [ ] 系统托盘、关闭窗口继续后台、Windows 安装包。
**完成条件：** UI 退出和重启后后台队列持续，安装卸载及凭据处理安全。

## M5 稳定化与发布
- [ ] 集成测试：网络中断、文件并发写入、外接磁盘消失、海量文件、长路径、权限拒绝。
- [ ] CI：fmt/clippy/tests，Windows 构建、打包、基础性能基准；README 操作说明。
- [ ] 明确 V0.1 不包含 `bidirectional`，运行时拒绝。
**完成条件：** `docs/TEST_AND_ACCEPTANCE.md` 全部 P0 通过，交付源码和签名策略说明（未签名需明确）。

## 每阶段汇报模板
1. 实际实现文件及重要设计决定；2. 执行命令与结果；3. 未通过测试/已知限制；4. 下一阶段依赖；5. 是否触碰任何真实文件（默认必须为否）。
