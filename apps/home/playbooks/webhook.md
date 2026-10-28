配置 webhook 推送（钉钉 / 飞书）

先判断：只是「往群里发消息」→ 用群机器人 webhook（本文）；要读应用数据（审批/日历/通讯录）→ 那是 OAuth 应用，另说。

**回答「怎么配置」时：先给 1-2-3 步骤，让主人完成第 2 步（放凭据），然后说「弄好告诉我，我来验证」。不要自己在沙盒里猜 URL，也不要试图读取凭据内容。**

## 钉钉（加签模式）

1. 主人操作：群 → 设置 → 机器人 → 添加机器人 → 自定义 → 安全设置勾选「加签」→
   复制 **Webhook 地址**（含 `access_token=`）和 **SEC 开头的密钥**。
2. 主人放凭据（两行：第 1 行 access_token，第 2 行 SEC）：

```bash
sudo install -d -m 0750 -o root -g chuan /etc/chuan/secrets
sudo sh -c 'printf "%s\n%s\n" "<access_token>" "<SEC...>" > /etc/chuan/secrets/dingtalk'
sudo chown root:chuan /etc/chuan/secrets/dingtalk
sudo chmod 0440 /etc/chuan/secrets/dingtalk
sudo -u chuan head -n1 /etc/chuan/secrets/dingtalk   # 能读到才算对
```

`access_token` 只是 webhook 里 `access_token=...` 那段，不要带域名、不要整条 URL。

3. 你来做（运行期注册，不用重启）：

```
service(action="add", name="dingtalk",
        base="https://oapi.dingtalk.com/robot/send",
        token_file="/etc/chuan/secrets/dingtalk", header="",
        query={access_token="{token}", timestamp="{ts_ms}", sign="{sign}"},
        sign={canonical="{ts_ms}\n{secret}"})
```

4. 验证：

```
http(service="dingtalk", method="POST", path="/robot/send",
     body={msgtype="text", text={content="chuan 连接测试"}})
```

`errcode` 为 0 才算成功；`310000` 是签名或关键词不匹配，先检查时间戳与 SEC。

## 飞书（自定义机器人 + 签名校验）

1. 主人操作：群 → 设置 → 群机器人 → 添加 → 自定义机器人 → 安全设置选「签名校验」，
   复制 Webhook（形如 `https://open.feishu.cn/open-apis/bot/v2/hook/<hook id>`）与 secret。
2. 凭据两行：第 1 行 **hook id**（路径最后那段，不是整条 URL），第 2 行 secret。
   命令同钉钉，文件名换成 `feishu`。
3. 注册：

```
service(action="add", name="feishu", base="https://open.feishu.cn",
        token_file="/etc/chuan/secrets/feishu", header="",
        body={timestamp="{ts_s}", sign="{sign}"},
        sign={canonical="{ts_s}\n{secret}"})
```

飞书用**秒**且签名放 **body**；钉钉用**毫秒**且放 **query**，不要弄反。

4. 验证：

```
http(service="feishu", method="POST", path="/open-apis/bot/v2/hook/{token}",
     body={msg_type="text", content={text="chuan 连接测试"}})
```

返回 `code` 或 `StatusCode` 为 0 才算成功。

## 关键词模式（主人不想加签时）

机器人安全设置选「自定义关键词」，消息文本里必须包含该关键词；此时**不需要 secret**，
也不配 `sign`：`service(add, ..., query={access_token="{token}"})` 即可。

## 通用规则

- **不要索取、读取、复述 token/secret 的值**；你只处理文件路径，`{token}` `{sign}` 由宿主替换。
- 不要猜 URL：钉钉只有一个入口 `/robot/send`；飞书 hook id 在路径上，用 `{token}` 占位。
- 签名由宿主算：不要写脚本自己算 HMAC，也不要把 secret 写进技能或记忆。
- 失败先看返回体：`310000` / `19021` 多为签名或关键词问题；`400` 多半是 body 结构不对。
- 成功后 `remember` 一条：`service/dingtalk = 已配置，凭据 /etc/chuan/secrets/dingtalk`，
  下次直接 `http(service="dingtalk", ...)`，不必重新问主人。
