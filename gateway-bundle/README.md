# WB 免费模型池（多上游自动路由网关）

一个 OpenAI 兼容端点，内部按质量分组把请求路由到 15 家上游的免费模型，
遇到限流 / 5xx / 超时 / 空响应 / 鉴权失败时自动降级到下一个候选。

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
cd ~/.wb-checkin-gateway
./toggle.sh status            # 状态 + 各候选冷却情况
./toggle.sh on | off          # 手动开关

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

**和 WB-Checkin 的关系**：WB-Checkin 0.10.9 起自带「自定义模型」标签（`src/components/FreeHubPanel.tsx`），
它通过 Tauri IPC → `src-tauri/src/freehub.rs` 读这个网关的 `/admin/api/*`，所以两个入口看到的是同一份
`config.json`：应用里改完，浏览器面板刷新即可见；反之亦然。网关没起来时那个标签显示「网关未运行」并给一个启动按钮。

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

## 质量分组

`tier.sh` 切换，改完自动重载并打印队列。

| 分组 | 候选数 | 收录标准 |
|---|---|---|
| `high`（默认） | 8 | 只收实测 1s–14s 能稳定出结果的，日常主力 |
| `standard` | 15 | 加上慢但可用的（gemini-4-hydrogen、glm-4.5-flash、step-5） |
| `all` | 40 | 全部登记项，含已知超时/失效的，用来探测渠道恢复 |

## 命令

```bash
cd ~/.wb-checkin-gateway
./toggle.sh on|off|status     # 开关 + 状态
./tier.sh high|standard|all   # 切分组（不带参数列出全部）
node probe.mjs --tier all     # 全渠道探测（只测有 key 的）
node probe.mjs --provider nvidia --gap 1700   # 单家探测，NVIDIA 要放宽间隔防 429
```

`probe.mjs` 逐个发最小请求，报告 HTTP 码 / 耗时 / 内容或错误原因，末尾给可用数与失败清单。

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

```
config.json      15 家上游 + 三个分组的全部配置（唯一配置入口）
gateway.mjs      网关本体，零依赖，外发走 /usr/bin/curl
probe.mjs        全渠道探测
toggle.sh        开关
tier.sh          分组切换
run.sh           启动包装（自动找 node）
gateway.log      运行日志
```

`*.bak-single-provider` 是改造前的单上游版本，回滚用。
