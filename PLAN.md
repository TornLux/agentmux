# agentmux 实施方案

## 0. 项目目标

把本地的 Claude Code 改造成一个"始终在线的多会话 agent 服务":

- **在家在电脑前**:打开 Windows Terminal → 选 / 创建 session → 看到 claude 原生 TUI,完整体验。
- **离开电脑出门在外**:通过 IM(Discord / Telegram / QQ 等)继续给任意 session 下指令,关键事件推送到手机。
- **回家**:打开 Terminal 重新接入任意 session,看到当前画面,无缝继续。
- **多 session 并行**:Discord 在跟 session A 对话的同时,QQ 可以在 session B 跑别的任务,Terminal 可以再开 session C,互不干扰。
- **进程独立于客户端**:任何 Terminal / IM 断开,claude 进程都继续活着。
- **可选自然语言驱动**:在 IM 用 `/ai` 前缀(初期默认)或全局自动路由(稳定后切换)让小模型替你解释命令,不强制记忆 `!cmd` 语法。

### 非目标

- 不做"把 claude 整个 TUI 镜像到 IM",IM 只承载语义事件(回答完成、需要审批、错误等),不模拟终端画面。
- 不分发给其他用户,自用。短期内不做账号系统、计费等。
- 不修改 Microsoft Terminal 源码 —— Terminal 始终是未改的官方版本。

---

## 1. 系统架构总览

### 进程图

```
Task Scheduler (开机触发)
    │
    ├─▶ broker.exe  ──────────────────────────────────────────┐
    │   (常驻,会话管理器)                                      │
    │                                                          │
    │     ┌── session "default"  ConPTY ── claude (子进程)     │
    │     ├── session "blog"     ConPTY ── claude (子进程)     │
    │     ├── session "exp"      ConPTY ── claude (子进程,可  │
    │     │                                hibernate 状态)     │
    │     └── ...(上限 max_sessions,默认 5)                   │
    │                                                          │
    │     ├── named pipe \\.\pipe\claude-broker                │
    │     │     ▲ 多客户端,每个绑定到一个 session             │
    │     │     └── claude-attach.exe (Terminal 启动时显示菜单 │
    │     │           让用户选 attach 哪个 session 或新建)     │
    │     │                                                    │
    │     ├── HTTP :8765                                       │
    │     │     ▲ ▲                                            │
    │     │     │ └── hook 脚本(claude 触发,带 session_id)   │
    │     │     └─── 控制平面 (sessions CRUD / interrupt 等)   │
    │     │                                                    │
    │     └── WebSocket :8766                                  │
    │           ▲                                              │
    │           └── 多个 platform-bot 进程订阅                 │
    │                                                          │
    └─▶ platform-discord.exe / platform-telegram.exe /         │
        platform-qq.{exe,py} ... (各自常驻,各自绑频道)        │
              ▲                                                │
              └── Discord/TG/QQ 网关 ── 你的手机                │
```

### 关键不变量

1. **claude 进程是 broker 的子进程**,生命周期跟 broker 绑定,不跟 Terminal 或 IM 绑定。
2. **broker 是会话管理器** —— 多 session 并行,每 session 有独立的 PTY、claude、缓冲区、状态。
3. **broker 是字节传输层**:只搬 PTY 字节,不解释 claude 的 TUI 内容。
4. **IM 桥是事件层**:通过 Claude Code hooks 拿到结构化事件,带 session_id 路由,不解析屏幕。
5. **Terminal 不被修改**:仅配置 profile 跑 `claude-attach.exe`,菜单 UI 在 attach 程序内部。
6. **平台扩展无关 broker**:加一个 IM 平台 = 加一个独立 bot 进程,broker 不动。

### tmux 类比

整套系统的会话模型借用 tmux,先有这个心智模型再读后面的细节会顺得多:

| tmux 概念 | 本项目 |
|---|---|
| tmux server(常驻) | broker |
| tmux session | 一个 claude 进程 + ConPTY + 环形缓冲区 + 元数据 |
| tmux client(attach) | claude-attach(Terminal) 或某个 platform-bot 的频道绑定 |
| `tmux send-keys C-c` | IM `!stop` 命令(broker 写 `0x03` 到目标 session 的 PTY) |
| `tmux new -s xxx` | IM `!new <name>` |
| `tmux attach -t xxx` | IM `!attach <name>` 或 attach 程序的菜单选择 |
| `tmux ls` | IM `!ls` |
| `tmux kill-session` | IM `!kill <name>` |
| `tmux detach` | Terminal 关窗口 / IM `!detach` |

---

## 2. 组件规格

### 2.1 broker(会话管理器)

常驻后台守护进程。**整套系统的心脏。**

#### 职责

| 职责 | 说明 |
|---|---|
| Session 生命周期管理 | create / attach / detach / kill / rename / hibernate / resume,上限 `max_sessions` |
| 托管多个 ConPTY | 每 session 一个 ConPTY,跑 `claude --dangerously-skip-permissions [--resume <claude-session-id>]` |
| 环形缓冲区(每 session 一份) | 保存最近 ~500KB PTY 输出原始 ANSI,新 viewer 接入时回放 |
| 缓冲区裁剪 | 识别 `ESC[?1049h/l`、`ESC[2J ESC[H` 等关键序列,在合适时刻丢弃旧字节 |
| Named pipe 服务 | `\\.\pipe\claude-broker`,viewer 通过 HELLO 帧选择目标 session |
| 客户端绑定表 | 每个客户端"当前盯着哪个 session",支持运行时切换 |
| 多 viewer fan-out | 每 session 内,所有 attach 在该 session 的 viewer 同步收到字节流 |
| 输入合流 | 同一 session 多个客户端的输入按到达顺序写到该 session 的 PTY stdin |
| Resize 协调 | 同一 session 多个 viewer 尺寸不同时,以最小列/行裁剪,避免 claude 超出小窗口 |
| HTTP 控制平面(`:8765`) | sessions CRUD、interrupt、hook event 接入、IM 控制命令 |
| WebSocket 事件总线(`:8766`) | platform-bot 订阅事件(带 session_id),broker 按订阅过滤推送 |
| Session 持久化 | `sessions.toml`,跨重启 `auto_resume` |
| Claude 监管 | claude 异常退出 → 记录 → 自动 `--resume` 拉起;频繁崩溃则禁用 auto-restart |
| Hibernate | idle 时间超阈值的 session 自动关闭 claude 进程,保留元数据,attach 时 resume |
| 优雅关闭 | `/shutdown` → 给所有 session 的 claude 发 SIGTERM → 等 grace → 关 PTY → 退出 |

#### 内部结构(组件级)

```
broker/
├── pty            — ConPTY 包装(每 session 一个实例)
├── ringbuf        — 带 ANSI 边界感知的环形缓冲区
├── session        — Session 结构 + 生命周期方法
├── manager        — Sessions 集合 + 绑定表 + 资源限制
├── pipe_server    — named pipe 监听 + 客户端注册 + 路由
├── http_server    — axum 路由(hook event / sessions API / 控制)
├── ws_server      — platform-bot 订阅 + 推送
├── persist        — sessions.toml 读写
├── shutdown       — 全局协调退出
└── main           — 启动顺序 + tokio runtime
```

#### 关键 tokio 模式

- 每 session 独占一个 `tokio::sync::broadcast`,给该 session 内多 viewer fan-out PTY 字节
- 每 session 独占一个 `tokio::sync::mpsc`,各输入源(viewer / IM)的字节合流写 PTY
- broker 主循环 `tokio::select!` 同时监听 pipe accept、HTTP/WS、内部控制信号、各 session 的 PTY 退出事件

---

### 2.2 Session(每个 session 的内部模型)

```rust
pub struct Session {
    pub id: SessionId,                  // 内部 UUID,稳定标识
    pub name: String,                    // 用户起的名字: "default" / "blog"
    pub claude_session_id: String,       // claude 自己的 session id,给 --resume 用
    pub cwd: PathBuf,                    // 启动时的工作目录
    pub state: SessionState,
    pub created_at: SystemTime,
    pub last_activity: SystemTime,
    pty: Option<ConPty>,                 // None = hibernated
    claude_pid: Option<u32>,
    ring_buf: RingBuffer,
    output_tx: broadcast::Sender<Bytes>,
    input_tx: mpsc::Sender<Bytes>,
    attached: HashSet<ClientId>,
    auto_resume_on_boot: bool,
}

pub enum SessionState {
    Idle,                       // 在跑 / 等用户
    Busy,                        // 模型正在生成 / 工具正在执行
    AwaitingInput,               // claude 出了问题等用户回复
    AwaitingApproval,            // 危险命令前置审批挂起
    Hibernated,                  // 元数据保留,claude 进程已关
    Crashed { reason: String },  // 异常退出
}
```

**SessionState 的更新来源**:

- `Busy` / `Idle`:hooks(`PreToolUse` 触发 → Busy;`Stop` 触发 → Idle)
- `AwaitingInput`:`Notification` hook
- `AwaitingApproval`:`PreToolUse` 命中危险命令,broker 自己设
- `Hibernated`:idle 超时 / 手动
- `Crashed`:claude 子进程退出码非 0

---

### 2.3 claude-attach(Terminal viewer,带菜单)

Terminal profile 启动的小程序。**纯字节透传 + 启动时 session 选择菜单。**

#### 启动流程

```
$ claude-attach.exe   (Terminal 启动 profile 时调用)
        │
        ▼
连接 \\.\pipe\claude-broker 的"控制通道"(HELLO 帧 mode=control)
        │
        ▼
GET /sessions 拉列表
        │
        ▼
渲染菜单(在 stdout / 用 ratatui 之类轻量 TUI 库):
  ┌──────────────────────────────────────────────┐
  │ Active sessions (broker @ 127.0.0.1:8765):   │
  │   1. default     busy    30s ago             │
  │   2. blog        idle    12m ago             │
  │   3. exp-rust    waiting 2m ago              │
  │   ────────────────────────────────────────   │
  │   n. <new session>                            │
  │   q. quit                                     │
  │ Choose [1-3/n/q]: _                           │
  └──────────────────────────────────────────────┘
        │
        ▼ 用户按 1
切换到"PTY 透传通道"(HELLO 帧 mode=rw, session=default)
        │
        ▼
进入 raw mode 透传,直到退出
```

#### 命令行参数

```
claude-attach.exe                    # 显示菜单
claude-attach.exe --session <name>   # 跳过菜单直接 attach
claude-attach.exe --new [<name>]     # 直接创建新 session 并 attach
claude-attach.exe --readonly         # 只接收输出,不发输入(围观模式)
claude-attach.exe --debug            # 把 frame 收发日志写 stderr
```

#### 必须做对的 4 件事(透传阶段)

1. **stdin 进 raw mode**(`SetConsoleMode` 关 `ENABLE_PROCESSED_INPUT`)。否则 Ctrl+C 会被 Windows 当信号杀掉 claude-attach 自己。
2. **stdout 进 VT 模式**(`ENABLE_VIRTUAL_TERMINAL_PROCESSING`)。让 ANSI 序列被 Terminal 正确渲染。
3. **字节级 1:1 透传**(除特殊键外,不缓冲不解码)。
4. **resize 带外**。窗口大小变化通过控制帧通知 broker;attach 时立即发一次当前尺寸。

#### Ctrl+C 语义

| 操作 | 行为 |
|---|---|
| 单按 Ctrl+C | `0x03` 字节透传给 claude → claude 自己处理(中断当前任务) |
| 1.5s 内连按两次 Ctrl+C | 拦截第二次,POST `/sessions/{id}/restart` → 该 session 的 claude 重启 |
| 1.5s 内连按三次 Ctrl+C | POST `/shutdown` → broker 整体关闭(连带所有 session) |
| Ctrl+\\ (`0x1c`) | detach hotkey,只关 attach 自己,不动后端 |
| Ctrl+D (`0x04`) | 透传,claude 收到 EOF → 优雅退出当前 session 的 claude |

#### 大致代码量

Rust 实现 ~250 行(含菜单 UI),产物 ~500KB exe。

---

### 2.4 IM bot 体系(imbot-core + 平台适配器)

支持多 IM 平台扩展,采用"一平台一进程 + 共享渲染层"的模式。

#### 拓扑

```
            broker WS :8766
              ▲ ▲ ▲ ▲
              │ │ │ └── platform-slack.exe   (将来)
              │ │ └──── platform-qq.py       (OneBot 协议)
              │ └────── platform-telegram.exe
              └──────── platform-discord.exe
```

每个 bot 是独立进程,互不影响。broker 对客户端数量无感。

#### imbot-core(共享 Rust crate)

平台无关层,所有 Rust 实现的 platform-bot 都依赖它。Python 实现的 bot(如 QQ)用等价 JSON schema 自行实现。

##### 核心 trait

```rust
pub trait Platform: Send + Sync {
    fn caps(&self) -> &Caps;
    
    async fn render(
        &self,
        intent: RenderIntent,
        ctx: &mut RenderCtx,
    ) -> Result<RenderOutcome>;
    
    async fn next_inbound(&mut self) -> Option<InboundEvent>;
}

pub struct Caps {
    pub can_edit: bool,
    pub can_react: bool,
    pub can_reply: bool,
    pub can_button: bool,
    pub can_upload: bool,
    pub max_chars: usize,
    pub edit_rate: RateLimit,
    pub markdown: MarkdownFlavor,    // None | CommonMark | Discord | TGv2 | CQ
}
```

##### 语义意图(broker event → RenderIntent)

```rust
pub enum RenderIntent {
    StreamingUpdate {
        thread_key: ThreadKey,        // 业务键(如 turn id),不是平台 message id
        body_markdown: String,
        committed: bool,
    },
    OneShot {
        body_markdown: String,
        priority: Priority,            // High = 必须 ping,Low = 静默
    },
    Bulk {
        title: String,
        body: String,
        suggested_form: BulkForm,
    },
    Decision {
        prompt: String,
        choices: Vec<Choice>,
        timeout_ms: u32,
        decision_id: String,
    },
}
```

##### 渲染降级链(在 imbot-core 写一次,所有平台共享)

| Intent | caps 全开 | 缺 edit | 缺 button | 缺 file | 全没有 |
|---|---|---|---|---|---|
| StreamingUpdate | edit 同一条 | 节流后只发最终版 | — | — | 同上 |
| OneShot | 直接发 + mention | — | — | — | 同上 |
| Bulk | file 附件 | — | — | 切片成多条带 (1/3) 标号 | 同上 |
| Decision | 发文本 + ✅❌ 反应 | — | 改成"回复 Y/N" 文本 parse | — | 文本 parse |

##### RenderCtx(平台无关的状态)

- `thread_key → 该平台的 message handle` 映射(给 edit 用)
- `decision_id → 该平台的 message handle + 监听状态`(给审批用)
- 限速队列(per-platform 不同)

#### 各平台 binary 职责

只需要实现:
1. `Platform` trait(SDK 调用)
2. 解析 inbound 事件转 `InboundEvent`
3. 加载本平台的配置(token、频道、用户白名单)
4. 连接 broker WS,启动 imbot-core 主循环

#### 频道-Session 绑定

每个 platform-bot 内存里维护:

```rust
struct ChannelBindings {
    map: HashMap<ChannelId, SessionId>,
}
```

- 持久化到 broker 的 `bindings.toml`(每个频道当前绑哪个 session),bot 重启不丢。
- 用户在频道发普通消息 → 路由到 binding 的 session 当 user prompt。
- 命令以 `!` 前缀,不进 claude,由 bot 直接处理(如 `!attach`、`!new` 等,见第 3.4 节)。

---

### 2.5 hook 脚本

由 Claude Code hooks 系统触发的短命进程。**事件过滤层 + 多 session 路由。**

#### 配置

`~/.claude/settings.json`:

```json
{
  "hooks": {
    "Stop": [
      { "hooks": [{ "type": "command", "command": "C:\\agent\\hooks\\hook-stop.exe" }]}
    ],
    "Notification": [
      { "hooks": [{ "type": "command", "command": "C:\\agent\\hooks\\hook-notification.exe" }]}
    ],
    "PreToolUse": [
      { "matcher": "Bash",
        "hooks": [{ "type": "command", "command": "C:\\agent\\hooks\\hook-bash-pre.exe" }]}
    ],
    "PostToolUse": [
      { "matcher": "Bash",
        "hooks": [{ "type": "command", "command": "C:\\agent\\hooks\\hook-bash-post.exe" }]}
    ]
  }
}
```

#### 多 session 路由(关键改动)

broker 启动每个 session 的 claude 时,用 env var 标记:

```rust
Command::new("claude")
    .env("AGENT_SESSION_ID", session.id.to_string())
    .env("AGENT_BROKER_URL", "http://127.0.0.1:8765")
    .args(&["--dangerously-skip-permissions",
            "--resume", &session.claude_session_id])
    .current_dir(&session.cwd)
    .spawn()
```

claude 把 env 传给 hook 子进程,hook 启动后读环境:

```rust
let session_id = std::env::var("AGENT_SESSION_ID")?;
let broker_url = std::env::var("AGENT_BROKER_URL")?;
// POST 时带上 session_id
```

broker 收到事件后路由:**只把该 session 相关的事件推给 attach 在该 session 的 platform-bot**。

#### 各 hook 语义

| hook | 行为 |
|---|---|
| `hook-stop` | 读 transcript jsonl 末尾 → 提取最新 assistant 消息 → POST `/event` `{session_id, type: "assistant_message", body}` |
| `hook-notification` | POST `/event` `{session_id, type: "notification", message}`(claude 等用户输入) |
| `hook-bash-pre` | 检查命令模式 → 命中危险:① 查 `/sessions/{id}/state`,有本地 Terminal viewer 在场则放行;② 否则 POST `/approval-request`,**阻塞**等结果,拒绝则退出码 2 阻塞 claude |
| `hook-bash-post` | 仅 `exit_code != 0` 时 POST `/event` `{session_id, type: "tool_error", ...}` |

#### "本地在场静音"机制(per-session 粒度)

每个 hook 第一步:`GET /sessions/{session_id}/state` → 检查 `local_viewer_attached: true` → 立即 `exit 0`。

注意:这是 per-session 的判断。session A 有本地 Terminal,IM 不收 A 的事件;但如果同时 session B 没有本地 Terminal,IM 仍然正常收到 B 的事件。

---

### 2.6 router(LLM 命令路由器)

`imbot-core` 的可选模块,把"自然语言"翻译成结构化命令。各 platform-bot 在 inbound 处理路径上调用。

#### 触发模式(可配置,渐进策略)

| 模式 | 触发条件 | 用途 |
|---|---|---|
| `Off` | 不启用 router,只认 `!` 前缀命令 | 故障应急 / 离线 |
| `Prefix` | 仅以 `/ai ` 开头的消息走 router | **初期默认**,先验证准确率与成本 |
| `Always` | 非 `!` 开头的消息全部走 router | 稳定后切换,获得最自然体验 |

**演进路径**:从 `Prefix` 起步 → 用一段时间观察:命中率、误判率、月成本、延迟体感都可接受 → 改 config 切到 `Always`。`Off` 永远是 fallback 选项。

#### 工具集(对模型暴露的 function)

每个工具背后是一次 broker HTTP 调用,tool name 与 broker API 一一对应:

```jsonc
[
  {"name": "list_sessions"},
  {"name": "create_session",  "args": {"name": "?",  "cwd": "?"}},
  {"name": "attach_session",  "args": {"name": "required"}},
  {"name": "detach"},
  {"name": "interrupt"},                   // 当前 session 发 0x03
  {"name": "restart_claude"},
  {"name": "kill_session",    "args": {"name": "required"}},
  {"name": "rename_session",  "args": {"new_name": "required"}},
  {"name": "get_status",      "args": {"name": "?"}},
  {"name": "forward_to_claude","args": {"text": "required"}},   // 关键:无操作出口
  {"name": "ask_clarification","args": {"question": "required"}} // 关键:模糊出口
]
```

`forward_to_claude` 和 `ask_clarification` 是关键的"非 meta"出口,让模型在意图模糊或本就是给 claude 的话时不要硬编 meta 命令。

#### system prompt 模板

```
你是 agentmux 的命令路由器。用户在 IM 里说的话,你判断该调哪个工具。

当前状态:
  当前频道绑定:{binding}
  所有 sessions:
{sessions_list}

规则:
  - 操作 session 系统(切换/新建/停止/查询):调对应工具
  - 给 claude 干活(写代码、问问题等):调 forward_to_claude
  - 模糊不清:调 ask_clarification,不要瞎猜
  - 多步意图按顺序调多个工具
  - 破坏性操作(kill_session / restart_claude / shutdown)必须有强证据

不要解释,直接调工具。
```

#### 模型选择

| 选项 | 单次成本 | 适用 |
|---|---|---|
| **Haiku 4.5** | ~$0.0008/次 | 日常默认 |
| Sonnet 4.6 | ~$0.0026/次 | 高准确率场景或两层路由 fallback |
| Opus | — | 不使用,过贵 |

**默认 Haiku**。`Prefix` 模式下大约 1k 次/月,~$0.8;`Always` 模式估 ~$24/月。

#### 多步意图执行

模型单轮可返回多个 tool_use,bot 顺序执行,每步回报:

```
你: "/ai 开个新 session 叫 blog,让 claude 列 outline"
   ↓
Haiku: [create_session{name:"blog"}, forward_to_claude{text:"列 outline"}]
   ↓
bot 执行 + 回报:
   ✅ 创建并切换到 session "blog"
   ✅ 转发给 claude:"列 outline"
   💬 等待 claude 回答…
```

#### 失败降级

| 故障 | 行为 |
|---|---|
| API 超时(>3s) | 退回 "当 prompt 转发到当前 session",IM 显示 ⚠️ "router 不可用,已转发" |
| Daily rate limit 触顶 | 同上 + 提示 "今日 router 配额用完,改用 ! 命令或继续转发" |
| 模型返回的 tool 不存在 | 拒绝执行,IM 显示原始 tool name + 建议 |
| 破坏性 tool(kill/shutdown/restart) | 执行前需用户 ✅ 反应二次确认 |

#### 实现位置

```
imbot-core/
└── src/
    └── router/
        ├── mod.rs           pub fn classify(msg, state) -> Vec<ToolCall>
        ├── tools.rs         tools schema 定义
        ├── prompt.rs        system prompt 渲染
        ├── client.rs        Anthropic API 客户端 (reqwest)
        └── executor.rs      tool_call → broker HTTP 调用
```

各 platform-bot 在 inbound 处理里:

```rust
async fn handle_inbound(msg: InboundMessage, state: &BotState) -> Action {
    if msg.text.starts_with("!") {
        return parse_structured(&msg.text);
    }
    match config.router_mode {
        RouterMode::Off => Action::ForwardAsPrompt(msg.text),
        RouterMode::Prefix => {
            if let Some(rest) = msg.text.strip_prefix("/ai ") {
                router.classify(rest, state).await
            } else {
                Action::ForwardAsPrompt(msg.text)
            }
        }
        RouterMode::Always => router.classify(&msg.text, state).await,
    }
}
```

---

## 3. 协议设计

### 3.1 viewer ↔ broker(named pipe)

二进制帧格式:

```
+--------+-----------+----------------+
| u8 tag | u32 len   | payload (len B)|
+--------+-----------+----------------+

tag = 0x01  PTY_DATA          双向。payload 是裸 PTY 字节
tag = 0x02  RESIZE             viewer→broker。payload = u16 cols, u16 rows
tag = 0x03  HELLO              viewer→broker。见下面
tag = 0x04  REPLAY_END         broker→viewer。环形缓冲区回放完毕
tag = 0x05  ATTACH             viewer→broker。运行时切换目标 session
tag = 0x06  CONTROL            viewer→broker。中断 / 重启 / shutdown
tag = 0x07  EVENT              broker→viewer。带外通知(如 session 状态变化)
```

HELLO payload(JSON):

```json
{
  "client_id": "uuid",
  "client_kind": "terminal" | "discord" | "telegram" | "qq",
  "mode": "control" | "rw" | "ro",
  "session": "name-or-id"     // null 时:control 模式不绑,rw/ro 模式 broker 拒绝
}
```

ATTACH payload:`{"session": "name"}` 切换当前客户端绑定的 session,broker 重置该客户端的字节流到新 session 的环形缓冲区回放。

CONTROL payload:`{"cmd": "interrupt" | "restart-claude" | "shutdown"}`,作用于 HELLO 时绑定的 session(shutdown 例外,作用全局)。

### 3.2 hook ↔ broker(HTTP `:8765`)

```
GET    /sessions                       → list,各 session 状态
POST   /sessions                       → create {name, cwd?, claude_args?}
GET    /sessions/{id}                  → details
DELETE /sessions/{id}?force=true       → kill
POST   /sessions/{id}/interrupt        → 写 0x03 到 PTY stdin
POST   /sessions/{id}/restart          → 杀 claude,--resume 同 session id
POST   /sessions/{id}/input            → {bytes_base64}
POST   /sessions/{id}/rename           → {new_name}
POST   /sessions/{id}/hibernate        → 立即 hibernate
GET    /sessions/{id}/state            → {state, local_viewer_attached, attached_clients[]}
GET    /sessions/{id}/transcript       → 当前 session 的 jsonl 路径

POST   /event                          → hook 事件入口,body: {session_id, type, ...}
POST   /approval-request               → hook 阻塞调用,body: {session_id, command, deadline_ms}
                                          → 200 {approved, reason}

GET    /state                          → 全局状态
POST   /shutdown                       → 关 broker(连带所有 session)
```

### 3.3 platform-bot ↔ broker(WebSocket `:8766`)

JSON 消息双向。**所有事件都带 session_id,bot 自己决定要不要给当前频道用。**

```jsonc
// broker → bot
{"type": "assistant_message", "session_id": "uuid", "body": "..."}
{"type": "notification",      "session_id": "uuid", "message": "..."}
{"type": "tool_error",        "session_id": "uuid", "command": "...", "stderr": "..."}
{"type": "approval_request",  "session_id": "uuid", "id": "uuid", "command": "...", "deadline_ms": 60000}
{"type": "session_state",     "session_id": "uuid", "state": "Busy" | ...}
{"type": "session_created",   "session_id": "uuid", "name": "..."}
{"type": "session_killed",    "session_id": "uuid"}

// bot → broker
{"type": "input",              "session_id": "uuid", "bytes_base64": "..."}
{"type": "approval_response",  "id": "uuid",         "approved": true, "by": "user_id"}
{"type": "command",            "name": "list_sessions" | "create_session" | ..., "args": {...}}
```

#### 订阅过滤

bot 连接 WS 后可以发:

```jsonc
{"type": "subscribe", "session_ids": ["uuid-a", "uuid-b"]}
```

broker 只推这些 session 的事件给该 bot,以及全局事件(如 session_created)。bot 频道切换 binding 时更新订阅。

### 3.4 IM 用户命令体系

每个 platform-bot 必须实现的标准命令:

| 命令 | 行为 |
|---|---|
| `!ls` / `!sessions` | 列所有 session(名字、状态、最近活动、绑定的 IM/Terminal) |
| `!new [<name>] [-cwd <path>]` | 创建并 attach 到新 session;name 不给则自动 `s1`/`s2` |
| `!attach <name>` | 当前频道切换到指定 session |
| `!detach` | 当前频道解绑,后续消息不进任何 session |
| `!stop` | 给当前 session 发 Ctrl+C(中断任务,保留 session) |
| `!restart` | 当前 session 的 claude 重启(保留 claude_session_id 用 --resume) |
| `!kill <name>` | 销毁 session(二次确认:bot 加 ✅ 反应,用户点击) |
| `!rename <new>` | 改当前 session 名字 |
| `!status` | 当前 session 详情(状态、最近输出片段、attached clients) |
| `!who` | 哪些客户端 attach 在哪些 session |
| `!help` | 列出所有命令 |

非命令消息默认进当前频道绑定的 session 当 user prompt。

#### `!ls` 渲染示例(Discord 用 embed 或 codeblock)

```
🟢 sessions (3/5)
─────────────────────────────────────────────
 ▶ default      ⚙️  busy        last: 30s ago    @discord
   blog         💤 idle         last: 12m ago
   exp-rust     ⚠️ awaiting     last: 2m ago     @qq
─────────────────────────────────────────────
▶ = 你当前 attach 的 session
@xxx = 哪个 IM/Terminal 绑在该 session 上
```

#### 首次发消息无绑定时

```
你: 帮我看一下这个 bug
bot:
   ⚠️ 你还没绑定 session,请选择:
     react ▶️ 连接到 default (busy, @discord)
     react 🆕 创建新 session
     或回复 !ls 查看所有
```

---

## 4. 关键设计决策与权衡

### 4.1 hooks 而非屏幕扫描做 IM 桥

**决策**:IM 端事件源是 Claude Code hooks,不是 PTY 字节流。

**为什么**:hooks 在语义边界触发,自带结构化数据。屏幕扫描需要 VT 模拟器 + 边界启发式 + 工具块识别,脆弱且 claude 升级 TUI 后会坏。

**代价**:streaming 中间状态拿不到,只有 turn 结束后的完整回复。对 IM 反而是优点,不刷屏。

### 4.2 不修改 Microsoft Terminal 源码

**决策**:Terminal 用未改的官方版本,只配 profile 跑 `claude-attach.exe`。

**为什么**:Terminal 是 ConPTY 之上的 UI 壳,我们要做的事都在 ConPTY 之外。改 Terminal 等于把 WinRT/XAML/Remoting 的复杂度全揽进来,毫无收益。

### 4.3 Ctrl+C 多击语义

**决策**:1 次 = 透传中断、2 次 = 重启当前 session 的 claude、3 次 = 关整个 broker。

**为什么**:三种语义递增,对应"中断任务 / 重置 session / 收工",每种都是真实需求,但频率不同(中断高频、重启偶尔、关整体很少)。

### 4.4 输出经 hooks,输入经 PTY 注入

**决策**:claude → IM 走 hooks;IM → claude 走 PTY stdin 写入。

**为什么**:hooks 是事件出口不是入口(`UserPromptSubmit` 是用户提交后触发,不能用来代用户提交)。输入方向必须有 PTY 注入路径,这意味着 broker 必须是 PTY 宿主。

### 4.5 PTY 输入流和审批流分离

**决策**:危险命令的批准不走 PTY 输入流,而是 hook 阻塞 + broker 异步等待 + IM 反馈。

**为什么**:模拟 claude 的"y/N"提示需要知道 TUI 当前状态,很难。改成 hook 阻塞机制,语义清楚,失败时 claude 收到的就是"hook 拒绝"。

### 4.6 缓冲区裁剪点

**决策**:broker 识别 `ESC[?1049h/l`、`ESC[2J`、`ESC[3J` 几个序列,在它们之后裁剪环形缓冲区前缀。

**为什么**:claude 频繁切 alt screen 和清屏,不裁剪则新 viewer attach 时回放出"上辈子"的脏数据。识别成本低(几十行)。

### 4.7 Session 持久化

**决策**:每 session 独立 claude_session_id,broker 重启时 `auto_resume = true` 的 session 自动 `--resume`。

**为什么**:跨重启上下文不丢。claude 自身支持 resume,broker 只需保存 ID 列表。

### 4.8 broker 升级为会话管理器(tmux for claude)

**决策**:broker 内部从"管 1 个 claude"改为"管 N 个 claude",每个客户端有独立的 session 绑定。

**为什么**:实际使用中"出门 IM 下任务"和"在家 Terminal 工作"是并行的,不应被迫共享同一个 claude。同时 Discord 和 QQ 也常需要并行做不同的事。多 session 是底层需要,不是 nice-to-have。

**代价**:broker 复杂度上升,协议所有路径加 session_id。但模式是 tmux 已经验证 20 年的,实现路径清楚。

### 4.9 Hooks 通过 env var 路由 session

**决策**:broker 启动 claude 时设 `AGENT_SESSION_ID` env,hook 读 env 后随 POST 一起发给 broker。

**为什么**:hook 进程从 claude 派生,自然继承 env。比 `transcript_path → session` 反向查表更简单,且 claude 若改了 transcript 路径规则也不会受影响。

### 4.10 一平台一进程 + 共享渲染层(imbot-core)

**决策**:每个 IM 平台一个独立 binary,都连 broker WS。共享 Rust crate `imbot-core` 提供平台无关的意图渲染、降级链、限速、binding 管理。

**为什么**:平台能力差异大(edit/react/button/file 各有有无),要在每个 bot 里独立写降级逻辑会重复且容易跑偏。意图层定义"想表达什么",capability 决定"怎么表达"。新平台 = 新 binary,不动现有代码。

**例外**:QQ 由于生态以 Python OneBot 为主(NoneBot2 等),用 Python 写 QQ bot,通过相同 JSON schema 与 broker 通信,不强行 Rust。

### 4.11 能力驱动渲染(capability-based)

**决策**:不针对每个平台手写渲染,而是定义平台能力描述符 + 通用渲染函数 + 降级链。

**为什么**:见上一条。具体例子:`Decision` 意图,can_react 平台用 ✅❌ 反应,缺 react 平台改成"回复 Y/N",但**业务代码层面只调用一次 `execute(Decision, &platform)`**。

### 4.12 用 LLM router 而非规则 parser 做自然语言命令

**决策**:不写"识别用户说话意图"的规则引擎,直接把意图分类丢给 Haiku/Sonnet 用 tool use 解决。初期用 `Prefix` 模式(`/ai` 显式触发),稳定后切 `Always` 模式(全自动)。

**为什么**:
- 规则引擎覆盖不全(中英混杂、口语化、缩写、多步意图)
- 维护成本指数级:每加一个工具就要补一组规则
- LLM 路由准确率高,会主动 ask_clarification,失败模式可控
- 成本可承受(`Prefix` <$1/月,`Always` ~$24/月 with Haiku)

**代价**:每次路由 +500ms 延迟、需要 Anthropic API key、有月度成本下限。

**渐进策略**:
- Phase 1(初期):`Prefix` 模式 — 用户主动加 `/ai` 才走 router,延迟仅在显式调用产生。**用此模式收集真实使用样本,验证 tool 定义和 system prompt 是否够好。**
- Phase 2(稳定后):配置改 `Always` — 非 `!` 全走 router,获得最自然体验。
- Phase 3(可选):两层路由 — Haiku 先跑,confidence 低时升级 Sonnet。

`Off` 模式作为永久 fallback,API 故障或想离线时切回。

---

## 5. 技术栈

### 主语言:Rust(QQ 适配器可选 Python)

### 项目结构

```
agent/
├── Cargo.toml                 (workspace)
├── .cargo/config.toml          (静态 CRT)
├── crates/
│   ├── shared/                 协议类型(broker ↔ bot ↔ hook 共用)
│   ├── imbot-core/             平台无关的渲染、capability、binding 管理
│   ├── broker/                 主进程(会话管理器)
│   ├── claude-attach/          Terminal viewer + 菜单
│   ├── platform-discord/       discord-bot.exe
│   ├── platform-telegram/      telegram-bot.exe
│   ├── hook-stop/
│   ├── hook-notification/
│   ├── hook-bash-pre/
│   └── hook-bash-post/
├── platform-qq/                Python 子项目(NoneBot2 + OneBot)
│   ├── pyproject.toml
│   └── src/
├── config/
│   ├── default.toml
│   ├── discord.toml
│   ├── telegram.toml
│   └── qq.toml
├── scripts/
│   ├── start-agent.ps1
│   ├── stop-agent.ps1
│   └── install-task.ps1
└── PLAN.md
```

### 主要依赖(Rust 部分)

```toml
# broker
tokio = { version = "1", features = ["full"] }
portable-pty = "0.8"
axum = "0.7"
tokio-tungstenite = "0.21"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
thiserror = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
windows = { version = "0.52", features = ["Win32_System_Console", "Win32_System_Pipes"] }
uuid = { version = "1", features = ["v4"] }
toml = "0.8"

# claude-attach
tokio = { version = "1", features = ["rt", "io-util", "net", "macros"] }
windows = { version = "0.52", features = ["Win32_System_Console"] }
ratatui = "0.26"           # 菜单 TUI
crossterm = "0.27"         # 菜单的键盘事件

# imbot-core
tokio = { version = "1", features = ["full"] }
tokio-tungstenite = "0.21"
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# platform-discord
serenity = { version = "0.12", default-features = false, features = ["client", "gateway", "rustls_backend", "model"] }
# 或 twilight 系列(更轻、更模块化)

# platform-telegram
teloxide = "0.13"
```

### 主要依赖(Python QQ bot)

```toml
nonebot2 = "*"
nonebot-adapter-onebot = "*"
websockets = "*"            # 连 broker
# 配合本机跑 NapCat / LLOneBot 等 OneBot 协议网关
```

### 编译产物

```
target/release/
├── broker.exe                ~6 MB
├── claude-attach.exe         ~500 KB(含 ratatui)
├── platform-discord.exe      ~6 MB
├── platform-telegram.exe     ~5 MB
├── hook-*.exe                ~200 KB × 4
```

### Cargo 优化选项

```toml
[workspace]
members = ["crates/*"]

[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
strip = true
panic = "abort"
```

`.cargo/config.toml`:

```toml
[target.x86_64-pc-windows-msvc]
rustflags = ["-C", "target-feature=+crt-static"]
```

---

## 6. 实施阶段

每阶段产出一个**可手动验收**的功能,不要先把所有架构搭好再调。

### 进度速览

截至 v0.2.1 + 后续两个 feature commit。原 plan 里的核心 phase 1-9 全部
落地,additional 的 LAN / web viewer / release pipeline / **PostToolUse
驱动的工具进度流** / **system tray + Windows toast(本地通知 + 审批)** 等都
做了。剩下的开放项是新方向(跨平台、vendor-agnostic backend、cost dashboard
等),见末尾 §11.

| Phase | 状态 | 备注 |
|---|---|---|
| 1. 最小链路(cmd) | ✅ 完成 | commit `5b5b41f` |
| 2. claude + 环形缓冲区 | ✅ 完成 | `5b5b41f` |
| 3. resize + 帧协议 + ANSI 裁剪 | ✅ 完成 | `5b5b41f` |
| 4. 多 viewer + Ctrl+C 多击 + HTTP 控制面 | ✅ 完成 | `5b5b41f`(detach 键改为 Ctrl+Q / Ctrl+]) |
| 5. hooks → events.jsonl | ✅ 完成 | `83bdba5`(含 install-hooks.ps1 + AGENT_SESSION_ID 哨兵) |
| 6a. Discord IM 适配器 | ✅ 完成 | `10a4791` MVP → `858b133` 大改进:per-channel binding、edit-in-place + typing indicator、reply-thread 路由、attachment forwarding、12 slash commands、reaction commands、@mention wake、DM mode、orphan recovery |
| 6b. Telegram 适配器 | ⏸ 跳过 | 当前需求只用 Discord;imbot-core 抽象未做(目前 platform-discord 直接调 serenity) |
| 6c. QQ 适配器 | ⏸ 跳过 | 同上 |
| 7. IM 输入 → claude | ✅ 完成 | `POST /sessions/:k/input` + 多行 submit fix + Bot 端转发 |
| 7.5. 多 session 重构 | ✅ 完成 | `8e0bc06` |
| 7.6. 菜单 + IM 命令 | ✅ 完成 | claude-attach 菜单 `7298e69`;Discord 12 个 slash + `!`-prefix 等价 commands |
| (额外)config 文件 + viewers 计数 + /state 端点 | ✅ 完成 | `d0c8307` |
| 7.7-A. Hibernate + 持久化 | ✅ 完成 | `7d74b9d` |
| 7.7-B. idle 自动 hibernate + crash 检测 | ✅ 完成 | `c5e331f` |
| 7.8. LLM router | ⏸ 跳过 | 实际未上 —— Discord 直转的体感够用,自然语言 router 没成为痛点 |
| 8. 危险命令 IM 审批 | ✅ 完成 | `858b133` `hook-pretool` + `/tool-request` long-poll + Discord ✅/❌ buttons,fail-open on broker outage |
| 9. 本地在场则闭嘴 | ✅ 完成 | hook 在 `local_viewer_attached=true` 时静默 |
| 10. 运维 | ◐ 部分 | PID 文件 + 按天轮转日志 + events.YYYY-MM-DD.jsonl 7 日 retention 都已完成;开机自启 / claude crash 自动重启**主动跳过** |
| 11. 打磨 | ◐ 部分 | 静态 CRT 链接、unified `agentmux.ps1` 入口、agentmux-cli TOML 助手、release zip + GitHub Actions 自动发布,都已完成;systray / 输入锁 等剩余项可选 |
| **额外:LAN attach + token auth** | ✅ 完成 | `ebfd70f` `claude-attach --broker http://host:port`,`Authorization: Bearer` 鉴权,loopback 豁免,constant-time 比较 |
| **额外:浏览器 web viewer** | ✅ 完成 | `1f67820` (v0.2.1) `http://broker:8765/` 单文件 HTML,xterm.js + addon-fit 通过 `include_bytes!` 嵌入(broker.exe 自包含),WS subprotocol auth 给浏览器,自动重连,移动端软键盘条 |
| **额外:release pipeline** | ✅ 完成 | `scripts/build-release.ps1` + `.github/workflows/release.yml` —— 推 `v*` tag 自动 windows-latest 跑 cargo build + 打 zip + 创 GitHub Release + sha256 校验和 |
| **额外:PostToolUse 进度流** | ✅ 完成 | `4063f40` 新 `hook-posttool` crate POST `tool_progress` 事件;Discord `platform-discord/src/progress.rs` 渲染单工具人话(`✏️ edit src/x.rs` / `🖥 $ cargo test` / `🔎 grep` / `🔌 mcp \`server.tool\`` 等);peek_pending + 800ms 节流 + 8 行 history 上限,`💭 working…` 占位符变成 live timeline,turn 完成时替换成最终答案 |
| **额外:hooks 安装器消重** | ✅ 完成 | `3fd9ec0` `scripts/install-hooks.ps1` 改成 basename 匹配:不论上次装在哪个 zip / 源码目录,重装永远收敛到 1 条指向当前 build 的条目;agentmux.ps1 的 init 步骤撤掉 "skip if installed" 闸,改成无条件调 install-hooks.ps1 当自愈 |
| **额外:system tray + Windows toast** | ✅ 完成 | 新 `agentmux-tray` crate(独立进程,主线程跑 tao 消息泵,worker 跑 tokio):tray-icon 颜色编码 session 状态 + 右键 per-session 子菜单(Attach / Interrupt / Hibernate / Restart / Kill);WinRT 直接 XML toast 走 protocol activation,`assistant_message` / `notification` / `tool_request` 三种 toast,后者带 `[Allow]` `[Deny]` 按钮;`agentmux://` URL scheme 注册到 HKCU,deeplink 走 `interprocess` 命名管道单实例转发;Discord 与 toast 并行接收同一个 `tool_request`,先到先得,broker `/tool-decision/:id` 幂等;**不依赖 IM 即可处理本地审批**。`scripts/start-broker.ps1` 加 mid-shutdown 双 probe 防止 tray "Stop broker" + 立即 `agentmux start` 的 race |

### Phase 1:最小可跑链路(单 session,先跑 cmd) ✅

- [x] Cargo workspace,三个 crate 占位:`broker`、`claude-attach`、`shared`。
- [x] broker:用 `portable-pty` 起 ConPTY,跑 `cmd.exe`。
- [x] broker:开 named pipe 服务,PTY 输出广播给客户端,客户端输入回写 PTY。
- [x] claude-attach:连 pipe,stdin↔pipe↔stdout 透传,raw mode,**先不做菜单**。
- [x] 配 Terminal profile,选这个 profile 进 cmd,关窗口 cmd 仍在 broker 里活着。
- [x] **验收**:再次打开同一 profile,cmd 还活着、prompt 还在。

### Phase 2:换成 claude + 环形缓冲区 + 重放 ✅

- [x] broker 改成跑 `claude --dangerously-skip-permissions`。
- [x] 加环形缓冲区(简单实现:固定字节 deque)。
- [x] viewer attach 时先发缓冲区全部,再切换实时流。
- [x] **验收**:跑 claude 几条对话 → 关 Terminal → 重开 → 立刻看到当前画面。

### Phase 3:resize + 缓冲区裁剪 ✅

- [x] viewer pipe 协议帧格式(tag/len/payload)。
- [x] claude-attach 监听 Win32 WINDOW_BUFFER_SIZE_EVENT,发 RESIZE 帧。
- [x] broker 收到 RESIZE 调 `ResizePseudoConsole`。
- [x] 缓冲区识别 alt screen / 清屏序列,裁剪。
- [x] **验收**:拉伸 Terminal 窗口,claude TUI 自适应;detach 后再 attach,画面无幽灵字符。

### Phase 4:多 viewer + Ctrl+C 多击语义 ✅

- [x] broker 内 fan-out 用 `broadcast`。
- [x] 同时开两个 Terminal,两边输出一致,任一边输入都生效。
- [x] claude-attach 实现 Ctrl+C 1/2/3 次的差异化处理。
- [x] broker 实现 `/sessions/default/interrupt`、`/sessions/default/restart`、`/shutdown`(暂时硬编码 default,Phase 7.5 起改 id-or-name 参数化)。
- [x] **验收**:两个 Terminal 同步显示;一边连按 3 次 Ctrl+C,broker / claude / 两个 attach 全部退出。

> **偏差**:plan 设计的 detach 键 Ctrl+\ (0x1c) 在 Windows 大多数键盘布局上被吃成裸 `\`(0x5c),改用 Ctrl+Q (0x11) 主键 + Ctrl+] (0x1d) 备选。

### Phase 5:hooks 集成(只输出方向,先不接 IM) ✅

- [x] 实现 `hook-stop`、`hook-notification` binary,POST `/event` 到 broker。
- [x] broker `/event` 入口收到事件后写到 `events.jsonl`。
- [x] 配 `~/.claude/settings.json`(脚本 `scripts/install-hooks.ps1` 自动 merge,正斜杠路径绕开 bash 反斜杠转义,幂等 + 旧条目迁移 + `-Uninstall`)。
- [x] **验收**:在 claude 里发一句话,看 `events.jsonl` 出现 `assistant_message` 事件。

> **附加**:`AGENT_SESSION_ID` env 哨兵 —— hooks 在用户其他 claude 进程触发时静默退出,只对 broker 派生的 claude 生效;`hook-stop` 用 file-stability 轮询替代固定 sleep 等待 transcript flush。

### Phase 6a:imbot-core 骨架 + Discord 适配器 ⏸ 跳过

> 用户决定先不接 IM。下面 6a/6b/6c/7 内容保留作未来恢复时的参考。

- [ ] `imbot-core` crate:`Platform` trait、`Caps`、`RenderIntent`、`RenderCtx`、降级渲染函数。
- [ ] `platform-discord` crate:实现 `Platform`,连 Discord,服务在私有频道。
- [ ] platform-discord 连 broker WS,订阅事件,broker 把 `/event` 收到的事件转发。
- [ ] 实现 StreamingUpdate / OneShot / Bulk 三种意图(Decision 留 Phase 8)。
- [ ] 节流(同一 thread_key 1.5s 内 edit)、分块。
- [ ] **验收**:claude 一条对话,Discord 频道收到回复消息。

### Phase 6b:Telegram 适配器(验证 imbot-core 抽象) ⏸ 跳过

- [ ] `platform-telegram` crate,实现 `Platform`。
- [ ] MarkdownV2 转义 helper(放 imbot-core)。
- [ ] **验收**:同时跑 Discord + Telegram,两边都能收到 claude 输出。

### Phase 6c(可选):QQ 适配器 ⏸ 跳过

- [ ] 本机跑 NapCat / LLOneBot 之一,获取 OneBot WS endpoint。
- [ ] `platform-qq` Python 子项目:NoneBot2 + 自建 broker WS 客户端 + 等价 imbot-core 渲染逻辑。
- [ ] **验收**:QQ 私聊收到 claude 输出。

### Phase 7:IM 输入 → claude ⏸ 跳过(依赖 Phase 6)

- [ ] 各 platform-bot 监听 IM 消息(白名单用户),发 WS `{"type":"input","session_id":...}`。
- [ ] broker 写 PTY stdin,自动追加 `\r`。
- [ ] **验收**:Discord 发"今天几号",claude 回复并出现在 Discord。

### Phase 7.5:多 session 改造(纯重构) ✅

- [x] broker 内部从单 session 改成 sessions 表,加 `Session` 结构、`Manager`、绑定表。
- [x] 协议所有路径加 session_id 字段,默认值 default 保证向后兼容(HELLO 帧选 session,无指定回落 "default")。
- [x] hooks 改成读 `AGENT_SESSION_ID` env;broker 启动 claude 时注入 env。
- [ ] ~~platform-bot 内部加 `ChannelBindings`~~ —— 跳过(IM 未做)。
- [x] **验收**:外部行为完全不变(单 session,默认 default),所有原有功能仍工作。

### Phase 7.6:IM 命令体系 + 多 session 用户体验 ◐ 部分

- [ ] ~~实现 `!ls` `!new` `!attach` …~~ —— IM 命令跳过(IM 未做)。
- [x] broker 实现 sessions CRUD HTTP API(`GET/POST /sessions`、`GET/DELETE /sessions/:id`、`POST /sessions/:id/{interrupt,restart,hibernate}`)。
- [x] claude-attach 加菜单 UX,用 `--session` / `--new` 参数(纯文本菜单 stdin/stdout,未引 ratatui)。
- [x] **本地验收**:无参 → 菜单选择;`--new [name]` 创建并 attach;菜单显示 viewers 数量与 hibernated 标识。

### (额外)config 文件 + viewers 计数 + /state 端点 ✅

> 在 7.6 之后、7.7 之前插入,把硬编码值抽出 + 为 Phase 9 做准备。

- [x] `shared::config::Config` —— TOML 加载,字段 http_addr / pipe_name / default_command / ring_cap_bytes / hibernate_idle_secs / sessions_toml_path。AGENT_CONFIG env 覆盖。缺省路径 `%LOCALAPPDATA%\agentmux\config.toml`。
- [x] `Session.attached: HashMap<viewer_id, ClientInfo>` —— HELLO 注册,disconnect 注销;`SessionInfo.viewers` + claude-attach 菜单显示。
- [x] `GET /sessions/:key/state` —— 返回 state、local_viewer_attached、attached_clients、claude_session_id、idle_secs。

### Phase 7.7:Hibernate + 持久化 ✅

#### Part A —— 持久化骨架

- [x] sessions.toml 持久化(id、name、cwd、argv、claude_session_id、auto_resume、created_at_ms),原子 .toml.tmp + rename。
- [x] hooks 透传 transcript_path,broker 从 jsonl 文件名抽 UUID 作为 claude_session_id 写入 session(plus `RingBuffer::clear()` 用于 hibernate 后清屏)。
- [x] attach 时若 Hibernated 自动 resume,broker 用 `claude --resume <id>` 续接。
- [x] broker 启动时按 sessions.toml 恢复 `auto_resume=true` 的(以 Hibernated 状态入册,首次 attach 才 spawn,省内存)。
- [x] `POST /sessions/:key/hibernate` 手动 hibernate;`SessionState` enum(Idle/Hibernated/Crashed)替换 /state 占位字符串。
- [x] **验收**:hibernate 后 attach 可 resume,broker 重启上下文不丢。

#### Part B —— 自动化

- [x] `last_activity` 跟踪(只在用户输入 / viewer attach / resume 时更新,**不**在 PTY 输出时更新 —— claude TUI 心跳否则会一直刷新计时器)。
- [x] `Manager` 加 `idle_scanner` tokio task,60s 一扫,Idle + 无 viewer + age > `hibernate_idle_secs` → 自动 hibernate(0 = 关闭)。
- [x] crash watcher:每 PTY 启动一个 try_wait 轮询,child 在 Idle 状态意外退出 → 标 Crashed;hibernate/restart/shutdown 在 take(child) 之前完成,watcher 看到 None 后自然退出。
- [x] Crashed session 视同 Hibernated,attach 触发 auto-resume。
- [x] **验收**:外杀 claude → 1.5s 内 state=crashed;reattach 自动 resume;闲置阈值后自动 hibernate。

### Phase 7.8:LLM router(Prefix 模式起步) ⏸ 跳过(依赖 Phase 6)

- [ ] `imbot-core/src/router/` 子模块:tools schema、system prompt 模板、Anthropic API 客户端、executor。
- [ ] 配置项 `router_mode = "Off" | "Prefix" | "Always"`,**默认 `Prefix`**。
- [ ] 实现 11 个 tools 对应 broker HTTP API,含 `forward_to_claude` 和 `ask_clarification` 两个出口。
- [ ] 各 platform-bot 在 inbound 处理路径调用 router。
- [ ] 多步 tool_use 顺序执行 + 每步 IM 回报。
- [ ] 失败降级(超时/限额/未知 tool/破坏性 tool 二次确认)。
- [ ] 每用户每日 rate limit(默认 500 次)。
- [ ] router 调用日志写 `events.jsonl`(原文 + 选中的 tool + args)。
- [ ] **验收**:
  - Discord 发 `/ai 开个新 session 叫 blog` → 自动 `create_session` + 切换 + 回报。
  - `/ai 当前 session 在干嘛` → 自动 `get_status` 并展示。
  - `/ai 帮我写个 hello world` → 模型应判定为 prompt,调 `forward_to_claude` 转发到当前 session。
  - 普通消息(无 `/ai` 前缀)走原有 `forward to bound session` 路径,**不调 LLM**。
  - 切配置 `router_mode = "Always"` → 重启 bot → 普通消息也走 router。

### Phase 8:危险命令 IM 审批 ⏸ 跳过(依赖 Phase 6)

- [ ] `hook-bash-pre` 实现命令模式匹配。
- [ ] broker `/approval-request` 阻塞端点。
- [ ] WS 推 `approval_request`,接收 `approval_response`。
- [ ] platform-bot 实现 Decision 意图渲染(✅❌ 反应,缺 react 时降级到文本回复)。
- [ ] **验收**:claude 试图 `rm -rf /tmp/test`,Discord 收到批准请求,点 ✅ 命令执行,点 ❌ claude 看到拒绝原因。

### Phase 9:本地在场则闭嘴(per-session) ✅

- [x] broker `/sessions/{id}/state` 端点返回 `local_viewer_attached: bool`(在"额外"阶段提前完成)。
- [x] hook(`hook-stop` + `hook-notification`)在拉到 broker_url 后调用 `GET /sessions/:id/state`,`local_viewer_attached=true` 直接 `exit 0`,跳过 POST。GET 失败时回落到正常 POST 路径(broker 真挂 POST 也会失败,殊途同归)。
- [x] **本地验收**:Terminal viewer attach default 时 events.jsonl 不再写 default 的 assistant_message / notification;detach 后立即恢复;只有 discord-kind viewer 在场则不算"本地"。IM 推送验证留给 Phase 6。

### Phase 10:运维(部分) ◐

> 用户决定跳过开机自启与 claude crash 自动重启。其余两项做。

- [x] `start-broker.ps1` / `stop-broker.ps1`(原 plan 写的 `start-agent.ps1`,功能等价)。
- [ ] ~~`install-task.ps1` 注册 Task Scheduler~~ —— 跳过(用户不需要开机自启)。
- [x] broker PID 文件 `%LOCALAPPDATA%\agentmux\broker.pid`(`PidGuard` Drop 清理;hard-kill 残留由 `start-broker.ps1` 启动时检测 stale 自动清)。
- [x] tracing 日志按天轮转(`%LOCALAPPDATA%\agentmux\logs\broker.YYYY-MM-DD.log`,`tracing-appender` `Rotation::DAILY` + `max_log_files=7`)。`start-broker.ps1` 不再 `-RedirectStandardOutput`,broker 自管;stderr 仍重定向用于捕获 panic / 早期 eprintln。
- [ ] ~~claude crash 自动重启~~ —— 跳过(用户不需要;crash 检测在 7.7-B 已有,attach 时仍会 auto-resume)。
- [x] **验收**:PID 单例(双开拒绝、stale 自愈、stop-broker 精准定位);日志按天分文件;Phase 7.7-B 的 crash → Crashed 状态保留(只是不再自动重启)。

### Phase 11:打磨(可选)

- [ ] IM bot 命令补全:`!history`、`!save-snapshot`、`!fork`。
- [ ] 输入锁(同一 session 多客户端打字时只让一个写)。
- [x] 静态 CRT 链接产出零依赖 exe(已配 `.cargo/config.toml` `+crt-static`)。
- [x] **systray 图标(状态指示 + 一键收工)** —— 新 `agentmux-tray` crate,见进度速览的"额外"行;独立进程主线程跑 tao 消息泵,worker 跑 tokio,tray-icon 颜色编码 session 状态(idle / running / waiting-approval / disconnected),右键菜单 per-session 子菜单 + Open web viewer + Stop broker + Quit tray;轮询 `/sessions` 5s 刷新 + WS 实时事件路。
- [x] **Windows toast 通知**(原 plan 没单列但天然配对 systray)—— `agentmux-tray` 同进程托管,`assistant_message` / `notification` / `tool_request` 三种 toast,后者带 `[Allow]` `[Deny]` 按钮通过 `agentmux://` URL scheme + 命名管道 IPC 把决定回投 broker,与 Discord 审批并行;**纯本地路径不依赖 IM**。
- [x] **Web viewer**(已在前面"额外"行做了:xterm.js 内嵌 broker.exe,通过 Tailscale / Cloudflare Tunnel 远程访问)。

---

## 7. 配置

### `config/default.toml`(broker 启动时读)

```toml
[broker]
pipe_name = "\\\\.\\pipe\\claude-broker"
http_addr = "127.0.0.1:8765"
ws_addr   = "127.0.0.1:8766"
log_path  = "%LOCALAPPDATA%\\agent\\broker.log"
pid_file  = "%LOCALAPPDATA%\\agent\\broker.pid"
sessions_state = "%LOCALAPPDATA%\\agent\\sessions.toml"
bindings_state = "%LOCALAPPDATA%\\agent\\bindings.toml"

[sessions]
max_sessions = 5
max_concurrent_busy = 3
default_claude_args = ["--dangerously-skip-permissions"]
hibernate_idle_hours = 24
auto_restart_max_per_hour = 5

[ringbuf]
size_bytes = 524288         # 512KB per session
clear_on_alt_screen_exit = true

[approval]
default_timeout_ms = 60000
dangerous_command_patterns = [
    "^rm\\s+-rf",
    "^del\\s+/f",
    ".*format\\s+[a-z]:",
    "^shutdown",
    "git\\s+push\\s+--force",
]
allowed_command_patterns = [
    "^ls\\b", "^cat\\b", "^echo\\b",
]

[shutdown]
ctrl_c_double_window_ms = 1500
```

### `%LOCALAPPDATA%\agent\sessions.toml`(运行时持久化)

```toml
[[sessions]]
id = "uuid-1"
name = "default"
claude_session_id = "abc-123"
cwd = "C:\\projects\\foo"
auto_resume = true

[[sessions]]
id = "uuid-2"
name = "blog"
claude_session_id = "def-456"
cwd = "C:\\projects\\blog"
auto_resume = false
```

### `%LOCALAPPDATA%\agent\bindings.toml`(运行时持久化)

```toml
[[binding]]
platform = "discord"
channel_id = "123456789"
session_name = "default"

[[binding]]
platform = "telegram"
chat_id = "987654321"
session_name = "blog"
```

### `config/discord.toml`

```toml
[broker]
ws_url = "ws://127.0.0.1:8766"
http_url = "http://127.0.0.1:8765"

[discord]
token_env = "DISCORD_BOT_TOKEN"
channel_ids = ["..."]
allowed_user_ids = ["..."]

[render]
strip_ansi = true
debounce_ms = 1500
max_message_chars = 1900
attach_threshold_chars = 4000
forward_tool_use = false
forward_assistant_message = true
forward_notification = true
forward_tool_error = true
```

### `config/telegram.toml`、`config/qq.toml`

结构对称(各自的 token / chat_id / 用户白名单),`render` 段相同。

### `[router]` 段(各 platform 配置共享结构)

```toml
[router]
mode = "Prefix"                              # Off / Prefix / Always,初期 Prefix
trigger_prefix = "/ai "                      # mode=Prefix 时的前缀(注意末尾空格)
model = "claude-haiku-4-5-20251001"          # 主模型
fallback_model = ""                          # 留空 = 不启用两层路由
api_key_env = "ANTHROPIC_API_KEY"
api_base_url = "https://api.anthropic.com"
timeout_ms = 3000
daily_call_limit_per_user = 500
require_confirmation_for_destructive = true  # kill / shutdown / restart 二次确认
log_calls = true                              # 写 events.jsonl
```

切到 `Always` 模式只需把 `mode = "Always"`,重启 platform-bot 即可,broker 无关。

---

## 8. 运维

### 启动

```powershell
# 手动
C:\agent\scripts\start-agent.ps1

# 自动(开机即起)
C:\agent\scripts\install-task.ps1
```

`start-agent.ps1` 启动顺序:

```
1. broker.exe         (常驻,等所有 session 拉起)
2. platform-discord.exe
3. platform-telegram.exe
4. (start) python -m platform_qq    (如启用)
```

### 状态查询

```powershell
curl http://127.0.0.1:8765/state | ConvertFrom-Json
# {
#   "broker_uptime_sec": 3600,
#   "sessions": [
#     {"id":"...","name":"default","state":"Busy","attached":["discord"]},
#     {"id":"...","name":"blog","state":"Hibernated","attached":[]}
#   ]
# }

curl http://127.0.0.1:8765/sessions/default/state | ConvertFrom-Json
# {"state":"Busy","local_viewer_attached":false,"attached_clients":["discord"]}
```

### 控制单个 session

```powershell
# 中断当前任务(等同 Ctrl+C 一次)
curl -X POST http://127.0.0.1:8765/sessions/default/interrupt

# 重启 claude(保留 session id 用 --resume)
curl -X POST http://127.0.0.1:8765/sessions/default/restart

# 销毁 session
curl -X DELETE http://127.0.0.1:8765/sessions/blog?force=true

# 创建新 session
curl -X POST http://127.0.0.1:8765/sessions `
     -H "Content-Type: application/json" `
     -d '{"name":"experiment","cwd":"C:\\test"}'
```

### 关闭整个 broker(连带所有 session)

```powershell
curl -X POST http://127.0.0.1:8765/shutdown
# 或
C:\agent\scripts\stop-agent.ps1
```

### 日志

- `%LOCALAPPDATA%\agent\broker.log`:tracing 输出,按天轮转。
- `%LOCALAPPDATA%\agent\platform-discord.log`、`platform-telegram.log` 等。
- `%LOCALAPPDATA%\agent\events.jsonl`:每条 hook event 一行,审计追溯。

### claude-attach 调试模式

```powershell
claude-attach.exe --debug    # stderr 打印 frame 收发日志,不影响 stdout 透传
```

---

## 9. 安全

`--dangerously-skip-permissions` + 远程 IM 注入 + 多 session 并行 = 高危组合。底线:

| 措施 | 必须? |
|---|---|
| Discord/TG 私有频道,只你一个人 | 必须 |
| 各 platform 配置 `allowed_user_ids` 白名单 | 必须 |
| 危险命令模式匹配 → IM 审批(per-session) | 必须 |
| 危险命令审批粒度:**每 session 各审各的**,不要"批准一次永久放行" | 必须 |
| Bot Token 用环境变量 / Windows Credential Manager,不进 git | 必须 |
| HTTP/WS 服务只 bind 127.0.0.1,不开公网 | 必须 |
| 审计日志(events.jsonl)永不删除 | 必须 |
| 多 session 间不能互访彼此的 cwd / 文件(claude 自己尊重 cwd 即可) | 推荐 |
| LLM router 的 Anthropic API key 用 env var,不进 git / 不进 config 文件 | 必须 |
| router 破坏性 tool(kill / shutdown / restart_claude)执行前需 ✅ 二次确认 | 必须 |
| router 每用户每日 call 数限额(默认 500) | 必须 |
| router 调用日志写 `events.jsonl`(原文 + tool + args),便于审计误判 | 推荐 |
| 切到 `router_mode = "Always"` 之前在 `Prefix` 模式至少跑两周观察误判率 | 推荐 |
| claude 跑在受限 Windows 用户下 | 推荐 |
| 长期运行放在 Windows Sandbox 或专门 VM | 锦上添花 |

**永远不要把 broker 的 HTTP/WS 暴露到公网**。IM 平台是唯一对外接口,IM 平台帮你扛认证 / DDoS / 限速。

---

## 10. 测试策略

| 测试项 | 怎么测 |
|---|---|
| 字节透传无丢失(单 session) | 通过 named pipe 发 1MB 随机字节,broker 回写到 fake PTY,读出来一致 |
| 多 viewer fan-out(同 session) | N 个 attach 同连同一 session,字节流一致 |
| 多 session 隔离 | 同时 2 个 session,分别注入不同字节,各自 attach 客户端只收到自己 session 的字节 |
| Hook session 路由 | 模拟两个 claude 进程不同 env,hook 触发后 broker 路由到正确 session |
| 重放正确性 | claude 跑一段输出 → 关 attach → 重新 attach → 屏幕和之前一样 |
| Ctrl+C 多击语义 | 自动化按键,验证 1/2/3 次分别触发 interrupt / restart / shutdown |
| 缓冲区裁剪 | 注入 `\x1b[2J` 后,缓冲区前面字节被丢弃 |
| Hook 在场不推、不在场推(per-session) | mock /state,各 session 独立验证 |
| 危险命令审批超时 | 模拟 IM 不响应,60s 后 hook 退出码 2 |
| Session resume 跨重启 | 关 broker → 重启 → claude 继续上一会话 |
| Hibernate / awaken | session idle 24h 后自动 hibernate,attach 时正确 resume |
| 平台 capability 降级 | mock 一个 caps 全 false 的平台,Decision 意图正确降级到文本 parse |
| IM 命令体系 | 跨平台(Discord + TG)同一 session,命令行为一致 |

---

## 11. 待定 / 未来工作

可能要做但现在不做:

- ~~**Web viewer**:基于 xterm.js 的浏览器 attach,通过 Tailscale 或 Cloudflare Tunnel 安全访问。需要 broker 把 named pipe 协议同时映射到 WebSocket。~~ **已完成(v0.2.1)。**
- ~~**本地通知**:`Notification` 事件除了 IM,也用 Windows toast。~~ **已完成(`agentmux-tray` crate)** —— 此外还做了 `tool_request` 的本地按钮审批,完全脱离 IM 也能批 / 拒。
- **输入锁**:同一 session 多客户端同时打字时,某段时间内只允许一个写,其他 read-only。`!lock` / `!unlock` 切换。
- **Session 分支**:基于 claude 的 `--resume` 在某个 turn 后分叉一个新 session,实验不同方向。
- **批量批准**:同 session 同 hour 内同类危险命令一次批准多次。
- **Slack / 飞书 / Matrix 适配器**:imbot-core 已经预留扩展点,新加只需要新 crate。
- **WeChat**:暂不支持(没有合规个人 bot API)。
- **自定义 ITerminalConnection**:在 Microsoft Terminal 里走原生 connection 而非 stdio relay,获得更好的 attach UX(标签页菜单等)。低优先级。
- **claude 异常监控**:卡死(长时间无输出)、内存暴涨等指标,自动重启或报警。
- **跨设备 broker**:这台机器跑 broker,另一台机器的 Terminal 通过 SSH/WireGuard attach。
- **router 切换到 Always 模式**:`Prefix` 跑稳后(误判率 < ~5%、月成本可接受),`router_mode` 改 `Always`,所有非 `!` 消息全过 LLM。
- **router 两层路由**:Haiku 先跑,confidence 低或参数不全时升级 Sonnet 重跑,准确率接近 Sonnet 单跑而成本只略增。
- **router 也用于 Terminal**:新增 `agent-cli "<自然语言>"` 命令,复用 `imbot-core::router`,Terminal 用户也享受自然语言而不必记菜单/参数。

---

## 附录 A:依赖与规范速查

### Claude Code hooks 入参速查

每个 hook 收到的 stdin JSON 关键字段:

```jsonc
// PreToolUse / PostToolUse
{
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": { "command": "...", "description": "..." },
  "tool_response": { "stdout": "...", "stderr": "...", "exit_code": 0 },  // PostToolUse 才有
  "session_id": "...",         // claude 自己的 session id
  "transcript_path": "...",
  "cwd": "..."
}

// Stop
{ "hook_event_name": "Stop", "session_id": "...", "transcript_path": "...", "stop_hook_active": false }

// Notification
{ "hook_event_name": "Notification", "message": "...", "session_id": "...", "transcript_path": "..." }
```

退出码:`0` 放行;`2` 阻塞(stderr 反馈给 claude);其它 = 错误。

我们额外读两个 env var:

```
AGENT_SESSION_ID    # broker 启动 claude 时注入的 session id(我们自己的)
AGENT_BROKER_URL    # http://127.0.0.1:8765
```

### IM 平台能力对照(实现适配器时参考)

| 能力 | Discord | Telegram | QQ(OneBot) | Slack | 飞书 |
|---|---|---|---|---|---|
| 编辑消息 | ✅ | ✅ | 部分(撤回重发) | ✅ | ✅ |
| 表情反应 | ✅ | ✅(有限) | ✅(有限) | ✅ | 部分 |
| 回复/线程 | ✅ | ✅ reply | ✅ at-reply | ✅ thread | ✅ |
| 上传文件 | ✅ | ✅ | ✅ | ✅ | ✅ |
| 按钮 / inline keyboard | ✅ | ✅ | ❌ | ✅ block | ✅ card |
| Markdown 风格 | Discord-flavor | MDv2 严格 | CQ 码 | mrkdwn | 飞书富文本 |
| 单消息字数 | 2000 | 4096 | ~3000 | ~40000 | 30000 |
| 编辑速率限制 | 5/5s 单条 | 1/s 每聊天 | 看实现 | 较宽 | 较宽 |

---

## 附录 B:实施顺序时间预估

按"晚上 + 周末"业余节奏:

| 阶段 | 累计工时估计 |
|---|---|
| Phase 1 最小链路(cmd) | 4-6h |
| Phase 2 claude + 重放 | 2-3h |
| Phase 3 resize + 裁剪 | 3-4h |
| Phase 4 多 viewer + Ctrl+C 多击 | 3-4h |
| Phase 5 hooks → 日志 | 2h |
| Phase 6a imbot-core + Discord | 8-10h |
| Phase 6b Telegram 适配器 | 3-4h |
| Phase 6c QQ 适配器(可选) | 6-8h |
| Phase 7 IM 输入 → claude | 2h |
| Phase 7.5 多 session 重构 | 5-7h |
| Phase 7.6 IM 命令 + attach 菜单 | 5-7h |
| Phase 7.7 Hibernate + 持久化 | 3-4h |
| Phase 7.8 LLM router(Prefix 模式) | 4-6h |
| Phase 8 审批流 | 4-5h |
| Phase 9 在场闭嘴(per-session) | 1h |
| Phase 10 持久化 + 运维脚本 | 3-4h |
| Phase 11 打磨(可选) | 不计 |

**关键里程碑**:

- **MVP 单 session 可用**(Phase 1-5):约 14-19h,本地 Terminal 已可用,claude 不再随关窗口死。
- **MVP 含 IM**(到 Phase 7):约 27-35h,Discord 单 IM 通,本地 + 远程都能用。
- **完整多 session 多 IM**(到 Phase 7.7 + 8/9):约 50-65h,设计目标全实现。
- **加 LLM router(Prefix)**(到 Phase 7.8):约 54-71h,IM 用户体验进入"自然语言"层。
- **再加打磨**(Phase 10-11):全套约 64-81h。

前 5 阶段(~15h)就能拿到"在家 Terminal 用 + 关掉 claude 不死"的可用版本,Phase 6-9 加上"出门 IM 用",Phase 7.5-7.7 加上"多 session 并行",Phase 10 加上"开机自动 + 跨重启"。

---

## 附录 C:常用术语

| 术语 | 含义 |
|---|---|
| broker | 常驻守护进程,会话管理器 |
| session | 一个 claude 进程 + ConPTY + 元数据,等同 tmux session |
| claude_session_id | claude CLI 自己的 session id(给 `--resume` 用),不同于我们的 session id |
| viewer | 任何连到 broker 的 PTY 客户端(Terminal 或 platform-bot) |
| platform-bot | 一个 IM 平台的独立 binary(discord-bot.exe 等) |
| imbot-core | 平台无关的渲染共享 crate |
| Caps | 平台能力描述符 |
| RenderIntent | 语义意图(StreamingUpdate / OneShot / Bulk / Decision) |
| ThreadKey | 业务键,跨平台稳定(如 turn id),用于 edit 同一条消息 |
| binding | "某个 IM 频道当前绑哪个 session" |
| hibernate | session 元数据保留,claude 进程关闭,attach 时 resume |
| attach / detach | 客户端连/断 session,session 本身不受影响 |
