# SSH 隧道使用指南

`ssh-tunnel-start.ps1` 和 `ssh-tunnel-stop.ps1` 用一台带公网 SSH 的中继机
**C**,把没有公网的 broker 主机 **A** 上的 agentmux 暴露给同样没有公网的
viewer 主机 **B**。agentmux 本身保持默认 `127.0.0.1:8765` 绑定不动,隧道
让 B 的 loopback 等同于 A 的 loopback。

## 1. 适用场景

- A、B 都没有公网 IP(家用宽带 / 移动网络 / 公司 NAT)
- C 有公网 IP,提供 SSH 登录(密码或密钥都行)
- A 跑 broker(以及可选的 Discord bot),B 想用浏览器或 `claude-attach`
  接入 A 上的 claude session

```
    A (broker)                                    B (viewer)
       │                                              │
       │  ssh -R 18765:127.0.0.1:8765                 │  ssh -L 8765:127.0.0.1:18765
       └────────────────────►  C  ◄───────────────────┘
                            (公网中继)
```

A、B 都跑同一个脚本,用 `-Side broker` / `-Side viewer` 区分。

## 2. 前置条件

| 主机 | 必需 |
|---|---|
| A | Windows 10/11、agentmux broker 在跑、能 SSH 到 C |
| B | Windows 10/11、能 SSH 到 C |
| C | sshd 接受登录、`AllowTcpForwarding yes`(默认值) |

C 上 **不需要**:管理员权限、改 `sshd_config`、开放额外端口、`GatewayPorts`。
默认 `GatewayPorts no` 让 18765 只在 C 的 loopback 可达,正是我们想要的最小
攻击面。

A、B 可以用 **同一个 SSH 账号** 登 C,也可以各用各的,只要两边都能登进去就行。

### 关于 OpenSSH 与 plink

- **密钥登录** 用 Windows 10+ 自带的 `ssh.exe`(OpenSSH 客户端)。一般已
  默认安装,没有的话:`Settings → Apps → Optional Features → Add a feature → OpenSSH Client`。
- **密码登录** 必须用 PuTTY 套件里的 `plink.exe`,因为 Windows 自带的 OpenSSH
  故意拒绝从非 tty 读密码,绕不过去。安装:

  ```powershell
  winget install --id PuTTY.PuTTY
  # 或
  choco install putty
  ```

  装完重启 PowerShell,让 PATH 重新加载。

## 3. 完整步骤

### 3.1 A 主机(broker 端)

确保 broker 在跑、保持默认配置(`http_addr = "127.0.0.1:8765"`,无需 token)。

**密钥登录(推荐)**:

```powershell
.\scripts\ssh-tunnel-start.ps1 -Side broker `
    -RemoteHost relay.example.com `
    -RemoteUser bob `
    -Auth key `
    -KeyFile $env:USERPROFILE\.ssh\id_ed25519
```

**密码登录**:

```powershell
.\scripts\ssh-tunnel-start.ps1 -Side broker `
    -RemoteHost relay.example.com `
    -RemoteUser bob `
    -Auth password
# 提示输入密码(隐藏字符)
```

成功后会看到:

```
Tunnel up. PID 12345 recorded in C:\Users\you\AppData\Local\agentmux\ssh-tunnel-broker.pid
Broker on this host (127.0.0.1:8765) is now reachable as 127.0.0.1:18765 on relay.example.com.
```

### 3.2 B 主机(viewer 端)

```powershell
# 密钥
.\scripts\ssh-tunnel-start.ps1 -Side viewer `
    -RemoteHost relay.example.com `
    -RemoteUser alice `
    -Auth key `
    -KeyFile $env:USERPROFILE\.ssh\id_ed25519

# 或密码
.\scripts\ssh-tunnel-start.ps1 -Side viewer `
    -RemoteHost relay.example.com -RemoteUser alice -Auth password
```

成功后:

```
Tunnel up. PID 67890 recorded in ...\ssh-tunnel-viewer.pid
http://127.0.0.1:8765/ on this host now reaches the broker on the other side.
  Browser: start http://127.0.0.1:8765/
  Attach:  .\claude-attach.exe --broker http://127.0.0.1:8765 --session default
```

### 3.3 验证隧道是否连通

ssh / plink 进程"在跑"不等于隧道"通了"。下面分 A、B 两端各自检查,出问题
可以一眼定位是哪一段断的。

#### 3.3.1 A 端(broker 主机)

```powershell
# 1. 隧道进程还活着
Get-Content $env:LOCALAPPDATA\agentmux\ssh-tunnel-broker.pid |
    ForEach-Object { Get-Process -Id $_ -ErrorAction SilentlyContinue }
# 期望:有一行 ssh 或 plink 的输出
```

```powershell
# 2. 反向端口在 C 上真的绑住了 —— 从 A 临时 SSH 进 C 跑一下
ssh bob@relay.example.com "ss -tlnp | grep 18765"
# 期望:
#   LISTEN 0 128 127.0.0.1:18765 0.0.0.0:* users:(("sshd",pid=...))
#
# 老系统(没有 ss)用 netstat:
ssh bob@relay.example.com "netstat -an | grep 18765"
# 期望看到 127.0.0.1:18765 处于 LISTEN
```

**第 2 步是最关键的一步**——它直接证明 A → C 这一段的反向转发真的生效
了,而不是 ssh 进程"看上去活着但其实转发请求被 sshd 拒了"(比如 sshd 配
了 `AllowTcpForwarding no`,A 上的 ssh 不会立刻退出,但 C 上根本没绑端
口,这种状态从 A 本地看不出来)。

#### 3.3.2 B 端(viewer 主机)

```powershell
# 1. 隧道进程还活着
Get-Content $env:LOCALAPPDATA\agentmux\ssh-tunnel-viewer.pid |
    ForEach-Object { Get-Process -Id $_ -ErrorAction SilentlyContinue }

# 2. 本地 8765 在监听
Test-NetConnection 127.0.0.1 -Port 8765
# 期望:TcpTestSucceeded : True

# 3. 端到端打通 —— 请求穿过 B → C → A → broker 再原路返回
curl http://127.0.0.1:8765/sessions
# 期望:返回 JSON,即使没 session 也是 [] 而不是连接错误
```

第 3 步通过了,就说明全链路能用,直接进 §3.4。

#### 3.3.3 哪一步失败说明什么

| 失败点 | 真正断在哪 | 怎么修 |
|---|---|---|
| B 端第 2 步失败(`TcpTestSucceeded : False`) | B 的 ssh / plink 已退出,本地 8765 没人绑 | 看 ssh-tunnel-start.ps1 输出有没有报错,再跑一遍 |
| B 端第 2 步过、第 3 步 connection refused / timeout | B → C 通,但 C 上 18765 没人接 → A 端隧道没起好或已断 | 回 A 端做 §3.3.1 第 2 步,看不到 LISTEN 就重启 A 端隧道 |
| B 端第 3 步连上但返回非 JSON 错误 | 隧道通了,但 broker 没在跑 | A 端跑 `.\agentmux status` 看 broker 健康状态 |
| A 端 §3.3.1 第 2 步看到 `0.0.0.0:18765` 而非 `127.0.0.1:18765` | C 上 sshd 开了 `GatewayPorts yes`,18765 暴露到公网 | 见 §6 安全须知,broker **必须** 配 attach_token |
| A 端 §3.3.1 第 2 步看不到任何 18765 行,但 ssh 进程还活着 | sshd 拒了端口转发(`AllowTcpForwarding no`),或 18765 已被别的进程占了 | 让 C 的管理员开 `AllowTcpForwarding`,或换一个 `-BridgePort` |

### 3.4 在 B 端访问 broker

隧道验证通过后,B 端的 `127.0.0.1:8765` 就**等价于** A 端的 broker。下面三种
访问方式和你坐在 A 主机本机用时**一字不差**——broker 看到的请求源 IP 全
是 127.0.0.1,触发 loopback 豁免,所以 **不需要任何 token / 环境变量**。

#### 3.4.1 浏览器(web viewer)

```powershell
start http://127.0.0.1:8765/
```

打开后:

- 顶部菜单列出 broker 上所有 session,点一下进入对应 xterm 终端
- 看到的就是 A 上 claude TUI 的实时画面;ring buffer 会回放最近 ~512 KB,
  所以一进来就是"接着上次"的状态,不是空屏
- **同一个 session 可以多个浏览器 / 设备同时打开**,输入合并,resize 取
  所有 viewer 中最小的列宽行高
- 移动端浏览器底部有软键栏(Esc / Tab / 方向键 / `^C` `^D` `^L` `^Z`),
  补虚拟键盘没有的键
- 网络抖动后 WebSocket 会自动重连退避,不丢 scrollback
- **关闭浏览器只是 detach**,A 上的 claude 继续跑,session 不受影响

> 要不要 token?默认配置(broker 绑 127.0.0.1)+ 本套隧道方案 = **不需要**。
> 浏览器进页面也不会弹 token 输入框。如果将来你切到 LAN 直连模式,再按
> README "Remote viewer over LAN" 配 `attach_token`。

#### 3.4.2 终端 viewer(claude-attach.exe)

最常用三种姿势:

```powershell
# 1. 弹菜单选 session(没有 --session 参数时)
.\claude-attach.exe --broker http://127.0.0.1:8765

# 2. 直接进某个已有 session
.\claude-attach.exe --broker http://127.0.0.1:8765 --session default

# 3. 新建 session 并进入
.\claude-attach.exe --broker http://127.0.0.1:8765 --new my-task
.\claude-attach.exe --broker http://127.0.0.1:8765 --new my-task --cwd D:\projects\foo
```

进入 TUI 后的快捷键(`README` "Highlights" 一节有完整说明,这里只挑常用):

| 快捷键 | 行为 |
|---|---|
| `Ctrl+Q` 或 `Ctrl+]` | **detach**,关闭这个 viewer,session 继续跑 |
| `Ctrl+C` x1(1.5 秒内) | 转发 `0x03`,中断当前 turn |
| `Ctrl+C` x2 | 重启 session 的 claude 子进程 |
| `Ctrl+C` x3 | 关闭整个 broker(慎用) |

多个 `claude-attach` 同时连同一个 session 也 OK:输入按到达顺序合并,
画面共享。

#### 3.4.3 HTTP API(可选,自动化用)

所有 README 里列的接口都能直接 `curl` 调用,因为隧道把 B 的 loopback 接到
broker 上:

```powershell
# 列 session
curl http://127.0.0.1:8765/sessions

# 给 default session 注入一句话
curl -X POST http://127.0.0.1:8765/sessions/default/input `
     -H "Content-Type: application/json" `
     -d '{"text":"hello from B","append_enter":true}'

# 中断
curl -X POST http://127.0.0.1:8765/sessions/default/interrupt
```

写脚本批量驱动 broker 时这种姿势最省事——不用拉起 viewer 也能控会话。

### 3.5 停止

```powershell
# A 主机
.\scripts\ssh-tunnel-stop.ps1 -Side broker

# B 主机
.\scripts\ssh-tunnel-stop.ps1 -Side viewer

# 同一台机器测试时跑过两边都开过
.\scripts\ssh-tunnel-stop.ps1 -Side all
```

## 4. 参数详解

### `ssh-tunnel-start.ps1`

| 参数 | 必需 | 默认 | 说明 |
|---|---|---|---|
| `-Side` | ✓ | — | `broker`(A 端,反向 `-R`)或 `viewer`(B 端,正向 `-L`) |
| `-RemoteHost` | ✓ | — | 中继 C 的 DNS 名或 IP |
| `-RemoteUser` | ✓ | — | C 上的 SSH 账号 |
| `-Auth` | ✓ | — | `key` 或 `password` |
| `-KeyFile` | 仅 key | — | 私钥路径,如 `~\.ssh\id_ed25519`。`-Auth key` 时必填 |
| `-RemoteSshPort` |  | `22` | C 的 sshd 端口 |
| `-BridgePort` |  | `18765` | C 上的中转端口。**A 和 B 必须一致** |
| `-BrokerPort` |  | `8765` | A 端:broker 监听的端口。viewer 端忽略 |
| `-LocalPort` |  | `8765` | B 端:本机暴露的端口。broker 端忽略 |

### `ssh-tunnel-stop.ps1`

| 参数 | 必需 | 默认 | 说明 |
|---|---|---|---|
| `-Side` | ✓ | — | `broker` / `viewer` / `all`(后者停掉两端 PID 文件对应的进程) |

## 5. 故障排查

### 启动后立刻退出

脚本会区分两类,分别给具体提示:

| 情况 | 检查 |
|---|---|
| 密钥模式 ssh 立刻退出 | 私钥路径对不对、C 上有没有放对应公钥、`-RemoteUser` 拼写、防火墙拦了 22 端口 |
| 密码模式 plink 立刻退出 | 密码错误、host key 还没缓存(见下)、sshd 是否拒绝端口转发(`AllowTcpForwarding no`) |

### plink 首次连新主机

`-batch` 模式禁用所有交互,如果 host key 没缓存到注册表,plink 会直接失败。**手动跑一次让它缓存**:

```powershell
plink -ssh bob@relay.example.com
# 出现 "Store key in cache? (y/n)" 时按 y
# 然后 Ctrl+C 退出,再回来跑 ssh-tunnel-start.ps1
```

之后这台机器再连这个 host 就不需要重复了。

> 提示:用密钥模式可以彻底跳过这一步——脚本带的 `StrictHostKeyChecking=accept-new` 让 OpenSSH 首次自动接受。

### "An broker tunnel is already running"

之前的隧道还活着,先停了再起:

```powershell
.\scripts\ssh-tunnel-stop.ps1 -Side broker
```

### 隧道一段时间后自己断了

家用 / 移动网络的 NAT 会把空闲 TCP 干掉。脚本已经设了
`ServerAliveInterval=30`,绝大多数情况下够用。还是断的话:

- 检查 C 的 sshd 配置 `ClientAliveInterval`(可以从服务端方向也打 keepalive)
- 用 `while ($true) { ssh-tunnel-start.ps1 ...; Start-Sleep 10 }` 包一层重连
- 或塞进 Windows 任务计划程序,触发器选 "Restart on failure"

### "PID xxxx is <name>, not ssh/plink"

stop 脚本看到 PID 文件里写的进程已经被 OS 回收成别的进程了,出于安全
拒绝杀。手动确认无误后:

```powershell
Remove-Item $env:LOCALAPPDATA\agentmux\ssh-tunnel-broker.pid
```

### B 端浏览器打开页面但 WebSocket 连不上

WebSocket 通过 SSH 隧道是透明的,这种问题几乎都是隧道本身没起来:

```powershell
# B 端确认本地 8765 在监听
Test-NetConnection 127.0.0.1 -Port 8765

# A 端确认 broker 在跑
.\agentmux status
```

### 想从 LAN 直连 broker(B 在同一局域网),不想走 C

那就别用这套隧道,直接看 README.md "Remote viewer over LAN":在 broker
配 `http_addr = "0.0.0.0:8765"` + `attach_token`。SSH 隧道方案是为
"双方都没公网,只有第三方 SSH 中继" 这个场景准备的。

## 6. 安全须知

### 默认拓扑(密码 / 密钥都一样)

- C 上的 18765 **只在 loopback 可达**(`GatewayPorts no` 默认),公网摸不到
- 从 broker 视角所有请求源 IP 都是 127.0.0.1,触发 agentmux 的 loopback 豁免,**无需 `attach_token`**
- 攻击面 = C 的 SSH 账号本身

**结论**:谁能登 C 谁就能控 broker。给 C 的 SSH 账号配强密码或干脆禁密码登录 + fail2ban。

### 密码模式的进程命令行泄露

`-Auth password` 时,plink 通过 `-pw <password>` 传密码,这个**短暂**出现
在 plink 的进程命令行里。本机其他用户(或恶意进程)在隧道存活期间
理论上可以读到。

```powershell
# 不放心的话,这条命令能看到 plink 命令行里有什么
Get-WmiObject Win32_Process -Filter "Name='plink.exe'" | Select-Object CommandLine
```

单用户机器 + 自己装的软件,一般无所谓。在意的话:**用密钥模式**,从根本上消除这个泄露面。

### 不要在 C 上开 GatewayPorts

`GatewayPorts yes` 会把 18765 绑到 0.0.0.0,瞬间变成公网端口。如果真的
有这种需求(比如 B 不想装 SSH 客户端,只想 `http://C:18765/`),broker
**必须** 配 `attach_token` 并且 C 上 `ufw / firewalld` 限定来源 IP。

## 7. 备选方案(脚本之外)

这套脚本解决"只有 SSH 中继"的场景。其他更省事的拓扑:

- **Tailscale / WireGuard**:A、B 加同一个 tailnet,B 直接 `http://A的tailscale-ip:8765`,无需 C,延迟也低
- **Cloudflare Tunnel**:A 跑 `cloudflared`,把 8765 暴露成 https 域名,B 用任何浏览器访问

但这些都改了拓扑;现有 SSH 中继 5 分钟能用上,先用着,不够稳再换。
