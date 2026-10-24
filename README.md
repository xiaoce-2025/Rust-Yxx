# Rust-Yxx

Rust-Yxx 是一个基于 **Rust + Axum** 的助手服务项目，包含用户认证、对话接口、微信通道和教学网视频相关能力。

## 环境要求

- Rust（建议 stable，1.75+）
- Cargo

## 安装方式

1. 克隆仓库并进入服务目录：

```bash
git clone https://github.com/xiaoce-2025/Rust-Yxx.git
cd Rust-Yxx/assistant-server
```

2. 准备配置文件：

```bash
cp .env.example .env
cp config/development.toml config/local.toml
```

3. 按需在 `config/local.toml` 或环境变量中填写你的配置（如 API Key）。

4. 编译并启动：

```bash
cargo build
cargo run
```

默认监听地址见配置文件 `config/development.toml`（当前默认 `0.0.0.0:8090`）。

## 简要使用介绍

服务启动后可通过以下方式快速体验：

1. 访问页面入口：`http://127.0.0.1:8090/`
2. 注册/登录后调用助手接口：`POST /api/assistant/send`
3. 查看健康状态：`GET /health`

项目内更多接口与说明可参考：

- `assistant-server/README.md`
- `assistant-server/QUICKSTART.md`
