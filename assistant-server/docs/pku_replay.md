# 手动课节与原生 Rust 好学 App 客户端

认证、请求签名、App 会话、回放查询及媒体代理均由 Rust 实现，不需要 Python、Conda、Playwright 或脚本子进程。实现依据为用户提供的 `pku_replay_web.zip`，逐项保留其 HTTP 请求顺序、应用配置、Cookie 上下文和签名参数。

## 运行

从 `assistant-server` 目录运行 `cargo run`。媒体合并和原有音频／讲义流程仍使用项目原有的 `ffmpeg` 等工具，因此 `ffmpeg` 需要安装并加入 PATH。部署时保留 `config/pku_replay_semesters.json`；没有额外的 Python 文件需要部署。

## 认证

每个平台用户持有独立的 Rust HTTP 客户端和内存 CookieJar。初始化时从现有 `user_agents.txt`／`user_agents.txt.gz` 池随机抽取一个 UA，登录、课程 API、播放列表、分片和密钥请求始终使用这个值。

1. GET Passport 的 `/auth/login?redirect=https://passport.pku.edu.cn/auth/app-login`，读取并校验官方隐藏表单。
2. POST IAAA `oauth.jsp`，保留 Cookie；应用必须为 `resource`，回调必须为 `https://passport.pku.edu.cn`。
3. 检查图形验证码及短信／邮件／OTP 策略；用户明确点击发送按钮后才调用验证码发送接口。
4. 每次提交登录现场 GET `getPublicKey.do`，使用 RSA PKCS#1 v1.5 加密共享的 IAAA 绑定密码并 Base64 编码，POST `oauthlogin.do`。
5. 使用同一 CookieJar 请求 Passport 票据回调，按 ZIP 的规则最多跟随四次页面跳转；必要时再 GET `/auth/app-login`。
6. 安全读取响应中的 JSON、`bridge.PKULoginSuccess(...)` 字面量、JSON.parse／stringify 包装或赋值变量，取得 App `_token` 和媒体 `_cookieToken`。不执行网页 JavaScript。

好学认证直接复用 IAAA 绑定密码，不另设密码输入。新绑定的共享凭据与 App UserInfo 会话使用 RSA-OAEP-SHA256 加密保存在 data/app_replay，重启后恢复相同 UA 与会话；该目录及密钥只应由服务账号访问并一起备份。旧绑定仅有学校 RSA 密文时，现场公钥相同可复用；若公钥已轮换，需要在原 IAAA 绑定页面重新绑定一次。HTTP 401 会清除过期会话并尝试自动重新认证，需要验证码时仍由用户完成额外验证。日志与浏览器不会收到密码、票据和令牌。

鉴权请求及重定向只允许 ZIP 中的三个 HTTPS 域名：Passport、IAAA 和 resource。诊断日志使用 `app_replay_auth` tracing，记录安全路径、HTTP 状态和会话字段是否出现，不记录查询参数、密码、票据、令牌或 Cookie。IAAA 通过但 App 会话缺失时，页面仍显示“认证诊断”。

## 添加课节

进入 `pkuvideo_sequence` 选择课程，点击“手动添加课节”。按教学网学期自动读取配置，可多选星期或指定一天，不能超出学期或北京时间今天。

`manual_attempts.json` 保存成功、失败、部分成功和中断的日期记录；提交查询前先持久化 pending 标记。正常刷新教学网课节只保留手动记录，不重复遍历 App 日期。再次提交相同星期时只查询新增日期；勾选重试后才重查失败／中断日期。已有 App 课次按 `course_id/sub_id` 去重。

添加时必须选择第 1–12 节内的起止节次；服务端验证开始不大于结束并保存、显示该范围。成功课节的 `manual_replay` 标记记录来源、日期和节次。处理、重新处理和重新执行下载步骤都先重新查询该日期的回放列表及签名详情接口获取新地址，再由 Rust 的私有回环媒体代理携带媒体 Cookie 拉取 MP4 或 HLS。代理逐级改写主／子播放列表、分片、密钥和 MAP 地址，ffmpeg 合并为视频，随后执行原来的 MP3、转写、讲义和 PDF 流程。FFmpeg 的参数只有本地随机媒体地址，不含远端令牌。跨域媒体重定向不传递 Cookie。

## 课节顺序与笔记文件

刷新、手动添加、删除和保存处理结果共用重编号逻辑。课节 ID 保持稳定，笔记统一命名为 `<course_id>_lesson_<实际序号>.tex`；例如在原第 1 节前插入课节后，原第 1 节的笔记成为第 2 节。添加时从刷新前最后一个课节向前移动，减少时从前向后移动，更新 `tex_filename` 并保留已处理状态。完整 PDF 也使用相同的日期／上课时间排序。

文件先按上述方向移入唯一临时位置，再发布新文件名并原子保存索引，不覆盖未关联文件。同数量重排或旧文件名迁移也不会发生重名覆盖。操作失败会回滚，服务中断后下次加载会根据 `.lesson-order/journal.json` 恢复；需要移出当前课程的旧笔记保存在 `.removed-notes/<事务ID>/`，不再参与当前课节下载。

只有手动课节显示“删除课节”，点击后必须准确输入“确认删除”再提交，后端也核对该文字。处理中的用户不能增删或刷新课节；下载笔记时也会锁定当前顺序。删除后保留日期查询记录，普通刷新不会重新添加已删除的手动课节。

## 学期配置

`config/pku_replay_semesters.json` 的键为 `起始年-结束年-学期序号`，兼容教学网的两位／四位年份及中文学期序号。已按配置中列出的官方校历预设 2024–2027 校本部范围（含考试期）；其他校区或学期可修改／追加，`PKU_REPLAY_SEMESTERS` 可以指定其他配置文件。缺少配置时明确提示并停止查询。

App 默认按去除空白后的完整课程名匹配。名称不同或 App 中有多门同名课程时，在 `app_course_ids` 中添加教学网 course key 到 App course_id 的映射，例如 `"_12345_1": "67890"`，以免添加其他课程。

## 验证

`cargo test app_replay` 验证原生客户端、回调字面量解析、RSA 公钥、签名向量、回放地址兼容和 HLS 改写；`cargo test manual` 验证查询去重和刷新后保留来源。真实账号的 App 会话交换与播放仍需要在校园网／VPN 下验证。

## APK 核对（2026-10-14）

对用户提供的 bdhx.apk.1.1 使用官方 jadx 1.5.6 静态反编译，未执行 APK、未上传第三方。部分无关方法反编译报错，以下关键实现已成功读取：BrowserBridgeInterface.PKULoginSuccess 解析 UserInfo 并写入 StoreManager；Room DAO 将 _token 与 _cookieToken 写入 UserInfo 表；SignInterceptor 使用 token 请求头、双 tenant 参数、3.12.0 版本与排序 MD5；PlaybackNetApi 先按 course_time 查询 get-use-member-sub，再通过 course_id/sub_id 查询 get-course-sub-detail；PlaybackVideoDetailActivity 使用 Cookie: _token=<UserInfo._cookieToken> 获取媒体，优先 videoArr.path。新增 Rust 实现保持这些协议，并随机选择现有 UA 池中的浏览器头。反编译输出仅位于忽略的 target/apk-analysis 目录。

## 观看手动回放与进度

手动添加时显示转圈和尝试提示，按钮为“关闭”；关闭弹窗不会中断已提交的添加，结果也会显示在页面状态区。手动课节的“观看回放”按钮跳转到 pkuvideo_fw，并通过好学渠道刷新日期列表、详情和媒体地址。两个下载渠道共用每用户一个视频槽位；新手动回放自动清理该用户已完成／失败任务的旧文件。已有任务仍在下载时提示等待，不并发覆盖。删除只定位回放存储目录内匹配任务 UUID 的目录。

HLS 下载按成功分片数实时更新至 99%，重试不重复计数，合并／封装完成后显示 100%。好学媒体代理根据实际读取完成的分片统计，支持子播放列表与字节范围分片，密钥和初始化片段不纳入分片计数。页面每两秒读取进度，99% 时显示正在合并；MP4 仍在下载完成后进入播放状态。跳转参数提交后从地址栏移除，刷新页面只恢复任务，不会再次替换已下载视频。
