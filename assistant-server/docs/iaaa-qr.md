# IAAA 扫码登录

支持严小希应用的 `/auth/admin` 登录页和 `/teaching-login` 一次性教学网认证页。点击“扫码登录”后，用北京大学 App 扫码并确认。二维码有效期约 3 分钟；过期或失败时可刷新，切换到账号登录或离开页面会停止轮询。

实现依据：`api文档/OAuthRefresh.js`、`api文档/OAuthLogin_20261113.js`，以及现有教学网登录配置。两种入口均使用 `appID=blackboard`，IAAA 回调参数为 `http://course.pku.edu.cn/webapps/bb-sso-BBLEARN/execute/authValidate/campusLogin`；实际消费票据时沿用教学网现有 HTTPS 回调。

- 初始化 `oauth.jsp`，生成 `genQRCode.do?userName=&_rand=…&appId=blackboard`。同一事务的初始化、二维码生成、轮询和回调共享独立 Cookie jar 和同一 UA。
- 每 3 秒 POST `oauthlogin4QRCode.do`，提交 `appId=PKUApp`、`issuerAppId=iaaa`、`targetAppId=blackboard` 和上述 `redirectUrl`。`E10` 表示继续等待；`E99`、`isStop=是`、账号未激活或次数过多时停止。
- 票据只在服务端使用一次。成功回调后，从已认证教学网 `/learn/api/public/v1/users/me` 响应的 `userName` 确认 10 位学号，不接受浏览器提交身份。
- 严小希登录复用现有账户创建、白名单与停用账户校验，建立系统会话后丢弃临时教学网 Cookie。一次性教学网认证要求已有系统登录与页面 CSRF 校验，仅把教学网会话存入当前用户的临时教学网缓存，不绑定账号密码。
- 浏览器请求使用随机事务 ID 与 HttpOnly、SameSite=Strict Cookie 双重关联。两个入口使用不同 Cookie；教学网事务额外绑定当前系统用户。事务过期或取消时清理临时 Cookie，服务重启后需重新扫码。上游票据和 Cookie 不返回浏览器。

本地接口为 `/api/auth/iaaa/qr/{start,poll,cancel}` 和 `/api/teaching-session/qr/{start,poll,cancel}`，均为 JSON POST。`start` 请求 `{}`；`poll`、`cancel` 请求 `{"id":"start 返回的事务 ID"}`。`start` 返回二维码图片 data URL；`poll` 返回 `pending`、`stopped` 或 `success`，认证失败通过现有错误响应返回。
