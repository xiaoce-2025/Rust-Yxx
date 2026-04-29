# use.md

## 项目当前状态

本项目已经改为 Rust 文件存储模式，不再依赖 PostgreSQL、Redis、Docker。

- 数据目录：`assistant-server/data/`
- 主要数据文件：`users.json`、`sessions.json`、`conversations.json`、`messages.json`

---

## 1. 启动服务

在 PowerShell 中执行：

```powershell
cd "d:\MyProject\Rust Yxx\assistant-server"
cargo run
```

启动后默认监听：`http://127.0.0.1:8080`

---

## 2. 常用访问地址

- 健康检查：`http://127.0.0.1:8080/health`
- 登录注册页：`http://127.0.0.1:8080/auth`
- 首页：`http://127.0.0.1:8080/`
- PKU 视频处理页：`http://127.0.0.1:8080/pkuvideo`
- 设置页：`http://127.0.0.1:8080/settings`

---

## 2.1 PKU Video API（5 字段提交）

```http
POST /api/pkuvideo/submit
Content-Type: application/json

{
	"platform_email": "your_email@example.com",
	"platform_password": "your_password",
	"pku_auth_one": "pku_username",
	"pku_auth_two": "pku_password",
	"video_url": "https://example.com/video"
}
```

说明：
- `pku_auth_one` 是 IAAA 账号（学号）
- `pku_auth_two` 是 IAAA 密码
- `User-Agent` 由服务端从 `assistant-server/user_agents.txt.gz` 随机抽取
- 后端流程严格按新版门户逻辑执行：先请求 `https://iaaa.pku.edu.cn/iaaa/oauthlogin.do`（`appid=blackboard` + 教学网 `redirUrl`）获取 `token`，再请求 `http://course.pku.edu.cn/webapps/bb-sso-BBLEARN/execute/authValidate/campusLogin?_rand=...&token=...` 完成教学网登录，然后直接访问下载链接

查询任务状态：

```http
GET /api/pkuvideo/jobs/{job_id}
```

---

## 3. 一键 API 测试

另开一个 PowerShell 窗口执行：

```powershell
cd "d:\MyProject\Rust Yxx\assistant-server"
.\test-api.ps1
```

只测单项：

```powershell
.\test-api.ps1 -Test health
.\test-api.ps1 -Test register
.\test-api.ps1 -Test login
.\test-api.ps1 -Test message
.\test-api.ps1 -Test settings
```

---

## 4. 常用开发命令

```powershell
# 编译检查
cargo check

# 构建
cargo build

# 发布构建
cargo build --release

# 运行已编译程序
.\target\debug\assistant-server.exe
```

---

## 5. 数据文件管理

```powershell
# 查看数据目录
ls .\data

# 查看用户
type .\data\users.json

# 查看会话
type .\data\sessions.json

# 重置本地数据（谨慎）
Remove-Item .\data\*.json -Force
```

---

## 6. 常见问题

### 6.1 端口 8080 被占用

```powershell
netstat -ano | findstr :8080
taskkill /PID <PID> /F
```

### 6.2 PowerShell 不允许执行脚本

```powershell
Set-ExecutionPolicy -Scope Process Bypass
```

### 6.3 改完代码后无法编译

```powershell
cargo clean
cargo check
```

---

## 7. 推荐阅读顺序

1. `assistant-server/QUICKSTART.md`
2. `assistant-server/test-api.ps1`
3. `assistant-server/COMMANDS.md`

以上三份文档已经和当前“无数据库、文件存储”方案对齐。