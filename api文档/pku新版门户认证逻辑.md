【重要提醒】
本文档参照仓库中的 20261113 版 IAAA 登录脚本。

流程：IAAA账密认证获取token→相关app使用token登录

【IAAA认证】
```
curl "https://iaaa.pku.edu.cn/iaaa/oauthlogin.do" \
  -H "Content-Type: application/x-www-form-urlencoded; charset=UTF-8" \
  --data-urlencode "appid=【填入应用id】" \
  --data-urlencode "userName=【此处填入学号】" \
  --data-urlencode "password=【使用本次公钥加密后的 Base64 密文】" \
  --data-urlencode "keyVersion=【同一个公钥响应中的 version】" \
  --data-urlencode "remTrustChk=false" \
  --data-urlencode "redirUrl=【填入重定向url】"
```
注意，此处的应用id会和重定向url在后端进行一致性校验，二者必须全部正确且匹配

提交格式为 `application/x-www-form-urlencoded`，必须显式包含 `remTrustChk`，值为字符串 `false`（不是 JSON 布尔值，也不要省略）。上例为教学网配置：`appid=blackboard`，`redirUrl` 使用上面的 HTTP 地址；其他应用应替换为各自匹配的应用 id 与回调地址。所有字段值均需进行表单编码，尤其是密码密文中的 `+`、`/`、`=` 等字符。示例中的 `--data-urlencode` 会自动完成编码。

返回格式如：

成功：
```
{"success":true,"token":"xxxx"}
```


失败：
```
{"success":false,"errors":{"code":"xxx","msg":"xxx"}}
```

登录成功后在重定向url后添加?token=xxxx参数即可。实际操作中会有防加载缓存的_rand参数，为了模仿真实情景可以添加此参数。如：
```
http://course.pku.edu.cn/webapps/bb-sso-BBLEARN/execute/authValidate/campusLogin?_rand=【随机数，0-1之间，小数点后16位】&token=【填入token】
```

【公钥与版本】

认证时从 `https://iaaa.pku.edu.cn/iaaa/getPublicKey.do` 获取公钥。响应示例中的 PEM 已省略正文：

```json
{"success":true,"key":"-----BEGIN PUBLIC KEY-----\n…\n-----END PUBLIC KEY-----","version":"202512"}
```

确认 `success` 为 `true` 后，用同一响应的 `key` 加密密码，并将 `version` 原样作为 `keyVersion` 随 `oauthlogin.do` 表单提交。示例中的 `202512` 是上游返回的版本，公钥和版本可能按月更新，应以实时响应为准。

保存绑定时一并记录密文、公钥和公钥版本。使用绑定凭据发起新认证前，先拉取当前公钥并核对绑定记录。公钥或已记录的版本发生变化时，请用户重新绑定。

【北大教学网】

应用id：blackboard

重定向url：http://course.pku.edu.cn/webapps/bb-sso-BBLEARN/execute/authValidate/campusLogin

【北京大学党员教育学习平台】

应用id：dangxiao

重定向url：http://dangxiao.pku.edu.cn/user/casLogin

【北大选课网】

应用id：syllabus

重定向url：http://elective.pku.edu.cn:80/elective2008/ssoLogin.do

