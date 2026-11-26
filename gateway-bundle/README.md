# WB 免费模型池（多上游自动路由网关）

一个 OpenAI 兼容端点，内部按质量分组把请求路由到 15 家上游的免费模型，
遇到限流 / 5xx / 超时 / 空响应 / 鉴权失败时自动降级到下一个候选。

除了填 key 的上游，网关还带**三个网页额度平台**（OrcaTerm / 豆包 / Antigravity），
它们用的是账号自己在平台上的免费额度，走各自的登录方式，见[下文](#三个网页额度平台)。

```
客户端 ──POST /v1/chat/completions──▶ 127.0.0.1:8901
                                        │
                    ┌───────────────────┼───────────────────┐
                    ▼                   ▼                   ▼
              NVIDIA NIM(直连)      智谱(直连)        OpenRouter(代理)
              Gemini(代理)          OFM 本地网关      魔搭/LongCat/Groq...
```

## 快速开始

```bash
# 状态：版本、当前分组、候选顺序、跳过原因、各平台登录态
curl -s http://127.0.0.1:8901/health | python3 -m json.tool

# 看每家上游的 key 是否就位
curl -s http://127.0.0.1:8901/providers | python3 -m json.tool

# 调用
curl -s http://127.0.0.1:8901/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"auto","messages":[{"role":"user","content":"hi"}]}'

# 点名某家的某个模型
-d '{"model":"nvidia::moonshotai/kimi-k3.1-pro", ...}'
```

端点：`http://127.0.0.1:8901/v1/chat/completions` · 密钥随意填（本机无鉴权）

手动启动（常驻方式失效时的兜底）：

```bash
cd ~/.wb-checkin-gateway && ./run.sh        # macOS
cd %USERPROFILE%\.wb-checkin-gateway && run.cmd   # Windows
```

## 配置面板（填 key、加渠道、试连）

```bash
./panel.sh            # 用 Chrome 应用窗口打开，像一个独立工具
# 或直接浏览器开 http://127.0.0.1:8901/admin
```

面板里能做的事，改完即时生效（就地重建队列，不重启进程、不中断请求）：

| 动作 | 效果 |
|---|---|
| 每行填 key → 保存 | 写回 `config.json`，该渠道自动解除跳过；刚拿到 key 的渠道把前 2 个模型放进当前分组 |
| 测试 | 真发一次请求量延迟，只回显结果，不写进网关记账、不影响冷却 |
| 启用 / 停用 | 等价于改 `disabled` |
| 编辑 | 改 endpoint / 代理 / 模型清单 / 备注（模型行支持 `id \| 显示名`） |
| 新增自定义渠道 | 任意 OpenAI 兼容上游，填 endpoint + key + 模型即入队，模型自动进 `all` 分组 |
| 顶部分组胶囊 | 切 `high` / `standard` / `all` |

面板只监听 `127.0.0.1`，且拒绝非本机来源的跨站写请求。

**和 WB-Checkin 的关系**：WB-Checkin 0.11 起把整套网关打进 app，首次启动解包到这个目录并注册常驻
（macOS 是 LaunchAgent，Windows 是 `run.cmd` + 按 `.gateway.pid` 杀进程）。app 里通过 Tauri IPC →
`src-tauri/src/freehub.rs` 读 `/admin/api/*`，所以两个入口看到的是同一份 `config.json`：应用里改完，
浏览器面板刷新即可见；反之亦然。app 里的四个标签页（自定义模型 / OrcaTerm / 豆包 / Antigravity）就是这个网关的界面。

升级时 app 会逐文件比对脚本内容并重启常驻进程 —— `launchd` 只在进程退出时重读脚本，不重启就会一直跑旧路由。

> 注：模型摆渡（OC Ferry）读的是 `sidecar/src/backend.js` 里写死的 `provider.id === 'opencode'`，
> 与本网关无关——那边的模型来自 OpenCode Zen 池，这边是各家平台自己的 key，两条路互不影响。

## 15 家上游现状

`key` 列 ✓ 表示已配好并实测通过；`—` 表示缺 key，网关会自动跳过，填上即生效。

| provider | key | 状态 | 免费档最强模型 | 额度 | 国内直连 |
|---|---|---|---|---|---|
| `nvidia` | ✓ | **可用** | nemotron-3-ultra-550b / minimax-m3.1-flash / super-vl-120b | 40 RPM，无日上限 | ✅ 0.5s |
| `ofm` | ✓ | **可用** | kilo-auto/free | 本机已有网关 | ✅ 本地 |
| `openrouter` | ✓ | 额度耗尽 | nemotron-3-ultra-550b:free / dots-3-note:free | 50 次/天（充 $10 → 1000） | ⚠️ 6s 慢 |
| `zhipu` | ✓ | 可用但慢 | glm-4.5-flash / glm-4.7-flash | ~100 万 tokens/天 | ✅ |
| `gemini` | ✓ | 可用但慢 | gemini-4-hydrogen / 3.5-flash | Flash 约 1500 RPD | ❌ **必须代理** |
| `modelscope` | — | 待填 key | Qwen3.5-235B / DeepSeek-V4-Flash | **每天 2000 次**（按次不计 token） | ✅ |
| `longcat` | — | 待填 key | LongCat-2.5（多模态） | 每日 5000 万 tokens 口径 | ✅ |
| `groq` | — | 待填 key | gpt-oss-120b / qwen3.8-27b | 30 RPM / 1000 RPD / 200K TPD | ❌ CDN 403，走代理 |
| `sambanova` | — | 待填 key | DeepSeek-V3.2 / gpt-oss-120b | 20 RPM / 20 RPD 每模型 | ✅ |
| `unorouter` | — | 待填 key | gemini-3.8-flash:free / deepseek-v4.1-flash:free | 每模型 ~1 RPM | ✅ |
| `sensenova` | — | 待填 key | SenseNova-V6-Pro | 公测免费 1500 次/5 小时 | ✅ |
| `cloudflare` | — | 待填 endpoint+key | @cf/openai/gpt-oss-120b | 10,000 Neurons/天 | ⚠️ |
| `deepseek` | ✓ | **已失效** | deepseek-chat | 付费 | ✅ |
| `volc` | ✓ | **已失效** | GLM-5.2 | 付费 | ✅ |
| `xai` | ✓ | **已失效** | grok-4.5 | 付费 | ❌ |

### 缺 key 的怎么补

改 `config.json` → `providers.<名字>.apiKey`，然后重载：

```bash
launchctl kickstart -k gui/$(id -u)/ai.wbgateway.openrouter-free
```

| provider | 去哪拿 key |
|---|---|
| `modelscope` | https://www.modelscope.cn/my/myaccesstoken （手机号注册，每天 2000 次） |
| `longcat` | https://longcat.chat （需个人实名） |
| `groq` | https://console.groq.com/keys （Google/GitHub 登录，无需卡） |
| `sambanova` | https://cloud.sambanova.ai/apis |
| `unorouter` | https://unorouter.com/en/token （Google/GitHub/Discord 登录） |
| `sensenova` | https://platform.sensenova.cn |
| `cloudflare` | https://dash.cloudflare.com/profile/api-tokens （endpoint 里的 `<ACCOUNT_ID>` 也要换） |

### 已失效的三家

不是缺 key，是 key 本身不能用，已设 `disabled: true` 避免每次请求白等：

| provider | 实测报错 | 修法 |
|---|---|---|
| `deepseek` | 401 `api key ****4bb6 is invalid` | 换新 key 后 `disabled` 改 `false` |
| `volc` | 400 `CodingPlan 订阅已过期`（账号 2124854726） | 续订后改回 |
| `xai` | 403 `no credits or licenses`，且直连不通 | 充值 + 保留 proxy 后改回 |

## 三个网页额度平台

这三个不走「填 key」，走的是**账号在平台网页版的登录态**，消耗该账号的免费额度。模型名带平台前缀：
`orcaterm/auto`、`doubao/deepseek-v4-1-flash`、`antigravity/auto` 这样。

| 平台 | 实现 | 登录 | 上游 |
|---|---|---|---|
| OrcaTerm | `orcaterm_auth.mjs` | 独立 Chrome 扫码，取 `orcaterm` 域 cookie 与 uin | 平台自己的网关，模型 ID 必须带 `TokenHub/` 或 `Hunyuan3/` 前缀 |
| 豆包 | `doubao_bridge.mjs` | 独立 Chrome 登录 doubao.com，取 cookie + 设备指纹 | `POST /chat/completion`，在页面上下文里 `fetch` 以拿到站点 JS 生成的签名 |
| Antigravity | `antigravity.mjs` | OAuth PKCE + 本地回调，存 refresh_token | `v1internal:streamGenerateContent`，逐帧转成 OpenAI chunk |

**豆包那层为什么绕。** 页面内请求靠站点自己的 JS 注入 `a_bogus` / `msToken`，所以必须在浏览器里发，不能纯 HTTP 重放。由此带来三件事：

1. 浏览器必须**有头** —— 豆包按浏览器特征判定，`--headless` 一律回 `710022004`；启动后用 CDP（`Browser.setWindowBounds`）最小化，运行期还会检查窗口状态，被 restore 就压回去。
2. 页面自己是活的：它通过 IM 长连接看到会话里有一条「有问无答」，会按 recovery 逻辑把同一条再发一遍，导致**一次提问扣两次额度**。所以桥接挂了 CDP `Fetch` 域只监听 `/chat/completion`，带 `is_recovery` 的请求直接 `failRequest`，我们那条 `continueRequest`。
3. 上游帧的正文与思考要按**块归属**分：`block_type=10000` 的 `parent_id` 指向思考容器（`10040`）就算思考，否则算正文；顶层 `{"text": ...}` 增量跟随当前打开的块。早先用 `10040` 出现次数的奇偶来判，必然错位（容器会来 start/update/finish 好几次），结果是正文被截断、思考内容被当成答案发出去。

**会话按客户端隔离。** 不是全局复用一条豆包会话：`config.doubao.sessions[指纹] = {id, at, count}`，指纹取首条用户消息，用消息总数判断是不是新会话（同一会话内只增不减，变少就是另开了）。否则新会话里问同一句，豆包会回「用户重复查询，此前已完成答复」。

**限流与风控。** `710022002` / `710022004` / `710012001` 会标成 `accountWide`，整组冷却 30 分钟并在本轮跳过同 provider，避免反复触发。登录器与桥接的 stderr/stdout 都落到 `doubao-login.log` / `orcaterm-login.log`，排错先看这两个。

## 质量分组

`tier.sh` 切换，改完自动重载并打印队列。

| 分组 | 候选数 | 收录标准 |
|---|---|---|
| `high`（默认） | 8 | 只收实测 1s–14s 能稳定出结果的，日常主力 |
| `standard` | 15 | 加上慢但可用的（gemini-4-hydrogen、glm-4.5-flash、step-5） |
| `all` | 40 | 全部登记项，含已知超时/失效的，用来探测渠道恢复 |

## 改配置后怎么重载

改完 `config.json` 需要重启常驻进程才会生效（macOS 的 `launchd` 只在进程退出时重读脚本与配置）：

```bash
launchctl kickstart -k gui/$(id -u)/ai.wbgateway.openrouter-free    # macOS
```

Windows 上由 app 按 `.gateway.pid` 走 `taskkill /PID` 再重拉，**不要用 `taskkill /IM node.exe`** —— 那会把你自己的 node 一起杀掉。

改上游 key、加渠道、切分组、试连都建议直接走面板（`http://127.0.0.1:8901/admin` 或 app 里的「自定义模型」标签），改完就地重建队列，不中断请求。

## 接口

| 路径 | 说明 |
|---|---|
| `POST /v1/chat/completions` | 主接口，支持 `stream`、`tools`、图片、`models` 多候选 |
| `GET /health` | 队列顺序、每个候选的冷却/成功/失败计数 |
| `GET /providers` | 每家上游的 key 是否就位、入队几个、被跳过的原因 |
| `GET /v1/models` | 当前分组下的模型列表 |

响应头 `x-gateway-model` / `x-gateway-provider` / `x-gateway-fallbacks` 说明这次是谁接的、降级了几次。

## 几个刻意的设计

**思考型模型不会返回空白。** kimi / step / muse / mistral-large / lightning 这类模型会把
`max_tokens` 吃在 `reasoning_content` 上，`content` 是 `null`。网关认出这种情况，把思考内容
兜底填进 `content`（原字段保留），否则客户端会显示一条空回复。

**账号级额度 vs 模型级限流分开处理。** OpenRouter 的 `free-models-per-day` 是账号级，
命中后换模型没用，网关直接返回 429 并说明「次日 08:00 重置」；NVIDIA 的 40 RPM 是模型级，
只冷却 60 秒然后换下一个模型继续。

**缺 key 不算失败。** 没配 apiKey 的 provider 在启动时就被剔出队列，`/health` 里的 `skipped`
计数能看出来，不会让请求消耗在必然 401 的候选上。

**网关专用字段不外发。** `models`（多候选）这类字段只有本网关认，透传给 NVIDIA 会直接
400 `Unsupported parameter(s): models`，所以发请求前统一删掉。

**单模型可覆盖超时。** muse-glimmer 要 63s，给它单独配 `timeoutMs: 90000`，不拖累其他模型。

## 2026-11-24 实测记录

可用：nemotron-3-ultra-550b 3.8s · minimax-m3.1-flash 1.6s · nano-omni 1.2s ·
bolt-e4b 0.7s · laguna-xs 0.7s · super-vl-120b 14.0s · mistral-large-4-1t 10.2s ·
step-5-preview 10.3s · muse-glimmer 63.8s · ofm 2.2s · glm-4.5-flash 8.2s ·
glm-4.7-flash 37.6s · gemini-4-hydrogen 37.7s · gemini-3.5-flash 23.6s

临时失败：kimi-k3.1-pro / glm-5.3-flash / gemma-4-31b-it（NVIDIA 侧两轮 120s 无响应）、
gemini-3.8-flash（503 高需求）、glm-4.6v-flash（429 访问量过大）

## 目录

app 解包时只复制 `src-tauri/src/freehub.rs` 里 `BUNDLE_FILES` 列出的文件 —— **新增脚本必须同步改那个清单**，否则新机器上解包后没有这个文件。

```
config.json          15 家上游 + 三个分组 + 三个平台的配置（唯一配置入口）
gateway.mjs          网关本体，零依赖
doubao_bridge.mjs    豆包：CDP 驱动浏览器、拼请求、解析帧
orcaterm_auth.mjs    OrcaTerm 扫码登录器
antigravity.mjs      Antigravity OAuth 反代
admin.html           配置面板（浏览器打开 /admin）
run.sh               macOS 启动包装（自动找 node、探测可执行性）
README.md            本文件
```

运行时生成（不进仓库）：

```
node / node.exe      运行时（Windows 版由 CI 构建时下载）
run.cmd              Windows 手动启动入口
.gateway.pid         进程号，重启按它精确杀进程
.bundle-version      app 写入的包版本，/health 的 version 取自它
.inject-status.json  最近一次模型注入结果
gateway.log          运行日志
stdout.log stderr.log
doubao-login.log     豆包登录器输出（排错先看这个）
orcaterm-login.log
.doubao-chrome-profile  .orcaterm-chrome-profile   独立浏览器 profile
```
