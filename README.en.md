# Claude++

[简体中文](README.md) · **English**

Share sessions between Claude Desktop and Claude Code CLI on the same machine, import Codex conversations, and turn on Fast Mode, Computer Use and other native features for a Desktop signed in with an API key.

![Tauri](https://img.shields.io/badge/built%20with-Tauri-blue) ![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS-informational) ![License](https://img.shields.io/badge/license-AGPL--3.0-green)

The interface is in Chinese; tab names below are given as they appear in the app.

## Why

Desktop keeps Code session metadata separately for every account and organization. Sign in through another channel or account and your earlier sessions disappear from the list; CLI sessions never show up in Desktop at all because they have no metadata. The transcripts themselves have always been shared (`~/.claude/projects/`); only the metadata is split.

## Features

| Feature | What it does |
|---|---|
| Unify storage | Merges every account and organization into one pool, so switching channels or accounts no longer changes the list. Original folders are backed up and can be restored |
| Shared sessions | Register CLI sessions in Desktop and keep chatting; unregister them and `claude -r` still works in the CLI. Transcripts are never copied |
| Branch grouping | Folds the branches created by rewind, resume and compact into one group, and shows and registers only the latest by default |
| Codex import | Turns Codex-only conversations into Claude transcripts without duplicates on re-import; Codex's own sessions stay untouched |
| Preview | Click a session for a read-only preview; large sessions load only the last 300 messages |
| Delete and clean up | Delete a session on both sides, or clear the deletion markers Desktop leaves behind. Deleted transcripts go to the recycle bin |
| Fast Mode | Makes an API-key Desktop request Fast by default. The key itself must be entitled to fast |
| Native features | Turns on features Desktop ships but keeps off in API-key mode; see below |
| Tray daemon | Unifies new channels and accounts as they appear, and puts Fast Mode back after Desktop or CLI updates |

## Native features

| Feature | Windows | macOS |
|---|:-:|:-:|
| Computer Use | ✓ | ✓ (with background mode) |
| Simplified and Traditional Chinese | ✓ | ✓ |
| Fast Mode toggle | ✓ | ✓ |
| CLI prewarming (experimental) | ✓ | ✓ |
| Simulators | Android | iOS / Android |

- Pick and apply them on 功能增强 (Native features), restore on 备份与恢复 (Backup and restore). Any combination works, and every change is rebuilt from the official backup.
- After installing, turn the matching switches on in Desktop's settings and complete the permission prompts yourself.
- Windows: applying and restoring need administrator approval; the patched exe loses its official signature until you restore; apply again after each Desktop update. The Android emulator needs a local Android SDK with the emulator installed.
- macOS: needs a valid code-signing identity in your keychain; ad-hoc signing does not work. Re-signing changes the app's identity, so you may have to sign in or grant permissions again. With 自动维护 Desktop 更新 (automatic update maintenance) on, features are reapplied once an update finishes; builds not yet supported stay official.
- If Desktop was modified elsewhere or a new build is not recognized, Claude++ refuses to change anything rather than guess.

## Getting started

1. Download from [Releases](https://github.com/alksdesu/ClaudePlusPlus/releases/latest). On Windows use the installer or the portable exe; on macOS (Apple silicon) open `Claude++.app`, which is unsigned, so right-click → Open the first time.
2. Quit Claude Desktop completely. Every write is refused while it runs.
3. 存储归一 (Unify storage): unify in one click.
4. Claude 会话 (Claude sessions): select CLI sessions and register them, then open the project in Desktop to continue.
5. Codex 导入 (Codex import): migrate in one click (quit Codex first), then register the results under Claude 会话.
6. Fast Mode: install (one administrator prompt on Windows); the bottom of the page shows whether recent replies actually ran fast.
7. 功能增强 (Native features): pick the native features you want and apply.

On Windows the Fast toggle in the model menu comes either from the patch the Fast Mode tab installs or from the native toggle on 功能增强. Only one can stay; follow the prompts in the app to switch.

## Safety

- Nothing is written while Desktop or Codex is running.
- Unifying and migrating never modify transcripts; unifying keeps the original folders as `.bak-<timestamp>`; deletions go to the recycle bin.
- Desktop is backed up in full before any change, patched and verified on a copy before replacement, and can be restored if something fails midway.
- Only client-side restrictions are lifted; server-side billing, entitlement and organization policy are untouched.

## Build

Requires Node.js and Rust stable.

```bash
npm install
npx tauri build
```

Always package with `npx tauri build`: an exe from plain `cargo build` loads the dev server and opens to a blank page. Run tests with `npm run check`, `npm run test:ui` and `cd src-tauri && cargo test`.

## Disclaimer

Unofficial tool that relies on Claude Desktop internals; some features may need an update after Desktop changes.

## License

AGPL-3.0-only
