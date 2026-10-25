# Claude++

[![简体中文](https://img.shields.io/badge/简体中文-d97757?style=for-the-badge)](#中文)
[![English](https://img.shields.io/badge/English-1a1918?style=for-the-badge)](#english)

让同一台机器上的 Claude Desktop 与 Claude Code CLI 共享会话，导入 Codex 对话，并为 API key 模式提供 Fast Mode 与原生 Computer Use 支持。

Share sessions across Claude Desktop and Claude Code CLI on the same machine, import Codex conversations, and enable Fast Mode and native Computer Use for API-key deployments.

![Tauri](https://img.shields.io/badge/built%20with-Tauri-blue) ![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS-informational) ![License](https://img.shields.io/badge/license-AGPL--3.0-green)

---

## 原生增强与工作台

开放 Desktop 里已有但对 API key 模式关着的原生功能。所有入口都是官方的，Claude++ 只负责把开关打开。

| 功能 | Windows | macOS | 说明 |
|---|:-:|:-:|---|
| **Computer Use** | ✓ | ✓ | 看屏幕、动鼠标键盘；macOS 另有后台模式 |
| **简体与繁体中文** | ✓ | ✓ | 装官方词典并打开语言选择器，未覆盖的键保留英文 |
| **Fast Mode 原生开关** | ✓ | ✓ | 模型菜单里的 Fast 开关，状态跟随 CLI |
| **CLI 原生预热**（实验） | ✓ | ✓ | 备用进程池，多占常驻内存 |
| **iOS / Android 模拟器** | — | ✓ | Windows 构建里没有这部分代码 |

功能可以任意组合，每次都从官方基线重新生成，关掉一项不影响其他选中的。应用被外部更新或改过、备份损坏、有未完成事务时一律拒绝覆盖，可从「备份与恢复」还原。

两端的补丁由同一套结构化规则生成：以属性名和字符串字面量为骨架，minify 后的变量名靠正则捕获再回填，命中必须唯一且改写后要通过回读校验。Desktop 更新只重命名符号时规则自动适应；结构真变了会停下报出未命中的锚点，而不是猜。

姓名、照片头像、Artifacts、扩展安装、听写和跨构建主题缺少可用的原生链路，卡片上写明缺什么，不提供装不上的开关。显示“已安装”只代表文件改好了，设备权限仍要你在系统里单独授权。

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
| **Codex 迁移** | 扫描 `~/.codex` 的 rollout 会话并与 Claude 侧对账：已迁移 / Claude 导入镜像 / 孤儿镜像 / Codex 独有。新版分页存储沿 `history_base` 把多页拼回一个对话（每页分叉点之后是编辑重发或回退前的废弃分支，不计入），`.jsonl.zst` 压缩页透明解压；名称取自 Codex 状态库里的对话名，子代理线程不单独列出。独有会话一键转为 Claude 转录（幂等：threadId 即 sessionId），并回写 Codex 导入台账防止它把产物再同步回去 |
| **会话预览** | 单击任意会话行弹出独立预览窗口：Markdown 渲染（DOMPurify 消毒）、工具调用聚合为摘要行、大会话只加载末尾 300 条。全程只读 |
| **彻底删除** | 两列各有删除入口：CLI 侧删转录并连带清掉它在 Desktop 的条目与墓碑，Desktop 侧删条目并带走对应转录。与注销的分工——注销只把会话退回 CLI 侧（`claude -r` 照常），删除是两边一起消失。勾组代表则整组分支文件一并删除，转录一律送系统回收站 |
| **Fast Mode 解锁** | 让用 Console API key（3p 模式）登录的 Desktop 也能用 fast mode：wrapper 顶替 bundled CLI 把 `fastMode` 合并进启动参数。Windows 另有 renderer patch 亮出实时开关并按模型显隐（弹一次 UAC）；macOS 的 wrapper 请求默认开启；原生开关可通过「原生增强 → Fast Mode 原生开关」的整包重签流程安装。Desktop / CLI 更新后托盘守护自动补上。前提是 key 已开通 fast 资格——只解客户端封锁，不碰服务端计费与授权 |
| **墓碑清理** | 一键清理 `deleted_*` 删除标记：仅清标记（解封会话、恢复可注册），或连 CLI 侧转录一并删除。删除一律送系统回收站，可反悔 |
| **原生功能** | 在「功能增强」页勾选安装，「备份与恢复」页还原。Computer Use、中文、Fast 开关和预热两端都可用，模拟器仅 macOS。补丁由结构化规则现场生成，命中不唯一或校验不过就拒绝修改 |
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
~/.codex/sessions/<年>/<月>/<日>/rollout-*.jsonl[.zst]   # Codex 会话；分页格式一个对话多页：rollout-<时间>-<对话>_<页>
~/.codex/state_<N>.sqlite                   # Codex 状态库，只读取 threads.name 作对话名称
<配置目录>/com.claudeplusplus.manager/fastmode.json   # Fast Mode 守护设置与已生效的版本组合
                                            # Windows: %APPDATA%；macOS: ~/Library/Application Support
<配置目录>/com.claudeplusplus.manager/computer-use/windows/   # Windows Computer Use 安装记录与原始文件备份
<配置目录>/com.claudeplusplus.manager/computer-use/           # macOS Computer Use 安装记录、原包备份与日志
```

### 使用

1. 从 Release 下载或自行构建：Windows 运行 `claude-plus-plus.exe`，macOS 打开 `Claude++.app`（未签名，首次需右键 → 打开）。常驻托盘/菜单栏
2. **完全退出 Claude Desktop**（所有写操作都有运行检测，运行中一律拒绝）
3. 「存储归一」页 → 一键归一；此后切渠道/换账号列表不再变化
4. 「Claude 会话」页 → 勾选 CLI 会话 → 注册，打开 Desktop 对应项目即可见、可继续。多分支会话折叠显示，`⑂ N` 可展开挑选特定分支。两列头的「删除」可彻底移除选中会话（二次确认，转录进回收站）
5. 「Codex 导入」页 → 筛选「Claude 无」→ 一键迁移（需先退出 Codex），完成后回「Claude 会话」页注册即可在 Desktop 打开
6. 「Claude 会话」或「Codex 导入」页单击会话行即可预览对话；大会话保留末尾 300 条并提示截断
7. 「Fast Mode」页 → 安装（需先退出 Desktop；Windows 的 renderer 部分弹一次 UAC，macOS 全程用户态）→ 保持自动守护开启，Desktop 或 CLI 更新后会自动补上；页面底部按转录里的 `usage.speed` 验证是否真 fast
8. 「功能增强」页 → 勾选 Computer Use → 应用更改 → 确认应用（Windows 确认 UAC；macOS 先选择有效签名身份）。完成后打开 Desktop，在设置中自行启用 Computer use 并完成原生授权；需要撤销时退出 Desktop，在「备份与恢复」页点「还原官方应用」

### Fast Mode 解锁

> 前提：Console API key 本身已开通 fast mode（服务端资格）。这里只解**客户端**的封锁，不绕服务端计费与授权。

Desktop 用 API key 登录时进入 `deploymentMode:"3p"`，官方把 fast 能力位硬编码为 `blocked_by_platform`：UI 不出开关，bundled CLI 落进 `sdk_opt_in_required`。但客户端唯一的钥匙是 `flagSettings.fastMode`，它只从 CLI 的 `--settings` 参数读取；而 UI 开关背后的 `setFastMode` IPC 在 host 侧没有门控。于是：

| 组件 | 做法 | 落点 |
|---|---|---|
| wrapper（两平台） | 顶替 Desktop 实际启动的那份 CLI，把 `fastMode:true` 合并进 `--settings` 后转发给挪开保留的官方本体（Windows 改名，macOS 连整个 bundle 一起挪）；Desktop 只校验 `.verified` 不比二进制本体 | Win `%LOCALAPPDATA%\Claude-3p\claude-code\<ver>\<build>\`<br>mac `~/Library/Application Support/Claude-3p/claude-code/<ver>/<build>/claude.app/Contents/MacOS/`<br>`<build>` 是清单 sha256 的前 12 位；同时识别 CLI 直接位于 `<ver>` 下的目录布局 |
| renderer patch（仅 Windows） | 改写 minified renderer 的相关锚点：按 IPC 可用 + 模型支持显示开关、按模型 ID 判定支持、清掉禁用原因，并为新版模型菜单补齐 `fast_mode` 配置。锚点以属性名与字符串字面量为骨架、变量名正则捕获后回填，Desktop 更新重命名 minify 符号不会失配。改前备份 `.orig`，始终从备份出发 patch，幂等 | MSIX 包内 `resources\ion-dist\assets\v1\` |

wrapper 管"未指定时的默认 fast"，原生开关管"实时切换与选择保留"。

Fast 开关可位于模型菜单底部，选择支持的模型后可看到「Enable fast mode」；独立按钮布局也受支持。需要修复时，退出 Desktop，在 Claude++ 的「Fast Mode」页点击「修复」，再重新打开 Desktop。

Windows 写 WindowsApps 需要管理员：Claude++ 以固定参数重新启动自身完成 renderer 改动（弹一次 UAC），提权进程不接受任何路径参数。

macOS wrapper 在用户目录部署，无需提权。它处理 Desktop 的默认禁用环境变量，并将官方静态 API key 映射给原生资格预检；已有 API-key 配置、托管策略与服务端授权保持原样。

macOS 的签名资源不能直接改写：ASAR 条目哈希、`Info.plist` 中的 ASAR 文件头哈希和整包签名必须同步更新。有效开发签名可以启动修改后的应用。Fast Mode 的原生开关可在「原生增强」中单独选择，复用同一套备份、校验、重签与还原流程；CLI wrapper 仍单独管理。不会关闭 Electron fuses、hardened runtime 或库校验。

CLI 那半边不走 Gatekeeper 的应用启动路径（Desktop 用 `posix_spawn` 直接执行），但内核的签名校验绕不过：官方 CLI 同样开着 hardened runtime，它的签名配对 `Contents/Info.plist`，一旦被拎出 bundle 或改个名，就不再算那个 bundle 的主程序，签名当场失配、exec 即 SIGKILL（Desktop 会把这个信号一律翻译成「被端点安全拦截」，跟真的 EDR 无关）。所以藏官方本体的办法是连**整个 bundle 一起挪**到 `claude-real.app`——签名只认 `Contents/MacOS/<主程序>` 这层相对结构，bundle 目录叫什么它不在意。腾出来的 `claude.app` 只留一个壳：`MacOS/claude` 软链到 Claude++ 自身，`MacOS/claude-real` 软链回本体。

Desktop 每次启动会读 CLI 的**前 8 字节**验 Mach-O 魔数（`0xFEEDFACF` 加 cputype，或 `0xCAFEBABE` 的 universal），不合格就判缓存失效、重下整个 bundle。所以顶替物不能是脚本，得是**软链到 Claude++ 自身**——它本身就是合格的 Mach-O，检查顺着软链读到它。Claude++ 发现自己以 `claude` 之名被调起就走 wrapper 分支：把 `fastMode` 并进 `--settings` 后 `exec` 官方本体，同一 PID，不需要 Windows 那套 Job Object。软链断掉（Claude++ 被移走或删掉）时 Desktop 会自动重下官方 CLI，失效模式是"回到原样"。

Windows 与 macOS 均按模型能力提供官方 Fast 控件。macOS wrapper 仅在启动参数没有指定 Fast 状态时补上默认值，显式关闭不会被覆盖。模型、组织、额度与冷却限制保持原生处理，实际速度以服务端返回结果为准。

验证以转录里的 `usage.speed` 为准。macOS 安装原生开关后，模型菜单里的选择和 Fast 标记跟随 CLI 上报，点击通过官方 `LocalSessions.setFastMode` 接口生效。冷却时保留原生的 Fast 选择语义，不代表该次请求一定以 Fast 处理。

Desktop 更新会换掉 CLI 版本目录（Windows 还会换 renderer）。守护线程监控 CLI 目录并定期核对 Desktop 版本，失效即自动补上；Desktop 运行中则等它退出后执行。wrapper 与 renderer 各修各的，一边失败不耽误另一边；失败的版本组合不再自动重试，升级 Claude++ 后会重新试一次。CLI 迁入构建子目录后，守护会把外层保留的官方本体接回 wrapper 身边；构建目录里已有新下载的官方本体时，则清掉外层残留。更新只重命名 minify 符号时锚点自动适应，无需改代码。只有 renderer 的代码结构真变了，页面才会列出未命中的锚点：此时 wrapper 仍生效（支持 fast 的会话仍默认开启，只是没有开关），需按同样语义重新适配 `src-tauri/src/fastmode.rs` 里的 `ANCHORS`；想先验证某份 renderer 能不能 patch，跑 `cd src-tauri && cargo run --example fastmode_patch_probe -- "<renderer>.js"`，它会逐锚点列出改写前后的命中数；再给一个输出路径（如 `out.mjs`）就会把改写结果写出来，用 `node --check out.mjs` 确认拼出的 JS 仍能解析。「还原官方」一键撤销全部改动。

Fast Mode 支持单文件和主逻辑、模型菜单分布在多个 renderer 文件中的布局。Windows 修复会先校验全部目标，再分别保存 `.orig` 并写入；写入失败时回滚本次改动。状态卡只有在所有目标均已打补丁时才显示成功。排查拆分布局时，将探测参数换成 `assets/v1` 目录，并将第二个参数设为独立输出目录；逐个对导出的 `.mjs` 执行 `node --check`。

### 原生功能（Windows）

面向 Windows x64 的 MSIX 包。完全退出 Desktop 后，在「功能增强」页勾选要装的功能，点「应用更改」并确认 UAC。装完重启 Desktop，在设置里打开对应开关。

按勾选的功能决定动哪些文件：主进程开关改 `resources/app.asar`，并同步 exe 的 `INTEGRITY / ELECTRONASAR` 资源；中文和 Fast 开关改 `resources/ion-dist` 下的 renderer，中文另外写入官方词典。全部先在副本上改完校验再替换，带操作锁和恢复记录，中途失败可还原。ASAR 校验 fuse 保持开启；改过的 exe 不再带官方 Authenticode 签名，还原即恢复。

记录与备份存于 `<配置目录>/com.claudeplusplus.manager/computer-use/windows/`。发现 Desktop 被外部更新或改过就拒绝覆盖。MSIX 更新会整包替换，更新后需要重新扫描并再装一次。

开发验收在 `src-tauri` 目录执行 `cargo run --example computer_use_probe -- status`，只读探测当前安装。副本验收用 `status|install|restore|apply COPY/app STORE [身份] [组合 JSON]`，其中 `COPY/app` 是独立应用副本，`STORE` 是副本外的记录目录；Windows 忽略身份参数，探针拒绝直接修改系统安装目录。

### 原生 Computer Use（macOS）

「功能增强」页提供检测、签名身份选择与安装，「备份与恢复」页负责还原，能否安装以页面检测结果为准。在主进程 JS 文件中按配置函数内容定位唯一入口，不依赖带哈希的 chunk 文件名；再核对入口哈希与补丁输出哈希。未知版本、重复命中或内容变化均拒绝安装，不把相似代码当作已兼容。

安装为 API key 模式补上原生 Computer Use 功能配置，并在兼容的应用中提供后台模式入口，不修改 CLI 二进制，不另建 MCP 服务。安装后仍需在 Claude 设置中自行启用并授权。Background 在允许的应用中后台操作，需要接管屏幕时先询问；Full control 默认接管屏幕、鼠标和键盘。HIPAA、会话授权、禁止控制的应用等检查保持原样。

旧版托管补丁显示「可升级」：先验证备份并还原官方包，再安装新版；中途失败可还原后重试。

- 需要本机钥匙串中有效的代码签名证书及私钥；已验证 Apple Development 签名，**不支持 ad-hoc**，不内置或导出证书私钥。
- 先退出目标 Desktop。Claude++ 保存完整官方备份，在临时副本更新 ASAR 条目与文件头哈希，逐个保留非身份类 entitlements 并重签嵌套组件，验证整包签名后才替换。
- 重签会改变应用身份，可能需要重新登录或重新授予系统权限；不保证原钥匙串访问或所有 Cowork 场景保持兼容。会话、API key、用户配置和系统授权数据库不作修改。
- 安装记录、原包备份和命令日志保存在 `<配置目录>/com.claudeplusplus.manager/computer-use/`。失败可用「还原官方应用」结束未完成事务；还原会比对整包哈希，不覆盖被外部更新或修改的应用，备份保留。
- 默认不在后台重签，也不关闭自动更新；只有开启「自动维护 Desktop 更新」后，才会在 Desktop 退出后按原选择恢复补丁。适配新版本前不会自动修改，也不会用旧备份覆盖新版本。

开发验收在 `src-tauri` 目录运行 `cargo run --example computer_use_probe -- status COPY/Claude.app STORE`；`install` 另接签名身份 SHA-1，`restore` 恢复备份。探针拒绝系统应用路径，使用独立副本与独立记录目录。运行时不依赖 Python、Node.js 或开发工具链。

### macOS 原生增强适配

主进程、后台模式、中文、模拟器、预热和 Fast 原生状态按同一份已验证适配记录检查。记录包含原始内容、功能组合和输出哈希；chunk 改名但内容未变时按内容发现，未知内容仍停止修改。旧版适配与独立备份保留。

原生权限、管理员策略和 Fast 的模型／服务端资格检查不变。更新维护只恢复原先选择且已验证支持的功能，不会自动打开新增选项。

### macOS 自动更新维护

仅 macOS 的重签增强需要此流程；Windows 不需要，继续使用官方更新。可在“功能增强”中开启“自动维护 Desktop 更新”，Claude++ 会保存当前功能组合与签名身份，并在托盘中维护更新状态。

检测到官方更新后，Desktop 运行中只排队，不强关会话。退出后验证备份、还原完整官方包，并在后台启动官方 Desktop，让官方更新器继续下载与安装；官方重启提示和正常退出流程保持原样。更新期间不会抢占官方安装器或覆盖新版本。

更新后的应用在退出后接受官方签名和补丁兼容性校验，只恢复当前 Claude++ 已验证支持的功能。未知版本保持官方原样，原功能选择保留；后续 Claude++ 适配后会重新检测。网络离线、签名身份失效或文件校验失败会显示具体状态，不假报成功，也不关闭更新签名校验。

流程记录持久化，重启 Claude++ 后继续处理未完成阶段。手动还原增强会关闭自动维护，避免自动装回；也可随时关闭维护或重新检测。此功能不修改 API 配置、会话内容、系统授权或管理员更新策略。

### 构建

```bash
npm install
npx tauri build   # Windows 产物: bundle/nsis 安装包 + claude-plus-plus.exe；macOS 产物: bundle/macos/Claude++.app + bundle/dmg
```

依赖：Node.js、Rust stable、Tauri。具体依赖以仓库清单与锁文件为准。

```bash
cd src-tauri && cargo test    # 单元测试（junction / 迁移字段 / 墓碑流转 / 幂等）
```

### 安全设计

- 写操作前检测对应 userData 的 Desktop 进程，运行中拒绝执行；Codex 迁移同理检测 Codex 进程
- 归一前原目录完整保留为 `.bak-<时间戳>`，一键还原
- 会话转录 jsonl 在归一与迁移全程只读；Codex 迁移只新增 Claude 转录、不改 Codex 会话本体，Codex 状态库以只读方式打开；墓碑清理的删除走系统回收站
- 元数据写入采用临时文件 + 原子改名
- 预览窗口全程只读，渲染前经 DOMPurify 消毒
- Computer Use 安装前校验版本、内容与备份，先处理副本再替换；操作锁防止并发写入，恢复记录用于处理中断。还原只接受匹配安装记录的应用；原生授权与禁止控制的应用检查保持不变
- Fast Mode：官方 CLI 原样保留在 Desktop 启动它的那层目录（Windows 改名，macOS 连 bundle 一起挪开），不下载不替换二进制内容；Windows 的 renderer 改动前备份 `.orig`、还原即删，提权子进程不接受任何路径参数、自行定位 MSIX 包。备份与还原要在包目录里增删 `.orig`，而 WindowsApps 下只有 TrustedInstaller 与 SYSTEM 可写，因此会对 `assets\v1` 这一层目录取得所有权（不递归、不影响 Desktop 更新，更新后目录重建即恢复默认）；macOS wrapper 只动用户目录下的 CLI；可选的原生 Fast 开关通过已验证的整包签名事务更新 Desktop，无需提权

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
| **Codex import** | Scans `~/.codex` rollouts and classifies every thread against Claude: migrated / mirror imported from Claude / orphan mirror / Codex-only. Paginated rollouts are stitched back into one thread along `history_base` (whatever follows a page's fork point is a branch abandoned by an edit or rewind and is left out), `.jsonl.zst` pages are decompressed transparently, names come from Codex's state database, and sub-agent threads are not listed on their own. Codex-only threads convert to Claude transcripts in one click (idempotent: threadId doubles as sessionId), and the conversion is recorded in Codex's import ledger so it never syncs the output back as a duplicate |
| **Session preview** | Click any session row to open a read-only preview window: markdown rendering (DOMPurify-sanitized), tool calls aggregated into summary lines, only the last 300 messages of huge sessions loaded |
| **Hard delete** | Both columns get a delete action: from the CLI side it removes the transcript plus that session's Desktop entry and tombstone; from the Desktop side it removes the entry along with the transcript it points at. Unlike unregister — which only sends a session back to the CLI side where `claude -r` still works — delete makes it vanish on both. Selecting a group representative deletes every branch file in that group; transcripts always go to the OS recycle bin |
| **Fast Mode unlock** | Lets a Desktop signed in with a Console API key (3p mode) use fast mode: a wrapper replaces the bundled CLI and merges `fastMode` into its launch settings. Windows additionally patches the renderer to surface the live toggle gated by model (one UAC prompt); on macOS the native toggle is installed through the signed Native Features transaction, while the wrapper supplies a default only when the launch settings do not explicitly choose on or off. When Desktop or the CLI updates, the tray daemon repairs on its own. The key must already be entitled to fast — this removes the client-side block, never server-side billing or entitlement |
| **Tombstone cleanup** | One-click cleanup of `deleted_*` markers: markers only (un-blocks re-registration), or markers plus CLI transcripts. All deletions go through the OS recycle bin |
| **Native features** | Pick features on the Native Features tab, restore from Backup & Restore. Computer Use, Chinese, the Fast toggle and prewarming work on both platforms; simulators are macOS-only. Patches come from structural rules applied to the current build — ambiguous matches or failed verification are refused |
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
~/.codex/sessions/<y>/<m>/<d>/rollout-*.jsonl[.zst]   # Codex threads; paginated threads span pages: rollout-<time>-<thread>_<page>
~/.codex/state_<N>.sqlite                   # Codex state database, read only for threads.name
<config dir>/com.claudeplusplus.manager/fastmode.json   # Fast Mode daemon settings and active-version ledger
                                            # Windows: %APPDATA%; macOS: ~/Library/Application Support
<config dir>/com.claudeplusplus.manager/computer-use/windows/   # Windows Computer Use records and original-file backups
<config dir>/com.claudeplusplus.manager/computer-use/           # macOS Computer Use records, original bundles, and logs
```

### Usage

1. Download from Releases or build locally: run `claude-plus-plus.exe` on Windows, open `Claude++.app` on macOS (unsigned — right-click → Open on first launch). Lives in the tray / menu bar
2. **Quit Claude Desktop entirely** (every write operation checks for running instances and refuses otherwise)
3. Unify tab → one click; channel/account switches no longer change the session list
4. Claude Sessions tab → select CLI sessions → Register; open the matching project folder in Desktop to see and resume them. Multi-branch sessions fold into one row — `⑂ N` expands them to pick a specific branch. Either column's Delete button wipes the selected sessions for good (confirmation required; transcripts go to the recycle bin)
5. Codex Import tab → filter "not in Claude" → migrate in one click (quit Codex first), then register the results on the Claude Sessions tab to open them in Desktop
6. Click a session row on the Claude Sessions or Codex Import tab to preview the conversation; large sessions show the last 300 messages with a truncation notice
7. Fast Mode tab → Install (quit Desktop first; on Windows the renderer step prompts UAC once, macOS stays entirely in user space) → leave auto-guard on and updates to Desktop or the CLI get repaired on their own; the bottom card verifies real fast via `usage.speed` in the transcripts
8. Native Features tab → select Computer Use → Apply changes → confirm (approve UAC on Windows; select a valid signing identity first on macOS). Then open Desktop, enable Computer use in settings, and complete its consent flow. To undo the change, quit Desktop and choose Restore official app on the Backup & Restore tab

### Fast Mode unlock

> Prerequisite: the Console API key itself is entitled to fast mode server-side. This removes the **client-side** block only; billing and entitlement stay with the server.

Signing in with an API key puts Desktop in `deploymentMode:"3p"`, where the fast-mode capability is hard-coded to `blocked_by_platform`: no toggle in the UI, and the bundled CLI lands in `sdk_opt_in_required`. Yet the only client-side key is `flagSettings.fastMode`, read solely from the CLI's `--settings` flag, and the `setFastMode` IPC behind the toggle has no gate on the host side. Hence:

| Component | How | Location |
|---|---|---|
| wrapper (both platforms) | Replaces the CLI that Desktop actually launches, merges `fastMode:true` into `--settings` and forwards to the stashed official binary (renamed on Windows, moved bundle-and-all on macOS); Desktop only checks `.verified`, never the binary itself | Win `%LOCALAPPDATA%\Claude-3p\claude-code\<ver>\<build>\`<br>mac `~/Library/Application Support/Claude-3p/claude-code/<ver>/<build>/claude.app/Contents/MacOS/`<br>`<build>` is the first 12 hex digits of the manifest sha256; layouts with the CLI directly under `<ver>` are also detected |
| renderer patch (Windows only) | Rewrites the relevant anchors in the minified renderer: show the toggle when IPC is available and the model supports fast, decide support by model id, drop the disabled reason, and supply the `fast_mode` configuration for the newer model menu. Anchors are keyed on property names and string literals, with variable names captured by regex and filled back in, so a Desktop update that renames minified symbols does not break them. Backs up `.orig` first and always patches from that backup, so it is idempotent | `resources\ion-dist\assets\v1\` inside the MSIX package |

The wrapper makes sessions fast by default; the toggle switches at runtime.

The Fast toggle can appear as "Enable fast mode" at the bottom of the model menu after selecting a supported model; standalone-button layouts are also supported. To repair the patch, quit Desktop, click "Repair" on Claude++'s Fast Mode tab, then reopen Desktop.

On Windows, writing under WindowsApps needs administrator rights: Claude++ relaunches itself with a fixed flag to apply the renderer change (one UAC prompt), and the elevated helper accepts no path arguments.

The macOS wrapper runs in user space. It handles Desktop’s default disable flag and maps official static API keys into the native eligibility check without overriding an existing API-key configuration, managed policy, or server authorization.

Signed macOS resources cannot be edited in isolation: the ASAR entry hashes, the ASAR header hash in `Info.plist`, and the bundle signature must be updated together. A valid development identity can sign the modified application. The original Fast Mode selector is available as a separate Native Features selection, using the same backup, validation, signing, and restore workflow. The CLI wrapper remains independently managed. Electron fuses, hardened runtime, and library validation stay enabled.

The CLI half does not go through Gatekeeper's app-launch path (Desktop `posix_spawn`s it directly), but there is no getting around the kernel's signature check: the official CLI also runs under hardened runtime, and its signature is paired with `Contents/Info.plist`. Lift it out of the bundle or rename it and it stops counting as that bundle's main executable, the signature mismatches immediately and exec ends in SIGKILL (which Desktop then reports as "blocked by endpoint security", unrelated to any real EDR). So the official binary is stashed by moving **the entire bundle** to `claude-real.app` — the signature only cares about the `Contents/MacOS/<main executable>` structure, not what the bundle directory is called. The vacated `claude.app` keeps just a shell: `MacOS/claude` symlinked to Claude++ itself, `MacOS/claude-real` symlinked back to the real binary.

On every start Desktop reads the CLI's **first 8 bytes** and checks the Mach-O magic (`0xFEEDFACF` plus cputype, or a `0xCAFEBABE` universal binary); anything else invalidates the cache and re-downloads the whole bundle. So the stand-in cannot be a script — it is a **symlink to Claude++ itself**, which is a valid Mach-O the check follows the link to. When Claude++ finds it was invoked under the name `claude` it takes the wrapper branch: merge `fastMode` into `--settings`, then `exec` the official binary — same PID, so none of the Job Object machinery Windows needs. If the symlink breaks (Claude++ moved or deleted) Desktop simply re-downloads the official CLI, so the failure mode is "back to stock".

Both platforms use the native model capability to offer Fast controls. The macOS wrapper preserves an explicit `fastMode:false`. Model, organization, credit, and cooldown checks remain native; the response determines the actual speed.

Trust `usage.speed` in the transcript. The macOS native model selector uses `LocalSessions.setFastMode`; its Fast mark follows the CLI state. Native cooldown selection does not guarantee Fast speed for that response.

A Desktop update replaces the CLI version directory (and on Windows the renderer too). The daemon watches the CLI directory and periodically checks the Desktop version, repairing as soon as something is missing, or waiting for Desktop to exit first. The wrapper and the renderer are repaired independently, so one failing never holds up the other; a failed version combination is not retried automatically, but upgrading Claude++ gives it one more try. After the CLI moves into a build subdirectory, the daemon brings the stashed official binary back beside the wrapper, or removes the leftover when the build already holds a freshly downloaded official CLI. When an update merely renames minified symbols the anchors adapt on their own. Only a real structural change to the renderer makes the tab list unmatched anchors: the wrapper still works (supported sessions still default to fast, just without the toggle) and `ANCHORS` in `src-tauri/src/fastmode.rs` needs re-adapting with the same semantics; to check a given renderer first, run `cd src-tauri && cargo run --example fastmode_patch_probe -- "<renderer>.js"`, which lists per-anchor hit counts before and after the rewrite; pass an output path such as `out.mjs` as a second argument to write the rewritten bundle and confirm it still parses with `node --check out.mjs`. "Restore official" undoes everything in one click.

Fast Mode supports both a single renderer file and layouts with the main logic and model menu spread across multiple files. On Windows, repair validates every target before saving individual `.orig` backups and writing changes; a failed write rolls back the changes from that attempt. Status reports success only when all targets are patched. To inspect a split layout, pass the `assets/v1` directory to the probe and a separate output directory as the second argument, then run `node --check` on each exported `.mjs` file.

### Native features (Windows)

Targets the MSIX package on Windows x64. Quit Desktop entirely, pick the features you want on the Native Features tab, select Apply changes, and approve the UAC prompt. Reopen Desktop and turn the matching switches on in its settings.

Which files change depends on what you picked: main-process switches patch `resources/app.asar` and update the executable's `INTEGRITY / ELECTRONASAR` resource; Chinese and the Fast toggle patch renderer chunks under `resources/ion-dist`, and Chinese also writes the official catalogs. Everything is patched and verified on copies before replacement, under an operation lock with a recovery journal, so an interrupted run can be restored. ASAR validation fuses stay enabled; the patched executable no longer carries a valid official Authenticode signature, and restoring brings it back.

Records and backups live in `<config dir>/com.claudeplusplus.manager/computer-use/windows/`. Externally updated or modified Desktop files are never overwritten. An MSIX update replaces the whole package, so rescan and apply again afterwards.

For development validation, run `cargo run --example computer_use_probe -- status` from `src-tauri` to inspect the installed app without modifying it. Test independent copies with `status|install|restore COPY/app STORE`, where `COPY/app` is a separate app copy and `STORE` is a separate record directory outside it. Windows needs no signing-identity argument; the probe refuses system installation directories.

### Native Computer Use (macOS)

The Native Features tab provides status detection, signing-identity selection, and installation, while the Backup & Restore tab handles restoration; the detection result determines whether installation is available. The configuration function is located by content across main-process JavaScript files, without depending on a content-hashed chunk filename. Both the original entry hash and patched output hash must match the compatibility record. Unknown versions, ambiguous matches, and changed content are rejected rather than assumed compatible.

Installation supplies the native Computer Use configuration for API-key deployments, including the background-mode entry point in compatible apps. The CLI binary is unchanged and no separate MCP server is introduced. Enable Computer use in Claude settings and complete the consent flow. Background works inside allowed apps and asks before taking over the screen; Full control takes over the screen, mouse, and keyboard by default. HIPAA, session authorization, and denied-app checks remain intact.

Older managed patches show an upgrade action: verify the backup, restore the official bundle, then install the new patch. An interrupted upgrade can be restored and retried.

- A valid local code-signing certificate and private key are required. Apple Development signing has been verified; **ad-hoc signing is not supported**. No signing private key is bundled or exported.
- Quit the target Desktop first. Claude++ backs up the entire original bundle, patches a temporary copy, updates ASAR entry and header integrity, preserves non-identity entitlements per component, and re-signs nested code. Replacement happens only after strict signature verification.
- Re-signing changes application identity. Login or system permissions may need renewal; existing Keychain access and all Cowork scenarios are not guaranteed. Conversations, API keys, user configuration, and the system permissions database are not edited.
- Records, original bundles, and command logs live under `<config dir>/com.claudeplusplus.manager/computer-use/`. Restore official app can finish an interrupted transaction. Restoration verifies full-bundle hashes and refuses to overwrite an externally updated or modified app; backups are retained.
- No background re-signing by default, and automatic updates stay enabled; only the opt-in automatic update maintenance reapplies the previous selection after Desktop exits. Unsupported versions are not modified, and old backups do not overwrite newer versions.

For development validation, run `cargo run --example computer_use_probe -- status COPY/Claude.app STORE` from `src-tauri`; `install` additionally takes a signing-identity SHA-1, and `restore` restores the backup. The probe rejects system application paths and uses a separate app copy and state directory. The installed feature needs no Python, Node.js, or development toolchain.

### macOS native compatibility

Main-process patches, background controls, Chinese catalogs, simulators, prewarming, and native Fast state share verified build metadata. Original and output hashes cover every feature combination; content-based renderer discovery tolerates renamed chunks, while unknown content remains untouched. Existing build support and independent backups are retained.

System permissions, administrator policies, and native Fast eligibility checks remain intact. Update maintenance restores previously selected compatible features without enabling newly added options.

### Automatic update maintenance on macOS

Only macOS needs the restore-and-reapply flow because native enhancements re-sign the app. Windows continues to use its official updater and does not expose these controls.

Enable automatic maintenance under Native Features. Claude++ saves the feature selection and signing identity, queues work while Desktop is running, restores the verified official bundle after Desktop exits, and opens the official app in the background so its own updater can proceed. It never force-quits sessions or competes with the official installer.

After an update, compatible enhancements are reapplied when Desktop is stopped. Unknown builds remain official with the requested selection retained for a future compatible Claude++ release. The journal survives restarts; manual restoration disables automatic reapplication. Signature verification, credentials, conversations, permissions, and administrator policies are preserved.

### Build

```bash
npm install
npx tauri build   # Windows: bundle/nsis installer + claude-plus-plus.exe; macOS: bundle/macos/Claude++.app + bundle/dmg
```

Requires Node.js, Rust stable, and Tauri. Dependency details are maintained in the repository manifests and lockfiles.

```bash
cd src-tauri && cargo test    # unit tests (junction / migration fields / tombstone flow / idempotency)
```

### Safety

- Every write operation checks for a running Desktop bound to the target userData and refuses while it lives; Codex import likewise refuses while Codex runs
- Unification keeps originals intact as `.bak-<timestamp>`; one-click restore
- Transcripts are strictly read-only during unify and migration; Codex import only adds Claude transcripts, never touches Codex threads, and opens Codex's state database read-only; tombstone deletions go to the recycle bin
- Metadata writes use temp-file + atomic rename
- Preview windows are read-only; rendered content is DOMPurify-sanitized
- Computer Use checks versions, content, and backups before installation, patches a copy before replacement, and uses an operation lock and recovery journal. Restoration requires the app to match its installation record; native consent and denied-app checks remain intact
- Fast Mode stashes the official CLI untouched right next to where Desktop launches it (renamed on Windows, moved bundle-and-all on macOS), never downloading or altering binary content; on Windows the renderer is backed up as `.orig` before patching and removed on restore, and the elevated helper takes no path arguments and locates the MSIX package itself. Creating and removing `.orig` means adding and deleting entries inside the package directory, which under WindowsApps only TrustedInstaller and SYSTEM may do, so ownership of the `assets\v1` directory itself is taken (non-recursive, harmless to Desktop updates, which rebuild the directory with default ACLs); on macOS the wrapper changes only the CLI under the user directory; the optional native Fast toggle changes signed Desktop resources through the native-feature transaction

### Disclaimer

Unofficial tool. Relies on Claude Desktop's internal storage layout; future versions may change it and break features — the failure mode is "no effect", never data loss.

## License

AGPL-3.0-only
