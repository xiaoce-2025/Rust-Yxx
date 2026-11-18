# 课堂笔记计费基础

处理额度统一存放于 SQLite `note_credit_accounts`。`FileUser.processing_quota` 只保留兼容数据；用户信息和管理后台显示的值从额度表投影。普通用户资料保存接口忽略旧表单携带的 `processing_quota`，只通过独立接口增量发放。

## 计费行为

- 入队不扣款、不冻结额度。直链任务保留原有存储额度检查和存储空间占用。
- Worker 获得 URL 执行资格后，用短写事务检查当前余额，只有余额大于 0 才允许执行。相同 URL 的互斥、优先级和 FIFO 均沿用原队列。
- 不同 URL 可并发通过准入；开始后不因其他任务扣款而停止。成功交付后按实际秒数向上取整小时扣款，允许最终余额为负。
- 缓存命中、跨账户复用、重新生成与普通生成采用同一结算流程。下载、生成或真正安装失败不收费。
- `note_credit_ledger` 的唯一任务索引与事务保证一任务只扣一次。流水、余额和最终成功任务 JSON 在同一事务提交；迟到失败通知不能覆盖已扣费成功记录。

## 部署与升级

1. 停止旧服务，备份整个 `data/` 与 `course_data/`。继续使用原工作目录启动新版，每个数据目录仍只运行一个实例。
2. 运行环境必须可执行 `ffprobe`（通常随 FFmpeg 安装）。新生成任务从下载的本地媒体检测时长；小数秒先向上取整。不能确定正时长时不会默认收费。
3. 第一次启用时，服务使用 SQLite `VACUUM INTO` 创建一致性备份 `data/assistant.pre-note-billing-<uuid>.sqlite3`，随后事务迁移旧用户处理额度，包括负值；`note_billing_migrations` 记录版本和启用时点。重启不会重复注入旧额度。
4. 历史成功任务不补扣；启用之前已生成的恢复快照沿用历史交付流程。之后进入 Worker 的新任务统一执行新计费。
5. 若要回滚，先停服务，完整恢复切换前的数据库及文件备份。上线后的新流水须先保留审计备份；不能只切回旧程序继续写旧额度字段。

## API 与后台

- `GET /api/note-credits/me`：当前认证用户余额及 `can_start_new_job`；响应禁止缓存。
- `GET /api/note-credits/ledger?limit=30&cursor=...`：当前用户使用记录；分页使用响应的 `next_cursor`。
- `POST /api/admin/users/:id/note-credits/grant`：需要管理员身份和 `X-Admin-Request: 1`。JSON 包含正整数 `amount`、非空 `reason`、`idempotency_key`。同一逻辑请求重试必须沿用同一个键；不同内容复用同一个键会被拒绝。
- `GET /api/admin/users/:id/note-credits/ledger`：管理员查看指定用户流水。
- 后台用户详情包含发放表单、真实负余额及最近流水；任务详情展示计费状态、实际秒数、扣款及失败码。
- `/pkuvideo` 与 `/pkuvideo_sequence` 的普通、重新生成、单节、批量按钮使用相同的即时余额检查。批量只检查一次，不因余额为 1 而禁止提交多节课。

## 待恢复结果

`note_job_billing` 独立于队列状态。交付快照保存在用户私有目录；结算成功后才显示成功并开放新结果下载。

- `DURATION_UNAVAILABLE`：尝试读取可信全局缓存时长、保留的本地媒体或通过原准备媒体信息重新探测。检测不到时保留快照和 `BILLING_PENDING`，不扣费、不伪装成功。
- `BILLING_COMMIT_FAILED`：SQLite 写入失败，结果保留为待恢复，不当作 AI 生成失败。
- `BILLING_SYSTEM_UNAVAILABLE`：执行准入时数据库异常，等待重新入队检查，不显示额度不足。
- 服务启动和每 60 秒恢复一次待结算任务；恢复不重新运行 AI，重复恢复不会重复收费。若后续课节已有新版交付，尚未安装的旧快照取消交付且不收费。
- 准备媒体的必要信息保存在私有数据库 sidecar，正常结算后删除；不会出现在用户任务 JSON。

## 验证

```powershell
cargo test --manifest-path assistant-server/Cargo.toml --offline -- --test-threads=1
node --test assistant-server/tests/note-credits.test.cjs
```

Rust 测试覆盖计量边界、并发负余额、失败不扣费、管理员与扣款竞态、幂等发放、事务故障回滚、重启恢复、迁移、Worker 缓存/重新生成计费、缺失时长等待恢复、权限隔离和既有 URL 队列规则。前端测试执行实际页面提交函数，验证低余额、API 异常、快速重复点击和一小时余额提交五节课。


## 直链存储与任务历史

- `/pkuvideo` 通过 `/api/pkuvideo/standalone-jobs` 仅管理直链任务的存储项目，排除课程课节和已删除文件的任务。最终 TeX 位于 `data/user_notes/<用户 ID>/<任务 ID>.tex`；中间文件位于 `data/pkuvideo_jobs/`。课程笔记仍在 `course_data/` 管理。
- 每个账户最多保留 3 个直链任务，排队、运行和尚未清理的终态任务都占名额。入库与存储配额扣减使用同一 SQLite 写事务，课程任务不占这些名额。
- 删除直链任务文件会原子释放一次名额，并记录 `artifacts_deleted_at`。成功、失败与原计费数据保留；不会退款或删除账本。删除后的任务不能继续下载，失败任务删除也会返还其占用的存储名额。
- `/task-history` 是独立的只读查询页面，使用 `/api/video-task-history` 查看当前用户全部笔记队列记录，包括课程、批量子任务、直链、重新生成和已删除直链文件的记录。支持搜索、筛选和分页，不提供下载或删除操作；接口也不返回下载链接及文件路径。
