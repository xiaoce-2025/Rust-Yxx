# 快速开始指南

## 项目概览

这是一个用 **Rust + Axum** 框架实现的智能助手服务器，采用**文件存储**（无需数据库）架构。

- ✅ **无数据库依赖** - 使用文件存储 (JSON)
- ✅ **完整的用户认证系统** - 支持注册、登录、登出
- ✅ **对话管理** - 创建和管理多个对话
- ✅ **用户设置** - 自定义模型和界面配置
- ✅ **WeChat 集成** - 支持绑定和消息接收

---

## 5 分钟快速开始

### 第 1 步：启动服务

```powershell
cd "d:\MyProject\Rust Yxx\assistant-server"
cargo run
```

等待编译完成，你会看到服务启动在 `http://127.0.0.1:8080`

```
✓ 服务已启动
✓ 监听: 127.0.0.1:8080
✓ 热重载可用
```

### 第 2 步：运行测试脚本

打开另一个 **PowerShell** 窗口：

```powershell
cd "d:\MyProject\Rust Yxx\assistant-server"
.\test-api.ps1
```

脚本会自动测试所有 API 端点：
- ✓ 用户注册
- ✓ 用户登录
- ✓ 获取用户信息
- ✓ 发送消息
- ✓ 获取对话
- ✓ WeChat 绑定状态
- ✓ 用户设置

### 第 3 步：查看结果

```
====== Rust 助手服务 API 测试 ======
基础 URL: http://127.0.0.1:8080

✓ 健康检查成功
✓ 注册用户成功
✓ 获取用户信息成功
✓ 发送消息成功
✓ 获取对话列表成功
✓ 获取设置成功
✓ 更新设置成功
✓ 用户登出成功

====== 测试完成 ======
通过: 8/8
```

---

## 常用命令速查

### 启动和停止

```powershell
# 启动开发服务器
cargo run

# 停止服务 (Ctrl+C)

# 仅检查编译，不运行
cargo check

# 构建发布版本
cargo build --release
```

### 运行测试

```powershell
# 运行全部测试
.\test-api.ps1

# 运行特定测试
.\test-api.ps1 -Test register      # 只测试注册
.\test-api.ps1 -Test login         # 只测试登录
.\test-api.ps1 -Test message       # 只测试发送消息

# 查看帮助
.\test-api.ps1 -Help
```

### 管理数据

```powershell
# 查看所有数据文件
ls data/

# 查看用户列表
type data\users.json | ConvertFrom-Json

# 查看活跃会话
type data\sessions.json | ConvertFrom-Json

# 清除所有数据（谨慎！）
rm data\*.json
```

---

## 测试核心功能

### 1. 用户注册和登录

```powershell
# 注册一个新用户
$body = @{
    username = "alice"
    email = "alice@example.com"
    password = "SecurePass123!"
} | ConvertTo-Json

$resp = Invoke-WebRequest -Uri "http://127.0.0.1:8080/api/auth/register" `
    -Method Post -Body $body -ContentType "application/json" -UseBasicParsing

$result = $resp.Content | ConvertFrom-Json
Write-Host "✓ 注册成功: $($result.token.Substring(0,8))..."
$token = $result.token

# 登录
$loginBody = @{
    email = "alice@example.com"
    password = "SecurePass123!"
} | ConvertTo-Json

$resp = Invoke-WebRequest -Uri "http://127.0.0.1:8080/api/auth/login" `
    -Method Post -Body $loginBody -ContentType "application/json" -UseBasicParsing

$result = $resp.Content | ConvertFrom-Json
Write-Host "✓ 登录成功: $($result.username)"
```

### 2. 获取用户信息

```powershell
$headers = @{"Authorization" = "Bearer $token"}

$resp = Invoke-WebRequest -Uri "http://127.0.0.1:8080/api/auth/me" `
    -Headers $headers -UseBasicParsing

$user = $resp.Content | ConvertFrom-Json
Write-Host "✓ 用户名: $($user.username)"
Write-Host "✓ 邮箱: $($user.email)"
```

### 3. 发送消息

```powershell
$body = @{
    text = "你好，今天天气怎么样？"
    conversation_id = $null  # null 表示创建新对话
} | ConvertTo-Json

$resp = Invoke-WebRequest -Uri "http://127.0.0.1:8080/api/assistant/send" `
    -Method Post `
    -Headers $headers `
    -Body $body `
    -ContentType "application/json" `
    -UseBasicParsing

$msg = $resp.Content | ConvertFrom-Json
Write-Host "✓ 对话 ID: $($msg.conversation_id)"
Write-Host "✓ 用户消息: $($msg.user_text)"
Write-Host "✓ AI 回复: $($msg.assistant_text)"
```

### 4. 更新用户设置

```powershell
$body = @{
    default_model = "gpt-4"
    theme = "dark"
    openai_api_key = "sk-your-api-key"
} | ConvertTo-Json

$resp = Invoke-WebRequest -Uri "http://127.0.0.1:8080/api/settings/profile" `
    -Method Put `
    -Headers $headers `
    -Body $body `
    -ContentType "application/json" `
    -UseBasicParsing

$settings = $resp.Content | ConvertFrom-Json
Write-Host "✓ 默认模型: $($settings.default_model)"
Write-Host "✓ 主题: $($settings.theme)"
```

---

## 数据文件说明

所有数据存储在 `data/` 目录（JSON 格式）：

| 文件 | 内容 | 说明 |
|------|------|------|
| `data/users.json` | 用户账户数据 | 包含用户名、邮箱、密码哈希 |
| `data/sessions.json` | 活跃会话 | 包含 token、过期时间、用户关联 |
| `data/conversations.json` | 对话记录 | 包含对话 ID、标题、时间戳 |
| `data/messages.json` | 消息历史 | 包含消息内容、角色、时间戳 |
| `data/user_configs.json` | 用户设置 | 包含模型、主题、API 密钥 |
| `data/wechat_bindings.json` | WeChat 绑定 | 包含 OpenID、昵称、头像 |

---

## 常见问题

### Q1: 服务无法启动，提示端口被占用

```powershell
# 查找占用 8080 端口的进程
netstat -ano | findstr :8080

# 杀死进程（替换 <PID>）
taskkill /PID <PID> /F

# 或者修改配置使用其他端口
# 编辑 config/development.toml
```

### Q2: 如何重置所有数据？

```powershell
# 删除数据目录
Remove-Item -Path "data" -Recurse -Force

# 重启服务，会自动创建新的数据目录
```

### Q3: 如何查看详细日志？

```powershell
# 设置日志级别为 debug
$env:RUST_LOG = "debug"
cargo run
```

### Q4: Token 过期了怎么办？

```powershell
# 直接用邮箱和密码重新登录即可获取新 token
# Token 默认有效期为 1 年
```

### Q5: 如何在其他机器上访问服务？

编辑 `src/main.rs`，修改监听地址：
```rust
// 改为
let listener = TcpListener::bind("0.0.0.0:8080").await?;

// 然后重新编译
cargo build
```

---

## 项目结构

```
assistant-server/
├── src/
│   ├── main.rs              # 应用入口
│   ├── store.rs             # 文件存储实现 (关键!)
│   ├── app_state.rs         # 应用状态
│   ├── auth.rs              # 认证逻辑
│   ├── config.rs            # 配置管理
│   ├── error.rs             # 错误定义
│   ├── models/              # 数据模型
│   ├── routes/              # API 路由
│   ├── services/            # 业务服务
│   └── plugins/             # 消息处理插件
├── data/                    # 数据存储 (JSON 文件)
├── config/                  # 配置文件
├── Cargo.toml              # Rust 依赖配置
├── test-api.ps1            # API 测试脚本 ⭐
├── COMMANDS.md             # 命令参考 ⭐
└── README.md               # 项目说明

⭐ 新添加的文件
```

---

## 下一步

### 深入学习

1. 查看 [COMMANDS.md](COMMANDS.md) - 全面的命令参考
2. 查看 [README.md](README.md) - 项目详细说明
3. 阅读源码 - 理解文件存储的实现

### 扩展功能

1. **实现完整的对话功能** - 查看 `src/routes/conversations.rs`
2. **添加 WeChat 消息处理** - 查看 `src/routes/wechat.rs`
3. **自定义插件** - 添加到 `src/plugins/`
4. **数据库迁移** - 如果需要可以添加 PostgreSQL 支持

### 部署应用

1. 编译发布版本：`cargo build --release`
2. 配置生产环境：修改 `config/development.toml`
3. 部署到云平台：AWS、Azure、腾讯云等

---

## 获取帮助

### 有用的命令

```powershell
# 查看 Cargo 帮助
cargo --help

# 查看测试脚本帮助
.\test-api.ps1 -Help

# 构建时显示详细信息
cargo build --verbose

# 运行单元测试
cargo test

# 格式化代码
cargo fmt

# 检查代码规范
cargo clippy
```

### 阅读文档

- [Rust Book](https://doc.rust-lang.org/book/)
- [Axum 文档](https://docs.rs/axum/)
- [Tokio 异步运行时](https://tokio.rs/)
- [Serde JSON 序列化](https://docs.rs/serde_json/)

---

## 开发技巧

### 热重载开发

使用 `cargo-watch` 实现文件改动自动编译：

```powershell
# 安装
cargo install cargo-watch

# 使用
cargo watch -x run
```

### 调试模式

```powershell
# 使用 GDB 调试（Windows 需要 LLDB）
rust-gdb ./target/debug/assistant-server

# 或使用 VS Code 调试（需要配置 launch.json）
```

### 性能分析

```powershell
# 使用 flamegraph 分析性能
cargo install flamegraph
cargo flamegraph

# 查看详细的编译时间
cargo build --release --verbose
```

---

## 常用的 API 速查

### 认证

```
POST   /api/auth/register        - 注册
POST   /api/auth/login           - 登录
POST   /api/auth/logout          - 登出
GET    /api/auth/me              - 当前用户 (需要 token)
```

### 对话

```
GET    /api/conversations        - 获取对话列表
POST   /api/assistant/send       - 发送消息
```

### 设置

```
GET    /api/settings/profile     - 获取设置
PUT    /api/settings/profile     - 更新设置
```

### WeChat

```
GET    /api/wechat/auth-url      - 获取授权 URL
GET    /api/wechat/binding-status - 获取绑定状态
DELETE /api/wechat/unbind        - 解除绑定
```

### 健康检查

```
GET    /health                   - 服务健康状态
```

---

## 最后提示

✅ **立即开始**：运行 `cargo run` 然后 `.\test-api.ps1`

✅ **查看日志**：`data/` 目录下的 JSON 文件包含所有数据

✅ **修改配置**：编辑 `config/development.toml` 

✅ **遇到问题**：查阅 [COMMANDS.md](COMMANDS.md) 的故障排除部分

---

**祝你使用愉快！** 🚀
