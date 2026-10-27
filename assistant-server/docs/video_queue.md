# 视频准备与处理队列

教学网和好学 App 都在提交阶段完成鉴权、回放地址解析和密钥获取。处理阶段的输入为 `PreparedMedia { url, enc_key }`，没有账号、密码、Cookie 或登录客户端；`pkuvideo::run_pipeline` 仅接收媒体、输出目录和处理服务配置，方便其他接口复用。播放列表和分片直接请求，只有密钥获取使用登录环境。

## 任务与完成通知

队列元素是可序列化的 `VideoTask` 数据，包含 `id`、`user_id`、`priority`、`submitted_at`、`media` 和 `completion`。`completion` 描述后续处理目标，例如课程和课节 ID。待处理队列不保存 Future、闭包、登录客户端或结果接收器。

提交端保存任务记录并调用 `VideoQueue::submit`，获得任务 ID 后立即结束；不会创建等待结果的后台任务，也不持有 `oneshot::Receiver`。视频处理由固定数量的消费者执行，完成后发送 `CompletionEvent` 到有界通道。一个固定的完成事件消费者调用对应的后续处理：课节任务安装笔记、更新课节状态和清理视频中间资源。处理器或后续处理发生错误时，任务 ID 对应的记录保存失败原因；通知处理器的异常不会阻断后续事件。

任务记录在完成后保留，支持按原任务 ID 查询最终结果。`postprocessing: true` 表示视频处理已完成、课节笔记正在保存，整体任务此时仍是 `running`；后续操作成功后才变为 `succeeded`。课节任务的结果路径在 `course_notes_path` 中。排队和处理中的任务不能删除；已完成任务可以删除记录，课节笔记继续保留。

## 配置

`config/development.toml`：

```toml
[pkuvideo]
processing_concurrency = 3 # 整个视频处理流水线的同时运行上限
m3u8_concurrency = 4      # 每个视频同时下载的分片数
m3u8_segment_timeout_seconds = 120
```

账号文件 `data/users.json` 中每个账号可添加 `"processing_priority": 5`。省略时为 5，数字越小越优先。配置在服务启动时加载，修改文件后重启生效。优先级在入队时确定；同优先级按成功入队顺序排列，已开始的任务不会被抢占。并发配置为 0 时按 1 执行。

## 课节密钥

`course_data/<账号>/<课程>/index.json` 的每个课节新增 `"enc.key"`（读取时也兼容 `enc_key`）。旧数据没有此字段仍能读取。值可以是原有的 16 字节 UTF-8 文本，或者 `hex:` 加 32 个十六进制字符；自动获取统一保存为后者，以兼容二进制 AES 密钥。此字段保存的是密钥内容，不是密钥 URL。

若课节已有链接和密钥，提交时不登录教学网或好学 App。否则先通过对应来源获取，成功后把链接与密钥保存回课节，再提交队列。密钥 URL 从 `EXT-X-KEY` 解析，不要求文件名为 `enc.key`，支持记录中的 `/taskflow/harpocrates/rose/...`。教学网鉴权失效时最多重新登录一次，并重做观看页、一次性跳转、视频 API 和密钥获取整个过程。

## 接口与状态

- 单课节 `POST .../courses/:course_id/videos/:video_id/process` 成功返回 `job_id`（兼容字段 `task_id`）、`status: queued` 和 `status_url`。准备尚未完成或密钥获取失败时不会入队。
- `POST /api/pkuvideofullclass/batch-submit` 接收 `course_id` 和 `video_ids`，逐个准备并提交，返回每个课节的结果；部分失败不撤回已入队任务。
- 统一任务查询 `GET /api/pkuvideo/jobs/:id`，需登录且只能查询本账号任务。返回任务 ID、优先级、创建/更新时间、`status`（`pending/running/succeeded/failed`）、`postprocessing`、步骤、结果路径及错误。前端按返回的任务 ID 轮询此接口，断开页面不会影响队列。
- 课节列表通过 `processing_tasks` 返回每个活动课节的 `job_id` 和 `lesson_id`，供刷新页面后恢复轮询；仍保留 `processing_video_ids` 和 `processing_video_id` 兼容字段。
- 单课节 `process-status` 按课程和课节查找任务，返回 `queued`、`processing`、`completed`、`failed` 或 `idle`；队列状态含账号优先级。
- 同一课节不能重复入队；同一账号可连续提交不同课节。任务失败后可重新提交，失败记录和资源保留用于排查。刷新、删除课节等改变课程存储的操作在该课程有任务时暂不执行。

队列目前在进程内运行。服务重启会把原有等待中或运行中的任务标记为失败，提示重新提交，避免永久占用状态；已保存的课节链接及密钥可复用。排队没有一小时超时删除。成功后安装课节笔记再清理该任务资源，处理目录使用提交时记录的固定路径，排队时间不会改变输出位置。

## 本次交互记录的兼容点

登录流程先访问教学网入口，再向 IAAA `oauth.jsp` POST `appID/appName/redirectUrl`，沿用同一个 Cookie jar 和 User-Agent 提交 OAuth。补齐空的 `randCode/smsCode/otpCode` 和浏览器的 Origin、Referer；HTTP 回调跟随到 HTTPS，不复用一次性 token 尝试第二个回调。回放 iframe 使用 HTML 解析器读取，兼容属性顺序、单引号和 `&amp;`，URL 查询参数先解码再编码。

抓包仅用作离线协议参考，没有把真实密码、token、Cookie 或原始 HAR 写入仓库，也没有使用记录中的凭证发起真实登录。验证码、短信验证及上游故障仍需要按服务端提示处理；离线测试不能保证外部服务始终登录成功。
