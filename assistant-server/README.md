# Assistant Server (Rust)

这是基于 Rust 的跨平台智能助手 Web 服务，包含：

- 多用户系统（注册、登录、会话）
- 用户数据隔离（会话、会话对话、微信通道）
- 微信 OAuth 绑定
- 微信交互通道重写（二维码登录、发送消息、后台监听、单活跃用户约束）
- 模块化插件回复（demo / weather / yxx）

## 快速启动

1. 准备 PostgreSQL 和 Redis。
2. 复制配置：

```bash
cp .env.example .env
cp config/development.toml config/local.toml
```

3. 执行迁移并启动：

```bash
cargo build
# 迁移
sqlx database create
sqlx migrate run
# 运行
cargo run
```

## 关键 API

前端页面入口:

- `GET /`（完整前端：登录/注册、对话、设置）
- `GET /app`
- `GET /auth`（独立登录/注册页）
- `GET /pkuvideo`（视频处理工作台）
- `GET /settings`（独立设置页）

- `POST /api/auth/register`
- `POST /api/auth/login`
- `GET /api/auth/me`
- `POST /api/auth/logout`
- `GET /api/settings/profile`
- `PUT /api/settings/profile`
- `GET /api/conversations`
- `POST /api/assistant/send`
- `POST /api/pkuvideo/submit`（5字段：平台账密2 + PKU认证2 + 视频地址1）
- `POST /api/pkuvideo/submit-web`（页面用，cookie登录态 + 3字段）
- `GET /api/pkuvideo/jobs/:id`
- `GET /api/wechat/auth-url`
- `GET /api/wechat/callback`
- `POST /api/wechat/bind`
- `DELETE /api/wechat/unbind`
- `GET /api/wechat/binding-status`
- `POST /api/wechat/channel/login/start`
- `POST /api/wechat/channel/login/confirm`
- `POST /api/wechat/channel/listener/start`
- `POST /api/wechat/channel/send`

`/api/pkuvideo/submit` 字段映射：

- `pku_auth_one` -> PKU IAAA `username`
- `pku_auth_two` -> PKU IAAA `password`

视频下载前，后端会执行 Rust 内置的 PKU 登录流程（IAAA OAuth + elective SSO），并从 `user_agents.txt.gz` 随机选择 `User-Agent`，拿到 Cookie 后再请求视频地址。

## 微信通道行为说明

- 登录后每个用户独立维护 `user_wechat_channels` 记录。
- 调度器定时长轮询 `getupdates`。
- 首个合法发送方会绑定为该用户的 `active_user_id`。
- 仅 `active_user_id` 可触发自动回复（多用户隔离 + 单活跃会话约束）。
- 超过 22 小时无用户消息会自动发送令牌激活提醒。

## 插件系统

分类器在 `src/plugins/classifier.rs`，模块路由在 `src/plugins/router.rs`。

当前模块：

- `demo`: 返回 `成功分类`
- `weather`: 调用天气 API
- `yxx`: 返回 `你好呀，我是严小希~`
