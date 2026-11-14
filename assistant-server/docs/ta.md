# 助教作业批改

入口为 `/ta`，也可从全站导航的“助教批改”进入。页面沿用课程回放批量处理的配色、页头、插画与课程侧栏。

打开页面先显示框架和加载占位，课程与作业通过 `/api/ta/workspace` 在后台读取。刷新、筛选、切换课程或提交、保存草稿和同步结果只更新下方工作区，不重新加载页头。加载失败可以局部重试，写入请求不会自动重发。

## 使用

1. 使用现有小助手账号登录。在用户中心完成已有的教学网绑定后，页面自动读取 `TeachingAssistant` 角色的课程；本模块没有独立登录或绑定表单。
2. 选择作业，可按批改组、评分状态、姓名或学号筛选。默认每位学生只展示最新提交，勾选“显示历史提交”可逐次查看。
3. 查看提交正文、下载附件或展开 PDF 预览，填写分数和评语。“保存并批改下一份”将本份保存为本地草稿并打开下一份。
4. “快速批改”支持选中提交统一给分、统一评语、满分草稿，也可选择当前筛选下的全部可评分提交，每次最多 200 份。已评分提交需要显式允许修改，豁免和提交中的记录不可评分。
5. 在“核对并同步”中勾选草稿，确认后写入教学网对应的具体提交。每份登分后回读核验分数，发生变更或失败即停止，其余草稿保留。

保存草稿不会修改教学网。登分结果未确认时会标记“需核对”并阻止直接重试；应先在教学网核对，再丢弃旧草稿或重新批改。

## 实现

- `src/services/ta.rs`：Rust 教学网客户端，共用回放的 curl 执行器，实现分页、角色识别、附件解析、nonce 和登分回读。
- `src/routes/ta.rs`：页面路由、会话获取、按账号隔离的持久草稿、批量操作和附件代理。
- `src/routes/ta_view.rs`：Rust 生成的 HTML、CSS、表单及内嵌的局部更新浏览器脚本。复用现有公共样式及资源，无新的前端构建或依赖包。

优先从 `AppState.login_cache` 获取已有教学网会话；缺少缓存时调用现有 `pkulogin::login_iaaa_and_get_cookies_with_cache`。直接将现有 Netscape Cookie 文件交给回放共用的 curl 执行器读写，不再导入另一套 Cookie jar；业务逻辑全部用 Rust，不调用 Python 或 pku3b，不新增登录组件。助教和教学网回放共用下述登录重试策略。

新建教学网登录会话最多尝试三次：前两次各从原有 `user_agents.txt`（没有可用项时回退到 `.gz`）随机取一个非空 Windows UA，并在发起请求前删除两个来源中该 UA 的所有重复项。第一次成功即停止，并把 UA 去重持久化到 `data/teaching_success_user_agents.txt`。连续两次失败后，第三次从成功池抽取但不删除；成功池为空时，再从原池取一个并删除。第三次失败则报错。原池耗尽时不会生成空 UA，仍可在第三轮利用成功池；两池皆空时返回明确错误。每轮重新执行完整 IAAA 登录、一次性 token 回调和教学网门户验证，只有全部通过才记入成功池。有效的现有会话继续直接复用，不消耗 UA。UA 池更新带进程内互斥和临时文件替换，成功池不包含密码、Cookie 或 token；部署时应持久保留消耗后的原池及成功池。

评分状态以教学网的提交状态为准：`NeedsGrading` 等未批改状态即使返回数值 `0`，也显示为待评分且不预填零分；`Completed`／`Graded` 状态下真正给出的零分仍显示为已评分。Blackboard 官方也说明了[未批改状态可能带有零分](https://help.anthology.com/blackboard/instructor/en/original-course-view/tests%2C-pools%2C-and-surveys/grade-tests.html)。

页面只用 GET 浏览和 POST 表单进行修改。修改请求校验当前登录会话派生的 CSRF 凭据，并在服务端重新校验助教身份、作业、提交记录、分数范围和提交版本。草稿通过现有 FileStore sidecar 接口存入 SQLite，键按用户 UUID、课程和作业隔离；登分前持久化操作标记，避免服务中断后未经核对地重复登分。

附件代理仅接受当前助教课程中真实提交页面返回的文件；请求及重定向限制为教学网同源。附件大小上限为 32 MiB，大文件提供教学网原始提交入口。PDF 按扩展名及文件头校验后支持预览，其他附件强制下载。

## 验证与范围

助教 HTTP 请求与教学网回放共用 `pkulogin::run_curl_bytes`，复用缓存 Cookie 文件、文件锁和 User-Agent，并沿用回放的 curl 参数与超时设置。评分表单写入临时文件后提交，不自动重发 POST。无需新增 Rust TLS 后端、OpenSSL 库或构建工具。

后台 `teaching_ta` 日志在 info 级别记录请求和响应状态，在 warn 级别记录经过脱敏的底层连接错误。日志不输出 Cookie、nonce、请求体、查询参数或学生标识。curl 连接错误、HTTP 登录过期和权限拒绝会分别显示。

可手动检查公开接口连通性，此测试不携带账号、密码或 Cookie，也不会登分：

```text
cargo test --offline teaching_curl_smoke_test -- --ignored --nocapture
```

```text
cargo test --offline '::ta::'
cargo test --offline protected_html_requires_valid_session_before_rendering
```

本地视觉预览使用虚构数据，不会访问教学网：

```text
cargo test --offline ta_visual_preview -- --ignored --nocapture
```

随后打开 `http://127.0.0.1:18192/ta`。该测试预览不提供登分操作，最多运行 15 分钟。

真实教学网权限、课程配置和页面结构可能不同，尤其是非标准作业、匿名/委派批改和不支持 reconciliation 的成绩列。首次使用需用真实助教课程验证读取和单份登分；模拟测试不能替代实网联调。本实现提供人工批改和快捷批量评分，不包含 AI 自动判分、Word 预览或 PDF 内页批注。
