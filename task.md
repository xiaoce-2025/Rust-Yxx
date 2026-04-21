## **系统架构设计：跨平台智能助手 Web 服务**

### **1. 核心架构图**

```mermaid
graph TB
    subgraph "客户端 (用户浏览器)"
        Browser[浏览器]
    end
    
    subgraph "Rust Web 服务器"
        Server[Rust HTTP Server]
        Auth[认证中间件]
        Router[路由分发]
        
        subgraph "核心模块"
            Assistant[智能助手核心]
            PluginMgr[插件管理器]
            Scheduler[任务调度器]
        end
        
        subgraph "数据层"
            DB[(数据库)]
            Cache[(Redis缓存)]
            File[文件存储]
        end
        
        subgraph "第三方集成"
            WeChatAPI[微信服务]
            LLMAPI[AI模型API]
        end
    end
    
    Browser -->|HTTP/WS| Server
    Server --> Auth
    Auth --> Router
    Router --> Assistant
    Router --> PluginMgr
    Assistant --> PluginMgr
    PluginMgr --> Assistant
    
    Assistant --> DB
    Assistant --> Cache
    PluginMgr --> File
    
    Assistant --> LLMAPI
    Scheduler --> WeChatAPI
```

---

## **2. 详细技术实现方案**

### **2.1 多用户系统设计**

```rust
// src/models/user.rs
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub password_hash: String,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// 用户配置表 - 每个用户的个性化设置
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserConfig {
    pub user_id: Uuid,
    pub openai_api_key: Option<String>,  // 用户自己的API密钥
    pub anthropic_api_key: Option<String>,
    pub default_model: String,
    pub theme: String,
    pub settings: serde_json::Value,  // 扩展配置
}

// 用户会话管理
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UserSession {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub expires_at: DateTime<Utc>,
    pub user_agent: Option<String>,
    pub ip_address: Option<String>,
}
```

### **2.2 数据隔离策略**

```rust
// src/core/auth.rs
pub async fn get_current_user(
    Extension(pool): Extension<PgPool>,
    TypedHeader(auth_header): TypedHeader<Authorization<Bearer>>,
) -> Result<User, ApiError> {
    let token = auth_header.token();
    let session = sqlx::query_as!(
        UserSession,
        "SELECT * FROM user_sessions WHERE session_id = $1 AND expires_at > NOW()",
        Uuid::parse_str(token)?
    )
    .fetch_optional(&pool)
    .await?;
    
    if let Some(session) = session {
        let user = sqlx::query_as!(
            User,
            "SELECT * FROM users WHERE id = $1 AND is_active = true",
            session.user_id
        )
        .fetch_one(&pool)
        .await?;
        
        Ok(user)
    } else {
        Err(ApiError::Unauthorized)
    }
}

// 所有数据操作都注入用户上下文
#[derive(Clone)]
pub struct UserContext {
    pub user_id: Uuid,
    pub db: PgPool,
}

// 在路由处理器中自动注入用户上下文
pub async fn get_user_conversations(
    user_ctx: UserContext,
) -> Result<Json<Vec<Conversation>>, ApiError> {
    // 自动限制只能查询当前用户的数据
    let convs = sqlx::query_as!(
        Conversation,
        "SELECT * FROM conversations WHERE user_id = $1 ORDER BY updated_at DESC",
        user_ctx.user_id
    )
    .fetch_all(&user_ctx.db)
    .await?;
    
    Ok(Json(convs))
}
```

### **2.3 微信绑定模块设计**

```rust
// src/services/wechat.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct WeChatService {
    app_id: String,
    app_secret: String,
    redirect_uri: String,
    http_client: reqwest::Client,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WeChatUserInfo {
    pub openid: String,
    pub nickname: Option<String>,
    pub headimgurl: Option<String>,
    pub unionid: Option<String>,
}

impl WeChatService {
    pub fn new(app_id: String, app_secret: String, redirect_uri: String) -> Self {
        Self {
            app_id,
            app_secret,
            redirect_uri,
            http_client: reqwest::Client::new(),
        }
    }
    
    // 1. 生成微信授权URL
    pub fn generate_auth_url(&self, state: &str) -> String {
        format!(
            "https://open.weixin.qq.com/connect/qrconnect?\
            appid={}&\
            redirect_uri={}&\
            response_type=code&\
            scope=snsapi_login&\
            state={}#wechat_redirect",
            self.app_id,
            urlencoding::encode(&self.redirect_uri),
            state
        )
    }
    
    // 2. 通过code获取access_token和openid
    pub async fn get_access_token(&self, code: &str) -> Result<(String, String), WeChatError> {
        let url = format!(
            "https://api.weixin.qq.com/sns/oauth2/access_token?\
            appid={}&\
            secret={}&\
            code={}&\
            grant_type=authorization_code",
            self.app_id, self.app_secret, code
        );
        
        let resp = self.http_client.get(&url).send().await?;
        let token_info: TokenResponse = resp.json().await?;
        
        Ok((token_info.access_token, token_info.openid))
    }
    
    // 3. 获取用户信息
    pub async fn get_user_info(&self, access_token: &str, openid: &str) -> Result<WeChatUserInfo, WeChatError> {
        let url = format!(
            "https://api.weixin.qq.com/sns/userinfo?\
            access_token={}&\
            openid={}&\
            lang=zh_CN",
            access_token, openid
        );
        
        let resp = self.http_client.get(&url).send().await?;
        let user_info: WeChatUserInfo = resp.json().await?;
        
        Ok(user_info)
    }
    
    // 4. 绑定微信到用户账户
    pub async fn bind_wechat_to_user(
        &self,
        db: &PgPool,
        user_id: Uuid,
        wechat_info: &WeChatUserInfo,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            INSERT INTO user_wechat_bindings (user_id, openid, nickname, avatar_url, unionid, bound_at)
            VALUES ($1, $2, $3, $4, $5, NOW())
            ON CONFLICT (user_id, openid) DO UPDATE SET
                nickname = EXCLUDED.nickname,
                avatar_url = EXCLUDED.avatar_url,
                updated_at = NOW()
            "#,
            user_id,
            wechat_info.openid,
            wechat_info.nickname,
            wechat_info.headimgurl,
            wechat_info.unionid,
        )
        .execute(db)
        .await?;
        
        Ok(())
    }
}
```

### **2.4 微信绑定的API接口设计**

```rust
// src/routes/wechat.rs
use axum::{
    extract::{Query, State},
    response::{Html, Redirect},
    Json,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct WechatAuthQuery {
    pub code: String,
    pub state: String,
}

#[derive(Debug, Deserialize)]
pub struct BindWechatRequest {
    pub code: String,
    pub state: String,
}

pub struct WechatRouter;

impl WechatRouter {
    pub fn routes() -> Router {
        Router::new()
            // 1. 获取微信登录URL（用于前端引导用户跳转）
            .route("/api/wechat/auth-url", get(Self::get_auth_url))
            // 2. 微信回调接口（微信服务器调用）
            .route("/api/wechat/callback", get(Self::callback))
            // 3. 绑定微信到当前用户（用户设置页面调用）
            .route("/api/wechat/bind", post(Self::bind_wechat))
            // 4. 解绑微信
            .route("/api/wechat/unbind", delete(Self::unbind_wechat))
            // 5. 获取绑定状态
            .route("/api/wechat/binding-status", get(Self::get_binding_status))
    }
    
    // 生成微信授权URL
    async fn get_auth_url(
        State(state): State<AppState>,
        user_ctx: UserContext,
    ) -> Result<Json<WechatAuthUrlResponse>, ApiError> {
        // 生成一个安全的state参数，防止CSRF
        let csrf_state = generate_random_string(16);
        
        // 存储state到Redis，设置短时间过期
        state
            .redis
            .set_ex(
                format!("wechat:state:{}", csrf_state),
                &user_ctx.user_id.to_string(),
                300, // 5分钟过期
            )
            .await?;
        
        let auth_url = state.wechat_service.generate_auth_url(&csrf_state);
        
        Ok(Json(WechatAuthUrlResponse { auth_url }))
    }
    
    // 微信回调处理器
    async fn callback(
        State(state): State<AppState>,
        Query(query): Query<WechatAuthQuery>,
    ) -> Result<Html<String>, ApiError> {
        // 1. 验证state参数
        let user_id_str: String = state
            .redis
            .get(format!("wechat:state:{}", query.state))
            .await?
            .ok_or(ApiError::InvalidWechatState)?;
        
        let user_id = Uuid::parse_str(&user_id_str)?;
        
        // 2. 用code换取access_token和openid
        let (access_token, openid) = state
            .wechat_service
            .get_access_token(&query.code)
            .await?;
        
        // 3. 获取用户信息
        let wechat_user = state
            .wechat_service
            .get_user_info(&access_token, &openid)
            .await?;
        
        // 4. 绑定到用户账户
        state
            .wechat_service
            .bind_wechat_to_user(&state.db, user_id, &wechat_user)
            .await?;
        
        // 5. 返回成功页面
        Ok(Html(include_str!("../templates/wechat_success.html")))
    }
    
    // 绑定微信（从用户设置页面调用）
    async fn bind_wechat(
        State(state): State<AppState>,
        user_ctx: UserContext,
        Json(req): Json<BindWechatRequest>,
    ) -> Result<Json<BindWechatResponse>, ApiError> {
        // 验证state（防止重放攻击）
        let expected_user_id: String = state
            .redis
            .get(format!("wechat:bind:state:{}", req.state))
            .await?
            .ok_or(ApiError::InvalidWechatState)?;
        
        if expected_user_id != user_ctx.user_id.to_string() {
            return Err(ApiError::InvalidWechatState);
        }
        
        // 后续流程与callback类似...
        
        Ok(Json(BindWechatResponse { success: true }))
    }
}
```

### **2.5 前端集成（设置页面）**

```html
<!-- templates/settings.html 中的微信绑定部分 -->
<div class="wechat-settings">
    <h3>微信绑定</h3>
    
    <div v-if="!wechatBound">
        <p>尚未绑定微信账号</p>
        <button @click="bindWechat" class="btn btn-primary">
            <i class="icon-wechat"></i> 绑定微信
        </button>
    </div>
    
    <div v-else>
        <div class="wechat-profile">
            <img :src="wechatAvatar" alt="微信头像" class="avatar">
            <div>
                <p>已绑定微信: {{ wechatNickname }}</p>
                <p>OpenID: {{ wechatOpenId }}</p>
            </div>
        </div>
        <button @click="unbindWechat" class="btn btn-danger">
            解绑微信
        </button>
    </div>
</div>
```

```javascript
// 前端微信绑定逻辑
async function bindWechat() {
    try {
        // 1. 获取微信授权URL
        const response = await fetch('/api/wechat/auth-url', {
            method: 'GET',
            credentials: 'include'
        });
        
        const { auth_url } = await response.json();
        
        // 2. 弹出微信授权窗口
        const width = 500, height = 600;
        const left = (window.screen.width - width) / 2;
        const top = (window.screen.height - height) / 2;
        
        const wechatWindow = window.open(
            auth_url,
            '微信授权',
            `width=${width},height=${height},top=${top},left=${left}`
        );
        
        // 3. 监听绑定结果
        const checkResult = setInterval(async () => {
            if (wechatWindow.closed) {
                clearInterval(checkResult);
                // 检查绑定状态
                await checkWechatStatus();
            }
        }, 1000);
        
    } catch (error) {
        console.error('微信绑定失败:', error);
    }
}

async function checkWechatStatus() {
    const response = await fetch('/api/wechat/binding-status');
    const status = await response.json();
    updateWechatUI(status);
}
```

### **2.6 数据库表设计**

```sql
-- 用户微信绑定表
CREATE TABLE user_wechat_bindings (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    openid VARCHAR(128) NOT NULL,
    unionid VARCHAR(128),
    nickname VARCHAR(100),
    avatar_url TEXT,
    bound_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
    UNIQUE(user_id, openid)
);

CREATE INDEX idx_wechat_openid ON user_wechat_bindings(openid);
CREATE INDEX idx_wechat_unionid ON user_wechat_bindings(unionid);

-- 用户API密钥表（加密存储）
CREATE TABLE user_api_keys (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider VARCHAR(50) NOT NULL,  -- 'openai', 'anthropic', 'google', etc.
    encrypted_key TEXT NOT NULL,     -- 加密后的API密钥
    iv BYTEA NOT NULL,               -- 加密IV
    is_active BOOLEAN DEFAULT true,
    last_used_at TIMESTAMP WITH TIME ZONE,
    created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT NOW(),
    UNIQUE(user_id, provider)
);

-- 用户插件配置表
CREATE TABLE user_plugin_configs (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    plugin_id VARCHAR(100) NOT NULL,
    config JSONB NOT NULL DEFAULT '{}',
    enabled BOOLEAN DEFAULT true,
    UNIQUE(user_id, plugin_id)
);
```

### **2.7 配置文件示例**

```toml
# config/development.toml
[database]
url = "postgres://user:pass@localhost/assistant_db"

[server]
address = "127.0.0.1"
port = 8080
frontend_dir = "./frontend-dist"

[redis]
url = "redis://localhost:6379"

[wechat]
app_id = "YOUR_WECHAT_APP_ID"
app_secret = "YOUR_WECHAT_APP_SECRET"
redirect_uri = "http://localhost:8080/api/wechat/callback"

[security]
session_secret = "your-session-secret-key-here-change-in-production"
jwt_secret = "your-jwt-secret-key-here-change-in-production"
api_key_encryption_key = "32-byte-key-for-aes-256-gcm-!!!"

[cors]
allowed_origins = ["http://localhost:3000", "http://localhost:8080"]
allow_credentials = true
```

### **2.8 部署与启动脚本**

```bash
#!/bin/bash
# deploy.sh

# 编译（跨平台）
if [[ "$OSTYPE" == "linux-gnu"* ]]; then
    cargo build --release --target x86_64-unknown-linux-musl
    BINARY="./target/x86_64-unknown-linux-musl/release/assistant-server"
elif [[ "$OSTYPE" == "msys" || "$OSTYPE" == "win32" ]]; then
    cargo build --release
    BINARY="./target/release/assistant-server.exe"
else
    echo "Unsupported OS"
    exit 1
fi

# 复制配置文件
cp config/production.toml ./

# 创建必要目录
mkdir -p data/{uploads,plugins,logs}

# 设置环境变量
export RUST_LOG=info,assistant_server=debug
export DATABASE_URL="postgres://..."

# 运行数据库迁移
if [ "$1" = "migrate" ]; then
    sqlx database create
    sqlx migrate run
fi

# 启动服务
exec $BINARY
```

```toml
# Dockerfile
FROM rust:1.70 as builder
WORKDIR /usr/src/app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/src/app/target/release/assistant-server /usr/local/bin/
COPY config/docker.toml /etc/assistant/config.toml
EXPOSE 8080
CMD ["assistant-server"]
```

### **2.9 核心优势总结**

1. **真正的跨平台**：同一份代码编译后，在Windows和Linux上都能运行
2. **数据完全隔离**：每个用户有自己的数据库空间、API密钥、插件配置
3. **企业级安全**：
   - API密钥加密存储
   - CSRF保护
   - 会话管理
   - SQL防注入
4. **完整的微信集成**：
   - OAuth2.0标准流程
   - 绑定/解绑功能
   - 防重放攻击
5. **可扩展架构**：
   - 插件系统支持WASM沙箱
   - 支持多种AI提供商
   - 模块化设计