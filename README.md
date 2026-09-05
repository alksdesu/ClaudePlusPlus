# Claude++

[![简体中文](https://img.shields.io/badge/简体中文-d97757?style=for-the-badge)](#中文)
[![English](https://img.shields.io/badge/English-1a1918?style=for-the-badge)](#english)

让同一台机器上的 Claude Desktop 与 Claude Code CLI 共享全部会话 —— 跨渠道、跨账号、双向迁移，还能把 Codex 的对话带进 Claude。

Share every Claude session across Claude Desktop and Claude Code CLI on the same machine — across gateway channels, across accounts, in both directions, plus one-way import of Codex conversations into Claude.

![Tauri](https://img.shields.io/badge/Tauri-2-blue) ![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS-informational) ![License](https://img.shields.io/badge/license-AGPL--3.0-green)

---

## 中文

<sub>[Switch to English ↓](#english)</sub>

### 背景

Claude Desktop 把 Code 会话元数据按 `账号/组织` 目录组合隔离存放。切换 gateway 渠道或登录账号会生成不同的组合目录，旧会话随即从列表"消失"；CLI 会话则因为没有 Desktop 元数据而完全不出现在 Desktop 列表里。实际上：

- **会话转录（jsonl）从未隔离** —— CLI 与 Desktop Code 会话共用 `~/.claude/projects/<项目>/<uuid>.jsonl`
- **隔离的只是元数据目录** —— `<userData>/claude-code-sessions/<accountId>/<orgId>/`

Claude++ 针对这两层分别下手。

### 功能

| 功能 | 做法 |
|---|---|
| **存储归一** | 把所有 `账号/组织` 组合合并进会话数最多的主池，原目录改名 `.bak-<时间戳>` 后建目录链接指回主池（Windows 用 NTFS junction，macOS 用 symlink）。任何渠道、任何账号、官方版或第三方 userData，枚举到的都是同一池。不修改应用本体，Desktop 升级不失效，随时可还原 |
| **会话迁移** | CLI→Desktop：为 jsonl 生成一份 Desktop 元数据（转录零拷贝，两边同源）。Desktop→CLI：按 Desktop 自己的墓碑约定移除元数据，转录保留，`claude -r` 照常续聊。两方向互为逆操作 |
| **分支归组** | rewind/resume 会把历史整段复制进新 jsonl，compact 续接则从写入母文件的 `compact_boundary` 行开始复制——按这两条血缘把同一逻辑会话的所有分支文件折叠为一组，默认只显示、只注册最新分支；同组任一分支已注册即拦截重复注册（UI/批次/存储三层） |
| **Codex 迁移** | 扫描 `~/.codex` 的 rollout 会话并与 Claude 侧对账：已迁移 / Claude 导入镜像 / 孤儿镜像 / Codex 独有。独有会话一键转为 Claude 转录（幂等：threadId 即 sessionId），并回写 Codex 导入台账防止它把产物再同步回去 |
| **会话预览** | 单击任意会话行弹出独立预览窗口：Markdown 渲染（DOMPurify 消毒）、工具调用聚合为摘要行、大会话只加载末尾 300 条。全程只读 |
| **彻底删除** | 两列各有删除入口：CLI 侧删转录并连带清掉它在 Desktop 的条目与墓碑，Desktop 侧删条目并带走对应转录。与注销的分工——注销只把会话退回 CLI 侧（`claude -r` 照常），删除是两边一起消失。勾组代表则整组分支文件一并删除，转录一律送系统回收站 |
| **Fast Mode 解锁** | 让用 Console API key（3p 模式）登录的 Desktop 也能用 fast mode：wrapper 顶替 bundled CLI 把 `fastMode` 合并进启动参数，renderer patch 亮出实时开关并按模型显隐。Desktop / CLI 更新后托盘守护自动重新部署与 patch（renderer 部分弹一次 UAC）。仅 Windows；前提是 key 已开通 fast 资格——只解客户端封锁，不碰服务端计费与授权 |
| **墓碑清理** | 一键清理 `deleted_*` 删除标记：仅清标记（解封会话、恢复可注册），或连 CLI 侧转录一并删除。删除一律送系统回收站，可反悔 |
| **托盘守护** | 常驻监控两池，新渠道/新账号首次出现自动归一；Desktop 运行中则挂起，退出后自动执行 |

### 存储结构

```
<userData>/                            # Windows 3p: %LOCALAPPDATA%\Claude-3p（官方: %APPDATA%\Claude）
│                                      # macOS:      ~/Library/Application Support/Claude-3p（官方: .../Claude）
├── claude-code-sessions/<acc>/<org>/  # Code 会话元数据池（隔离实体）
│   ├── local_<uuid>.json              #   活跃会话元数据，cliSessionId 指向转录
│   └── deleted_<cliSessionId>         #   墓碑：内容为删除时刻的毫秒时间戳
├── local-agent-mode-sessions/...      # Cowork 沙箱会话（内嵌独立 .claude，不参与迁移）
└── ant-did                            # base64(设备 UUID)，gateway 模式的 accountId 来源

~/.claude/projects/<cwd编码>/<uuid>.jsonl   # 会话转录，CLI 与 Desktop 共用
~/.codex/sessions/<年>/<月>/<日>/rollout-*.jsonl   # Codex 会话（Codex 迁移页的数据源）
%APPDATA%\com.claudeplusplus.manager\fastmode.json   # Fast Mode 守护设置与已 patch 的版本记录
```

### 使用

1. 从 Release 下载或自行构建：Windows 运行 `claude-plus-plus.exe`，macOS 打开 `Claude++.app`（未签名，首次需右键 → 打开）。常驻托盘/菜单栏
2. **完全退出 Claude Desktop**（所有写操作都有运行检测，运行中一律拒绝）
3. 「存储归一」页 → 一键归一；此后切渠道/换账号列表不再变化
4. 「会话迁移」页 → 勾选 CLI 会话 → 注册，打开 Desktop 对应项目即可见、可继续。多分支会话折叠显示，`⑂ N` 可展开挑选特定分支。两列头的「删除」可彻底移除选中会话（二次确认，转录进回收站）
5. 「Codex 会话迁移」页 → 筛选「Claude 无」→ 一键迁移（需先退出 Codex），完成后回「会话迁移」页注册即可在 Desktop 打开
6. 任意页面单击会话行即可预览完整对话内容
7. 「Fast Mode」页 → 安装（需先退出 Desktop，renderer 部分弹一次 UAC）→ 保持自动守护开启，Desktop 或 CLI 更新后会自动补上；页面底部按转录里的 `usage.speed` 验证是否真 fast

### Fast Mode 解锁（Windows）

> 前提：Console API key 本身已开通 fast mode（服务端资格）。这里只解**客户端**的封锁，不绕服务端计费与授权。

Desktop 用 API key 登录时进入 `deploymentMode:"3p"`，官方把 fast 能力位硬编码为 `blocked_by_platform`：UI 不出开关，bundled CLI 落进 `sdk_opt_in_required`。但客户端唯一的钥匙是 `flagSettings.fastMode`，它只从 CLI 的 `--settings` 参数读取；而 UI 开关背后的 `setFastMode` IPC 在 host 侧没有门控。于是：

| 组件 | 做法 | 落点 |
|---|---|---|
| wrapper | 顶替版本目录的 `claude.exe`，把 `fastMode:true` 合并进 `--settings` 后转发给改名保留的 `claude-real.exe`；Desktop 只校验 `.verified` 不比 exe 本体 | `%LOCALAPPDATA%\Claude-3p\claude-code\<ver>\` |
| renderer patch | 改写 minified renderer 的三处锚点：按 IPC 可用 + 模型支持显示开关、按模型 ID 判定支持、清掉禁用原因。改前备份 `.orig`，始终从备份出发 patch，幂等 | MSIX 包内 `resources\ion-dist\assets\v1\` |

wrapper 管"会话默认 fast"，开关管"实时切换"。写 WindowsApps 需要管理员：Claude++ 以固定参数重新启动自身完成 renderer 改动（弹一次 UAC），提权进程不接受任何路径参数。

| 模型 | fast |
|---|---|
| Opus 5 / Opus 4.8 | 真 fast |
| Opus 4.6 | 服务端已不再提供，开关不显示 |
| 其他 | 不支持，开关隐藏 |

验证以转录里的 `usage.speed` 为准（页面底部的验证卡统计的就是它），Desktop 状态栏的 Fast 标签有官方显示 bug。

Desktop 更新会同时换掉 CLI 版本目录和 renderer。守护线程监控 CLI 目录并定期核对 MSIX 版本，失效即自动补上；Desktop 运行中则等它退出后执行。若 Desktop 大版本改动让三处锚点匹配不上，页面会列出未命中项：此时 wrapper 仍生效（Opus 会话默认 fast，只是没有开关），需按同样语义重新适配 `src-tauri/src/fastmode.rs` 里的 `ANCHORS`。「还原官方」一键撤销全部改动。

### 构建

```bash
npm install
npx tauri build   # Windows 产物: bundle/nsis 安装包 + claude-plus-plus.exe；macOS 产物: bundle/macos/Claude++.app + bundle/dmg
```

依赖：Node.js 18+、Rust stable、Tauri 2。

```bash
cd src-tauri && cargo test    # 单元测试（junction / 迁移字段 / 墓碑流转 / 幂等）
```

### 安全设计

- 写操作前检测对应 userData 的 Desktop 进程，运行中拒绝执行；Codex 迁移同理检测 Codex 进程
- 归一前原目录完整保留为 `.bak-<时间戳>`，一键还原
- 会话转录 jsonl 在归一与迁移全程只读；Codex 迁移只新增 Claude 转录、不改 Codex 会话本体；墓碑清理的删除走系统回收站
- 元数据写入采用临时文件 + 原子改名
- 预览窗口全程只读，渲染前经 DOMPurify 消毒
- Fast Mode：renderer 改动前备份 `.orig`，还原即删；官方 CLI 改名保留在原目录，不下载不替换二进制内容；提权子进程不接受任何路径参数，自行定位 MSIX 包

### 免责

非官方工具。依赖 Claude Desktop 内部存储结构，后续版本结构变化可能导致功能失效——失效模式是"不生效"，不会破坏数据。

---

## English

<sub>[切换到中文 ↑](#中文)</sub>

### Background

Claude Desktop stores Code session metadata under per-`account/org` directory combos. Switching gateway channels or accounts lands you in a different combo, and previous sessions vanish from the list; CLI sessions never appear in Desktop at all because they lack Desktop metadata. In reality:

- **Session transcripts (jsonl) were never isolated** — CLI and Desktop Code sessions share `~/.claude/projects/<project>/<uuid>.jsonl`
- **Only the metadata directories are isolated** — `<userData>/claude-code-sessions/<accountId>/<orgId>/`

Claude++ addresses both layers.

### Features

| Feature | How it works |
|---|---|
| **Pool unification** | Merges every `account/org` combo into the largest pool, renames originals to `.bak-<timestamp>`, and drops directory links pointing back (NTFS junctions on Windows, symlinks on macOS). Every channel, account, official or third-party userData enumerates the same pool. No app binaries touched — survives Desktop updates, fully reversible |
| **Session migration** | CLI→Desktop: generates Desktop metadata pointing at the existing jsonl (zero-copy, both sides share one transcript). Desktop→CLI: removes metadata following Desktop's own tombstone convention; the transcript stays and `claude -r` keeps working. The two directions are exact inverses |
| **Branch grouping** | rewind/resume copies the full history prefix into a new jsonl, and compact continuations start at a `compact_boundary` line also written into the mother file — both lineages fold every branch of one logical session into a single group. Only the latest branch is shown and registered by default; registering a branch whose sibling is already registered is refused at UI, batch and store level |
| **Codex import** | Scans `~/.codex` rollouts and classifies every thread against Claude: migrated / mirror imported from Claude / orphan mirror / Codex-only. Codex-only threads convert to Claude transcripts in one click (idempotent: threadId doubles as sessionId), and the conversion is recorded in Codex's import ledger so it never syncs the output back as a duplicate |
| **Session preview** | Click any session row to open a read-only preview window: markdown rendering (DOMPurify-sanitized), tool calls aggregated into summary lines, only the last 300 messages of huge sessions loaded |
| **Hard delete** | Both columns get a delete action: from the CLI side it removes the transcript plus that session's Desktop entry and tombstone; from the Desktop side it removes the entry along with the transcript it points at. Unlike unregister — which only sends a session back to the CLI side where `claude -r` still works — delete makes it vanish on both. Selecting a group representative deletes every branch file in that group; transcripts always go to the OS recycle bin |
| **Fast Mode unlock** | Lets a Desktop signed in with a Console API key (3p mode) use fast mode: a wrapper replaces the bundled CLI and merges `fastMode` into its launch settings, and a renderer patch surfaces the live toggle gated by model. When Desktop or the CLI updates, the tray daemon redeploys and re-patches on its own (the renderer step prompts UAC once). Windows only; the key must already be entitled to fast — this removes the client-side block, never server-side billing or entitlement |
| **Tombstone cleanup** | One-click cleanup of `deleted_*` markers: markers only (un-blocks re-registration), or markers plus CLI transcripts. All deletions go through the OS recycle bin |
| **Tray daemon** | Watches both pools; when a new channel/account combo first appears it auto-unifies — deferred while Desktop is running, executed once it exits |

### Storage layout

```
<userData>/                            # Windows 3p: %LOCALAPPDATA%\Claude-3p (official: %APPDATA%\Claude)
│                                      # macOS:      ~/Library/Application Support/Claude-3p (official: .../Claude)
├── claude-code-sessions/<acc>/<org>/  # Code session metadata pool (the isolation boundary)
│   ├── local_<uuid>.json              #   active session metadata; cliSessionId points at the transcript
│   └── deleted_<cliSessionId>         #   tombstone: deletion epoch millis as file content
├── local-agent-mode-sessions/...      # Cowork sandbox sessions (embedded .claude; not migratable)
└── ant-did                            # base64(device UUID), accountId source in gateway mode

~/.claude/projects/<encoded-cwd>/<uuid>.jsonl   # transcripts, shared by CLI and Desktop
~/.codex/sessions/<y>/<m>/<d>/rollout-*.jsonl   # Codex threads (source of the Codex tab)
%APPDATA%\com.claudeplusplus.manager\fastmode.json   # Fast Mode daemon settings and patched-version ledger
```

### Usage

1. Download from Releases or build locally: run `claude-plus-plus.exe` on Windows, open `Claude++.app` on macOS (unsigned — right-click → Open on first launch). Lives in the tray / menu bar
2. **Quit Claude Desktop entirely** (every write operation checks for running instances and refuses otherwise)
3. Unify tab → one click; channel/account switches no longer change the session list
4. Migrate tab → select CLI sessions → Register; open the matching project folder in Desktop to see and resume them. Multi-branch sessions fold into one row — `⑂ N` expands them to pick a specific branch. Either column's Delete button wipes the selected sessions for good (confirmation required; transcripts go to the recycle bin)
5. Codex tab → filter "not in Claude" → migrate in one click (quit Codex first), then register the results on the Migrate tab to open them in Desktop
6. Click any session row on any tab to preview the full conversation
7. Fast Mode tab → Install (quit Desktop first; the renderer step prompts UAC once) → leave auto-guard on and updates to Desktop or the CLI get patched again on their own; the bottom card verifies real fast via `usage.speed` in the transcripts

### Fast Mode unlock (Windows)

> Prerequisite: the Console API key itself is entitled to fast mode server-side. This removes the **client-side** block only; billing and entitlement stay with the server.

Signing in with an API key puts Desktop in `deploymentMode:"3p"`, where the fast-mode capability is hard-coded to `blocked_by_platform`: no toggle in the UI, and the bundled CLI lands in `sdk_opt_in_required`. Yet the only client-side key is `flagSettings.fastMode`, read solely from the CLI's `--settings` flag, and the `setFastMode` IPC behind the toggle has no gate on the host side. Hence:

| Component | How | Location |
|---|---|---|
| wrapper | Replaces `claude.exe` in the version directory, merges `fastMode:true` into `--settings` and forwards to the renamed `claude-real.exe`; Desktop only checks `.verified`, never the exe itself | `%LOCALAPPDATA%\Claude-3p\claude-code\<ver>\` |
| renderer patch | Rewrites three anchors in the minified renderer: show the toggle when IPC is available and the model supports fast, decide support by model id, drop the disabled reason. Backs up `.orig` first and always patches from that backup, so it is idempotent | `resources\ion-dist\assets\v1\` inside the MSIX package |

The wrapper makes sessions fast by default; the toggle switches at runtime. Writing under WindowsApps needs administrator rights: Claude++ relaunches itself with a fixed flag to apply the renderer change (one UAC prompt), and the elevated helper accepts no path arguments.

| Model | fast |
|---|---|
| Opus 5 / Opus 4.8 | real fast |
| Opus 4.6 | no longer served fast by the backend; toggle hidden |
| others | unsupported; toggle hidden |

Trust `usage.speed` in the transcript (the verification card at the bottom of the tab counts exactly that); the Fast label in Desktop's status bar has a known display bug.

A Desktop update replaces both the CLI version directory and the renderer. The daemon watches the CLI directory and periodically checks the MSIX version, repairing as soon as something is missing, or waiting for Desktop to exit first. If a major Desktop release changes the renderer so the three anchors no longer match, the tab lists the misses: the wrapper still works (Opus sessions default to fast, just without the toggle) and `ANCHORS` in `src-tauri/src/fastmode.rs` needs re-adapting with the same semantics. "Restore official" undoes everything in one click.

### Build

```bash
npm install
npx tauri build   # Windows: bundle/nsis installer + claude-plus-plus.exe; macOS: bundle/macos/Claude++.app + bundle/dmg
```

Requires Node.js 18+, Rust stable, Tauri 2.

```bash
cd src-tauri && cargo test    # unit tests (junction / migration fields / tombstone flow / idempotency)
```

### Safety

- Every write operation checks for a running Desktop bound to the target userData and refuses while it lives; Codex import likewise refuses while Codex runs
- Unification keeps originals intact as `.bak-<timestamp>`; one-click restore
- Transcripts are strictly read-only during unify and migration; Codex import only adds Claude transcripts and never touches Codex threads; tombstone deletions go to the recycle bin
- Metadata writes use temp-file + atomic rename
- Preview windows are read-only; rendered content is DOMPurify-sanitized
- Fast Mode backs the renderer up as `.orig` before patching and removes it on restore; the official CLI is renamed in place, never downloaded or altered; the elevated helper takes no path arguments and locates the MSIX package itself

### Disclaimer

Unofficial tool. Relies on Claude Desktop's internal storage layout; future versions may change it and break features — the failure mode is "no effect", never data loss.

## License

AGPL-3.0-only
