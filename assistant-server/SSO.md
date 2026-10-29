# 外部统一认证客户端

此目录仅实现小助手的 OIDC 接入。认证中心独立部署，其源码和账户数据不属于本仓库。

由认证中心管理员登记机密客户端和准确的回调地址，再通过服务环境配置：

```text
SSO__ENABLED=true
SSO__ISSUER=https://auth.example.com
SSO__CLIENT_ID=yxx_autonote
SSO__CLIENT_SECRET=由管理员提供，不得提交到Git
SSO__REDIRECT_URI=https://assistant.example.com/auth/sso/callback
```

使用进程环境或只保存在服务器上的受限 EnvironmentFile 提供密钥，不要写入已被 Git 跟踪的 development.toml。显式使用 HTTP 时额外设置 `SSO__ALLOW_INSECURE_HTTP=true`；默认不允许非本机 HTTP。

用户从 `/auth/sso/start` 进入，浏览器仅携带短期授权码回调。小助手后端负责客户端密钥、PKCE、state、nonce、签名、issuer/audience/过期时间以及 UserInfo subject 校验，不信任浏览器直接提交的身份。

每次统一认证登录仍须通过小助手自己的 `data/whitelist.json`。稳定绑定 issuer + subject，首次可关联同一已验证邮箱的原有用户，保留旧密码、数据和配额。仅接受受信任认证中心签发的已验证邮箱。

`/auth/admin` 是“通过北京大学IAAA认证登录”入口，仅显示 IAAA 登录，不再提供小助手独立账户的登录或注册界面。它与外部认证中心的管理后台不是同一入口。
