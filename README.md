# Claude++

**简体中文** · [English](README.en.md)

让同一台机器上的 Claude Desktop 和 Claude Code CLI 共用会话，导入 Codex 对话，并为 API key 登录的 Desktop 打开 Fast Mode、Computer Use 等原生功能。

![Tauri](https://img.shields.io/badge/built%20with-Tauri-blue) ![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS-informational) ![License](https://img.shields.io/badge/license-AGPL--3.0-green)

## 为什么需要

Desktop 按账号和组织分开存放 Code 会话的元数据。换个渠道或账号登录，之前的会话就从列表里消失；CLI 会话没有这份元数据，在 Desktop 里根本看不到。会话转录其实一直是两边共用的（`~/.claude/projects/`），被隔开的只是元数据。

## 功能

| 功能 | 说明 |
|---|---|
| 存储归一 | 把所有账号、组织的会话合到一个池里，切渠道、换账号后列表不变。原目录先备份，随时能还原 |
| 会话互认 | CLI 会话注册到 Desktop 里接着聊；从 Desktop 注销后，CLI 照样能 `claude -r`。转录不复制 |
| 分支归组 | rewind、resume、compact 产生的分支折叠成一组，默认只显示、只注册最新的一条 |
| Codex 导入 | 把 Codex 独有的对话转成 Claude 转录，重复导入不会多出副本，Codex 的会话本身不动 |
| 会话预览 | 单击会话打开只读预览，大会话只加载最后 300 条 |
| 删除与清理 | 两边一起删除会话，或清理 Desktop 留下的删除标记。删掉的转录进系统回收站 |
| Fast Mode | 让 API key 登录的 Desktop 默认请求 Fast。前提是 key 本身有 fast 资格 |
| 原生功能 | 打开 Desktop 自带、但对 API key 模式关着的功能，见下节 |
| 托盘守护 | 新渠道、新账号出现时自动归一；Desktop 或 CLI 更新后自动补上 Fast Mode |

## 原生功能

| 功能 | Windows | macOS |
|---|:-:|:-:|
| Computer Use | ✓ | ✓（含后台模式） |
| 简体与繁体中文 | ✓ | ✓ |
| Fast Mode 原生开关 | ✓ | ✓ |
| CLI 原生预热（实验） | ✓ | ✓ |
| 模拟器 | Android | iOS / Android |

- 在「功能增强」页勾选并应用，在「备份与恢复」页还原。功能可以任意组合，每次都从官方备份重新生成。
- 装完要去 Desktop 设置里自己打开对应开关、完成授权。
- Windows：应用和还原都要管理员确认；改过的 exe 不再带官方签名，还原后恢复；Desktop 更新后要回来重新应用。Android 模拟器需要本机装好 Android SDK 和模拟器。
- macOS：需要钥匙串里有效的代码签名身份，不支持 ad-hoc。重签会改变应用身份，可能要重新登录、重新授权。开启「自动维护 Desktop 更新」后，更新装完会自动把功能装回去，新版还没适配的先保持官方原样。
- macOS 会按代码结构识别新版入口，支持目录和 ASAR 两种资源布局；结构或完整性校验不通过时停止安装，保留官方版本。
- Desktop 被外部改过，或者新版的结构认不出来，都会拒绝修改，不会去猜。

## 上手

1. 从 [Releases](https://github.com/alksdesu/ClaudePlusPlus/releases/latest) 下载。Windows 用安装包或免安装 exe；macOS（Apple 芯片）打开 `Claude++.app`，应用没有签名，第一次要右键 → 打开。
2. 完全退出 Claude Desktop。Desktop 开着时，所有写操作都会被拒绝。
3. 「存储归一」页一键归一。
4. 「Claude 会话」页勾选 CLI 会话并注册，在 Desktop 里打开对应项目就能接着聊。
5. 「Codex 导入」页一键迁移（先退出 Codex），再回「Claude 会话」页注册。
6. 「Fast Mode」页点安装（Windows 会弹一次管理员确认），页面底部能看到最近的回复有没有真的走 fast。
7. 「功能增强」页勾选要装的原生功能，点「应用更改」。

Windows 上模型菜单里的 Fast 开关有两个来源：「Fast Mode」页安装时附带的补丁，或「功能增强」页的原生开关。两者只能留一个，切换时按页面提示操作。

## 安全

- Desktop 或 Codex 开着时拒绝写入。
- 归一和迁移不改会话转录；归一前原目录保留为 `.bak-<时间戳>`；删除一律进回收站。
- 改 Desktop 前先完整备份，在副本上改完并校验才替换，中途失败可以还原。
- 只解除客户端的限制，不碰服务端的计费、授权和组织策略。

## 构建

需要 Node.js 和 Rust stable。

```bash
npm install
npx tauri build
```

出包要用 `npx tauri build`，直接 `cargo build` 出的 exe 会去连开发服务器，打开是空白页。测试用 `npm run check`、`npm run test:ui` 和 `cd src-tauri && cargo test`。

## 免责

非官方工具，依赖 Claude Desktop 的内部结构，Desktop 更新后部分功能可能要等适配。

## License

AGPL-3.0-only
