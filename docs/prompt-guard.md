# 屏蔽远端 heron 提示词

`tools/claude_prompt_guard.py` 是 macOS 本机工具，处理独立 Claude Code 和 Desktop 内置 CLI。它让 `tengu_heron_brook` 的消费函数返回空，覆盖 bootstrap、缓存、签名配置覆盖层和 GrowthBook 经此字段传入的文本。内置 Haiku 指导及其他提示词功能保留。

它只屏蔽这个字段，不代表所有服务端提示词入口都已关闭，也不能改变服务端自己的处理行为。

## 使用

需要 Python 3.9 或以上及 macOS 自带的 `codesign`。

```sh
python3 tools/claude_prompt_guard.py status ~/.local/bin/claude
python3 tools/claude_prompt_guard.py apply ~/.local/bin/claude
python3 tools/claude_prompt_guard.py restore ~/.local/bin/claude
```

Desktop 传入当前实际 CLI 的可执行文件路径；已有 Fast wrapper 时，传入 `claude-real.app/Contents/MacOS/claude`。工具也会解析符号链接。

应用前会验证原签名、备份原文件或完整 CLI bundle，并在副本上修改和重新签名。通过回读、签名检查和启动检查后原子交换到正式位置。还原要求当前文件与记录的修改结果一致，备份校验通过后恢复原始内容和原始签名。文件已被外部修改时拒绝覆盖。

备份及事务记录默认存放在：

```text
~/Library/Application Support/com.claudeplusplus.manager/prompt-guard/
```

`--state-dir` 可以指定测试用记录目录。正式备份需要保留，以便还原。

## 会话与更新

- 新启动的 CLI 进程生效；安装不会强制结束正在运行的会话。Desktop 完全退出后重新打开，确保旧进程不再使用原代码。
- 旧会话的完整提示词快照可能已经包含远端文本。补丁忽略未标记的快照，按当前官方逻辑重新生成，再在本地快照元数据里写入 `cppHeronGuard: 1`。该标记不发送给模型，也不修改聊天消息。后续仍正常复用快照。
- 官方 CLI 可以读取带额外字段的快照；还原工具不会删聊天记录。
- 修改的代码模块会重新编译，冷启动可能比原版慢。其余模块保留字节码。
- 客户端更新可能替换补丁。更新后用 `status` 核对当前可执行文件，再按需重新应用。未知 Bun 布局或不唯一的源码结构会被拒绝。Claude++ 的「提示词屏蔽」页可以识别本工具的安装记录，并从原始备份升级或还原。此独立工具不提供自动守护。

## 验证

```sh
python3 tools/test_claude_prompt_guard.py --binary /path/to/original/claude
```

`--binary` 可重复指定。需要未修改的官方 CLI；可使用工具保留的原始备份。

测试在临时目录启动真实 CLI 可执行文件，使用虚构 key 和本机 HTTP 模拟接口检查实际请求。覆盖两个文本来源的原版对照、屏蔽、Haiku 指导保留、旧会话恢复、再次恢复、新快照标记持久化、还原后读取快照、重复安装与还原、外部修改拒绝和无效二进制拒绝。测试目录自动删除。

## 二进制处理

保留原 Mach-O 布局，仅修改识别出的消费与快照处理代码。Bun 模块记录中的旧字节码、源码哈希及预链接图会绕过修改后的源码，因此对应缓存一并失效。快照写入模块放入已停用、并确认与其他模块引用不重叠的预链接区域；文件大小及 Mach-O section 布局保持不变，签名由系统工具重新生成。

二进制布局核对依据为 [Bun 的模块图实现](https://github.com/oven-sh/bun/blob/main/src/standalone_graph/StandaloneModuleGraph.rs)，最终以本机结构检查和运行验证为准。
