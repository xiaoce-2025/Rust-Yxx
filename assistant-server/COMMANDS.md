# 常见命令参考

本文档收集了项目开发和测试的常见命令。

## 目录
1. [项目初始化](#项目初始化)
2. [编译和运行](#编译和运行)
3. [测试命令](#测试命令)
4. [API 端点参考](#api-端点参考)
5. [数据管理](#数据管理)
6. [故障排除](#故障排除)

---

## 项目初始化

### 首次设置

```powershell
# 克隆或进入项目目录
cd d:\MyProject\Rust Yxx\assistant-server

# 初始化 Rust 项目（如果需要）
cargo init --name assistant-server
```

---

## 编译和运行

### 开发环境运行

```powershell
# 进入项目目录
cd d:\MyProject\Rust Yxx\assistant-server

# 运行开发服务器（带热重载）
cargo run

# 编译检查（不生成二进制）
cargo check

# 构建发布版本
cargo build --release

# 运行已编译的二进制
.\target\debug\assistant-server.exe

# 清理编译缓存
cargo clean
```

### 指定配置运行

```bash
# 使用特定的配置文件
RUST_LOG=debug cargo run

# 修改监听端口
./target/debug/assistant-server --port 3000
```

---

## 测试命令

### 运行自动化测试脚本

```powershell
# 运行所有测试
.\test-api.ps1

# 仅运行指定测试
.\test-api.ps1 -Test register        # 用户注册测试
.\test-api.ps1 -Test login           # 用户登录测试
.\test-api.ps1 -Test info            # 获取用户信息测试
.\test-api.ps1 -Test logout          # 用户登出测试
.\test-api.ps1 -Test message         # 发送消息测试
.\test-api.ps1 -Test conversations   # 对话列表测试
.\test-api.ps1 -Test wechat          # WeChat 绑定测试
.\test-api.ps1 -Test settings        # 用户设置测试
.\test-api.ps1 -Test health          # 健康检查测试

# 显示帮助信息
.\test-api.ps1 -Help
```

### 手动 API 测试

#### 健康检查
```powershell
Invoke-WebRequest -Uri "http://127.0.0.1:8080/health" -UseBasicParsing
```

#### 用户注册
```powershell
$body = @{
    username = "newuser"
    email = "user@example.com"
    password = "SecurePass123!"
} | ConvertTo-Json

$resp = Invoke-WebRequest -Uri "http://127.0.0.1:8080/api/auth/register" `
    -Method Post `
    -Body $body `
    -ContentType "application/json" `
    -UseBasicParsing

$result = $resp.Content | ConvertFrom-Json
$result | ConvertTo-Json
```

#### 用户登录
```powershell
$body = @{
    email = "user@example.com"
    password = "SecurePass123!"
} | ConvertTo-Json

$resp = Invoke-WebRequest -Uri "http://127.0.0.1:8080/api/auth/login" `
    -Method Post `
    -Body $body `
    -ContentType "application/json" `
    -UseBasicParsing

$result = $resp.Content | ConvertFrom-Json
$token = $result.token
Write-Host "登录成功，Token: $token"
```

#### 获取当前用户信息
```powershell
$token = "your-token-here"
$headers = @{"Authorization" = "Bearer $token"}

$resp = Invoke-WebRequest -Uri "http://127.0.0.1:8080/api/auth/me" `
    -Headers $headers `
    -UseBasicParsing

$resp.Content | ConvertFrom-Json | ConvertTo-Json
```

#### 发送消息
```powershell
$token = "your-token-here"
$headers = @{"Authorization" = "Bearer $token"}

$body = @{
    text = "你好，这是一条测试消息"
    conversation_id = $null
} | ConvertTo-Json

$resp = Invoke-WebRequest -Uri "http://127.0.0.1:8080/api/assistant/send" `
    -Method Post `
    -Headers $headers `
    -Body $body `
    -ContentType "application/json" `
    -UseBasicParsing

$resp.Content | ConvertFrom-Json | ConvertTo-Json
```

#### 获取对话列表
```powershell
$token = "your-token-here"
$headers = @{"Authorization" = "Bearer $token"}

$resp = Invoke-WebRequest -Uri "http://127.0.0.1:8080/api/conversations" `
    -Headers $headers `
    -UseBasicParsing

$resp.Content | ConvertFrom-Json | ConvertTo-Json
```

#### 获取 WeChat 绑定状态
```powershell
$token = "your-token-here"
$headers = @{"Authorization" = "Bearer $token"}

$resp = Invoke-WebRequest -Uri "http://127.0.0.1:8080/api/wechat/binding-status" `
    -Headers $headers `
    -UseBasicParsing

$resp.Content | ConvertFrom-Json | ConvertTo-Json
```

#### 获取用户设置
```powershell
$token = "your-token-here"
$headers = @{"Authorization" = "Bearer $token"}

$resp = Invoke-WebRequest -Uri "http://127.0.0.1:8080/api/settings/profile" `
    -Headers $headers `
    -UseBasicParsing

$resp.Content | ConvertFrom-Json | ConvertTo-Json
```

#### 更新用户设置
```powershell
$token = "your-token-here"
$headers = @{"Authorization" = "Bearer $token"}

$body = @{
    default_model = "gpt-4"
    theme = "dark"
    openai_api_key = "sk-..."
} | ConvertTo-Json

$resp = Invoke-WebRequest -Uri "http://127.0.0.1:8080/api/settings/profile" `
    -Method Put `
    -Headers $headers `
    -Body $body `
    -ContentType "application/json" `
    -UseBasicParsing

$resp.Content | ConvertFrom-Json | ConvertTo-Json
```

#### 用户登出
```powershell
$token = "your-token-here"
$headers = @{"Authorization" = "Bearer $token"}

$resp = Invoke-WebRequest -Uri "http://127.0.0.1:8080/api/auth/logout" `
    -Method Post `
    -Headers $headers `
    -UseBasicParsing

$resp.Content | ConvertFrom-Json | ConvertTo-Json
```

---

## API 端点参考

### 认证相关
| 方法 | 端点 | 说明 | 需要认证 |
|------|------|------|---------|
| POST | `/api/auth/register` | 用户注册 | ❌ |
| POST | `/api/auth/login` | 用户登录 | ❌ |
| GET | `/api/auth/me` | 获取当前用户信息 | ✅ |
| POST | `/api/auth/logout` | 用户登出 | ✅ |

### 对话相关
| 方法 | 端点 | 说明 | 需要认证 |
|------|------|------|---------|
| GET | `/api/conversations` | 获取对话列表 | ✅ |
| POST | `/api/assistant/send` | 创建对话/发送消息 | ✅ |

### WeChat 相关
| 方法 | 端点 | 说明 | 需要认证 |
|------|------|------|---------|
| GET | `/api/wechat/auth-url` | 获取授权 URL | ✅ |
| GET | `/api/wechat/binding-status` | 获取绑定状态 | ✅ |
| DELETE | `/api/wechat/unbind` | 解除绑定 | ✅ |

### 设置相关
| 方法 | 端点 | 说明 | 需要认证 |
|------|------|------|---------|
| GET | `/api/settings/profile` | 获取用户设置 | ✅ |
| PUT | `/api/settings/profile` | 更新用户设置 | ✅ |

### 健康检查
| 方法 | 端点 | 说明 |
|------|------|------|
| GET | `/health` | 服务健康检查 |

---

## 数据管理

### 查看数据文件

```powershell
# 查看数据目录
ls -Recurse d:\MyProject\Rust Yxx\assistant-server\data

# 查看用户数据
type d:\MyProject\Rust Yxx\assistant-server\data\users.json

# 查看会话数据
type d:\MyProject\Rust Yxx\assistant-server\data\sessions.json

# 查看对话记录
type d:\MyProject\Rust Yxx\assistant-server\data\conversations.json

# 查看消息记录
type d:\MyProject\Rust Yxx\assistant-server\data\messages.json
```

### 重置数据

```powershell
# 删除所有数据文件（谨慎操作）
Remove-Item -Path "d:\MyProject\Rust Yxx\assistant-server\data\*.json" -Force

# 删除整个数据目录
Remove-Item -Path "d:\MyProject\Rust Yxx\assistant-server\data" -Recurse -Force
```

### 备份数据

```powershell
# 创建数据备份
$timestamp = Get-Date -Format "yyyyMMdd_HHmmss"
Copy-Item -Path "d:\MyProject\Rust Yxx\assistant-server\data" `
    -Destination "d:\MyProject\Rust Yxx\assistant-server\backup_$timestamp" `
    -Recurse

# 恢复备份
Copy-Item -Path "d:\MyProject\Rust Yxx\assistant-server\backup_20260331_192600\*" `
    -Destination "d:\MyProject\Rust Yxx\assistant-server\data" `
    -Recurse -Force
```

---

## 故障排除

### 问题：服务无法启动
```powershell
# 检查端口是否被占用
netstat -ano | findstr :8080

# 杀死占用 8080 端口的进程
taskkill /PID <PID> /F

# 验证服务是否启动
Test-NetConnection -ComputerName 127.0.0.1 -Port 8080
```

### 问题：编译错误
```powershell
# 清理缓存重新编译
cargo clean
cargo check
cargo build
```

### 问题：API 请求超时
```powershell
# 增加请求超时时间（秒）
$resp = Invoke-WebRequest -Uri "http://127.0.0.1:8080/..." `
    -TimeoutSec 30 `
    -UseBasicParsing
```

### 问题：认证失败
```powershell
# 确认 token 格式正确
$token = "your-token-here"
$headers = @{"Authorization" = "Bearer $token"}

# 验证 token 是否过期（查看 data/sessions.json）
type d:\MyProject\Rust Yxx\assistant-server\data\sessions.json
```

### 问题：查看日志
```powershell
# 运行时输出日志
$env:RUST_LOG = "debug"
cargo run
```

---

## 快速开始指南

### 1. 启动服务
```powershell
cd d:\MyProject\Rust Yxx\assistant-server
cargo run
# 服务将监听 http://127.0.0.1:8080
```

### 2. 运行完整测试
```powershell
# 在另一个 PowerShell 窗口中
.\test-api.ps1
```

### 3. 手动测试单个功能
```powershell
# 注册用户
$body = @{username="user1"; email="user1@example.com"; password="pass123"} | ConvertTo-Json
$resp = Invoke-WebRequest -Uri "http://127.0.0.1:8080/api/auth/register" `
    -Method Post -Body $body -ContentType "application/json" -UseBasicParsing
$token = ($resp.Content | ConvertFrom-Json).token

# 获取用户信息
$headers = @{"Authorization"="Bearer $token"}
Invoke-WebRequest -Uri "http://127.0.0.1:8080/api/auth/me" `
    -Headers $headers -UseBasicParsing | ForEach-Object { $_.Content | ConvertFrom-Json }
```

---

## 有用的快捷方式

### PowerShell 别名（可添加到 $PROFILE）
```powershell
# 在 PowerShell 配置中添加
function Start-RustServer {
    cd d:\MyProject\Rust Yxx\assistant-server
    cargo run
}

function Test-RustAPI {
    cd d:\MyProject\Rust Yxx\assistant-server
    .\test-api.ps1 @args
}

alias rs = Start-RustServer
alias test = Test-RustAPI
```

### Linux/macOS 快捷方式（.bashrc/.zshrc）
```bash
# 启动服务
alias start-server='cd ~/Rust\ Yxx/assistant-server && cargo run'

# 运行测试
alias test-api='cd ~/Rust\ Yxx/assistant-server && ./test-api.ps1'

# 编译检查
alias check='cd ~/Rust\ Yxx/assistant-server && cargo check'
```

---

## 环境变量配置

```powershell
# 设置日志级别
$env:RUST_LOG = "debug"        # 详细日志
$env:RUST_LOG = "info"         # 信息日志
$env:RUST_LOG = "warn"         # 仅警告和错误
$env:RUST_LOG = "error"        # 仅错误

# 设置监听地址（需要修改代码）
$env:SERVER_HOST = "0.0.0.0"   # 接受来自任何 IP 的连接
$env:SERVER_PORT = "3000"      # 修改端口为 3000
```

---

## 生产环境部署

### 编译发布版本
```powershell
# 构建优化的发布版本
cargo build --release

# 运行发布版本
.\target\release\assistant-server.exe
```

---

## 相关文件

| 文件 | 说明 |
|------|------|
| `Cargo.toml` | 项目依赖配置 |
| `src/main.rs` | 应用入口 |
| `src/store.rs` | 文件存储层实现 |
| `src/routes/` | API 路由定义 |
| `data/` | 数据文件存储目录 |
| `test-api.ps1` | API 自动化测试脚本 |
| `config/development.toml` | 开发环境配置 |

---

## 最后更新
- **日期**: 2026-03-31
- **版本**: 0.1.0
- **状态**: 开发版本 (使用文件存储代替数据库)
