【重要提醒】
本文档参照20251215版的认证逻辑编写，确保在OAuthLogin.js认证请求版本为"20251215"的情况下可正常使用，其余情况请自行验证。

流程：IAAA账密认证获取token→相关app使用token登录

【IAAA认证】
```
curl "https://iaaa.pku.edu.cn/iaaa/oauthlogin.do" \
  -H "Content-Type: application/x-www-form-urlencoded; charset=UTF-8" \
  --data-raw "appid=【此处填入应用id】blackboard&userName=【此处填入学号】&password=【此处填入密码（明文）】&redirUrl=【填入重定向url】"
```
注意，此处的应用id会和重定向url在后端进行一致性校验，二者必须全部正确且匹配

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

【20251212更新】
自2025年12月12日起，IAAA支持密文和明文两种密码传输方式，在IAAA登陆页面中有
```
// 获取RSA公钥
$.ajax('/iaaa/getPublicKey.do',
    {
        dataType:"json",
        success : function(data,status,xhr) {
            var json = data;
            if(true == json.success){
                crypt = new JSEncrypt();
                crypt.setPublicKey(json.key); // 设置公钥
            }
        },
        error : function(xhr,status,error) {
        }
});
// 进行RSA加密
var encryptedPWD = $("#password").val();
if(crypt!=null)
    encryptedPWD = crypt.encrypt($("#password").val()); // 使用公钥加密密码
// 随后在提交的 data 中使用 encryptedPWD
```
推测在后续更新中可能会停用明文传输方式，建议使用密文进行密码传输

RSA公钥可通过访问
```
https://iaaa.pku.edu.cn/iaaa/getPublicKey.do
```
获得，返回格式如

```
{"success":true,"key":"-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAqw9PsMk8v9ED\/LiLT62I\nDnelyIA\/s8blyxqNmbgXT4xtq+Y64Bd+THYPZ4dUIRuFmMvPowQm9wL27W3PEtQy\nC8VN+TzW\/nPzc74fy9cRxgaSh1FXNQBqYZtltb6G5YvwBvZlYdKhE3Oo3noUD0FJ\nJC11Nmcy2\/x1V2pwXHRy2DHKaWB1EEtQ9dRxuMZolZIpEwWnT4CHfwEvth83kNRp\nE8471KJEqyQqmqJt3JRerH4X4p41zQFIxCsrznAwku3b1qm0vgGLQ8t7XEiCjDX0\nm5yIJEuW5t1YcteutuJX5+5oXxe2Fo04Wkn1pO6+QoJopqHcHJD5C+7GlnPOLB1c\nDQIDAQAB\n-----END PUBLIC KEY-----"}
```


【北大教学网】

应用id：blackboard

重定向url：http://course.pku.edu.cn/webapps/bb-sso-BBLEARN/execute/authValidate/campusLogin

【北京大学党员教育学习平台】

应用id：dangxiao

重定向url：http://dangxiao.pku.edu.cn/user/casLogin

【北大选课网】

应用id：syllabus

重定向url：http://elective.pku.edu.cn:80/elective2008/ssoLogin.do

