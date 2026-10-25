use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use super::rules;
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[cfg(any(test, target_os = "macos"))]
struct Build {
    version: &'static str,
    original_hash: &'static str,
    patched_hash: &'static str,
    legacy_hash: Option<&'static str>,
}

#[cfg(any(test, target_os = "macos"))]
const BUILDS: &[Build] = &[
    Build {
        version: "3.1623.1",
        original_hash: "429f11be76446920f3d2b339b6fbc1223c45632b511036b267e35a2d9f3f7fc3",
        patched_hash: "e599ab50e6ec0ac6745f5873ad366ef739dcb3113a63bc07b5acea963ba5d81a",
        legacy_hash: None,
    },
    Build {
        version: "3.1623.4",
        original_hash: "40a2851c92a686453694dfc3bbc6a62433f4d21772c24725ea93ba835caad198",
        patched_hash: "d7a7a0c8c38617b7065f3c441497b3d0122fa8e45c6f0ec5b01c311ff6c1fe6c",
        legacy_hash: Some("4e50ee6e3bb171c26b8bd4cbe77d9bd1dda4be5c8880faaef5ce65dc344701ba"),
    },
];
#[cfg(any(test, target_os = "macos"))]
const BEFORE: &str = "function Fen(e){return e.selfHostedSessions?e.builtinBrowserEnabled?Pen:Nen:e.builtinBrowserEnabled?Aen:Den}";
#[cfg(any(test, target_os = "macos"))]
const AFTER: &str = "function Fen(e){let t=e.selfHostedSessions?e.builtinBrowserEnabled?Pen:Nen:e.builtinBrowserEnabled?Aen:Den;return{...t,1291166712:Ten({enabled:!0})}}";
#[cfg(any(test, target_os = "macos"))]
const BACKGROUND_AFTER: &str = "function Fen(e){let t=e.selfHostedSessions?e.builtinBrowserEnabled?Pen:Nen:e.builtinBrowserEnabled?Aen:Den;return{...t,1291166712:Ten({enabled:!0,appScopedEnabled:!0}),1346958739:Ten({major:27,minor:9999})}}";
#[cfg(any(test, target_os = "macos"))]
const MODE_BEFORE: &str = "function YU(e={}){let t=e.requireFreshGrowthBook??!0;if(process.platform!==\"darwin\")return!1;let n=_O();return n.major<15||S_r(n,x_r())||t&&!GU()?!1:wb(WU,\"appScopedEnabled\",!1,O())}";
#[cfg(any(test, target_os = "macos"))]
const MODE_AFTER: &str = "function YU(e={}){let t=e.requireFreshGrowthBook??!0;if(process.platform!==\"darwin\"&&process.platform!==\"win32\")return!1;if(process.platform===\"darwin\"){let n=_O();if(n.major<15||S_r(n,x_r()))return!1}return t&&!GU()?!1:wb(WU,\"appScopedEnabled\",!1,O())}";
#[cfg(any(test, target_os = "macos"))]
const FAST_0_BEFORE: &str = r#"for(let t of["badge","tooltip","notice","thinking"])"#;
#[cfg(any(test, target_os = "macos"))]
const FAST_0_AFTER: &str = r#"for(let t of["badge","tooltip","notice","thinking","fast_mode"])"#;
#[cfg(any(test, target_os = "macos"))]
const FAST_1_BEFORE: &str = r#"g={id:i.id,name:f,description:h,thinking:Xw(t,i.id,e),"#;
#[cfg(any(test, target_os = "macos"))]
const FAST_1_AFTER: &str = r#"g={id:i.id,name:f,description:h,thinking:Xw(t,i.id,e),...t==="code"&&s?.fast_mode!==void 0&&{fast_mode:s.fast_mode},"#;
#[cfg(any(test, target_os = "macos"))]
const FAST_2_BEFORE: &str = r#"claude_code_fast_mode:"blocked_by_platform""#;
#[cfg(any(test, target_os = "macos"))]
const FAST_2_AFTER: &str = r#"claude_code_fast_mode:t(n&&iw().allowsLocalSpawn)"#;
#[cfg(any(test, target_os = "macos"))]
const FAST_3_BEFORE: &str = r#"let o=o9t(t.thinking);return o&&(n.thinking=o),Object.keys(n).length>0?n:void 0"#;
#[cfg(any(test, target_os = "macos"))]
const FAST_3_AFTER: &str = r#"let o=o9t(t.thinking);return o&&(n.thinking=o),ww(t.fast_mode)&&(n.fast_mode=t.fast_mode),Object.keys(n).length>0?n:void 0"#;
const MAX_HEADER: u64 = 32 * 1024 * 1024;
const MAX_ENTRY: u64 = 64 * 1024 * 1024;

/// macOS 逐版本记录用它区分官方、旧补丁与新补丁
#[cfg(any(test, target_os = "macos"))]
#[derive(Debug, PartialEq, Eq)]
pub enum PatchState {
    Pristine,
    Legacy,
    Patched,
}

#[cfg(any(test, target_os = "macos"))]
fn build(version: &str) -> Result<&'static Build> {
    BUILDS
        .iter()
        .find(|build| build.version == version)
        .context("当前 Desktop 尚未适配；请更新 Claude++ 后重新扫描，不会猜测锚点")
}

#[cfg(target_os = "macos")]
pub fn check_version(version: &str) -> Result<()> {
    if super::macos_recipe::get(version).is_some() { return Ok(()); }
    build(version).map(|_| ())
}

#[cfg(any(test, target_os = "macos"))]
fn classify_hash(hash: &str, build: &Build) -> Result<PatchState> {
    if hash == build.original_hash {
        Ok(PatchState::Pristine)
    } else if hash == build.patched_hash
        || build.version == "3.1623.4" && profile_mask(hash).is_some()
    {
        Ok(PatchState::Patched)
    } else if Some(hash) == build.legacy_hash {
        Ok(PatchState::Legacy)
    } else {
        anyhow::bail!(
            "主进程入口哈希与 Desktop {} 的适配记录不符，拒绝修改",
            build.version
        )
    }
}

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) struct Archive {
    file: File,
    header: Value,
    raw_header: Vec<u8>,
    payload_start: u64,
    length: u64,
}

fn field(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn entry_mut<'a>(header: &'a mut Value, path: &[&str]) -> Result<&'a mut Value> {
    let mut value = header;
    for part in path {
        value = value
            .get_mut("files")
            .and_then(|files| files.get_mut(*part))
            .context("ASAR 中没有已适配的主进程入口")?;
    }
    Ok(value)
}

impl Archive {
    fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path).context("读取 app.asar")?;
        let length = file.metadata()?.len();
        let mut prefix = [0u8; 16];
        file.read_exact(&mut prefix).context("ASAR 文件头不完整")?;
        let header_size = u64::from(field(&prefix, 4));
        let pickle_size = u64::from(field(&prefix, 8));
        let json_size = u64::from(field(&prefix, 12));
        ensure!(
            field(&prefix, 0) == 4 && header_size <= MAX_HEADER,
            "ASAR 文件头不受支持"
        );
        ensure!(
            header_size == pickle_size + 4
                && pickle_size >= json_size + 4
                && pickle_size - json_size - 4 < 4
                && pickle_size % 4 == 0,
            "ASAR pickle 长度异常"
        );
        let payload_start = 8 + header_size;
        ensure!(payload_start <= length, "ASAR 文件被截断");
        let mut raw_header = vec![0; json_size as usize];
        file.read_exact(&mut raw_header)?;
        let header = serde_json::from_slice(&raw_header).context("解析 ASAR 文件头")?;
        Ok(Self {
            file,
            header,
            raw_header,
            payload_start,
            length,
        })
    }

    /// 只读不校验；调用方必须在使用内容前调 verify_entry
    fn read(&mut self, path: &[&str]) -> Result<Vec<u8>> {
        let entry = entry_mut(&mut self.header, path)?;
        ensure!(
            entry.get("link").is_none() && entry.get("unpacked").is_none(),
            "主进程入口不是普通打包文件"
        );
        let offset: u64 = entry
            .get("offset")
            .and_then(Value::as_str)
            .context("缺少 ASAR offset")?
            .parse()?;
        let size = entry
            .get("size")
            .and_then(Value::as_u64)
            .context("缺少 ASAR size")?;
        ensure!(size <= MAX_ENTRY, "ASAR 入口过大，拒绝处理未知布局");
        let end = offset.checked_add(size).context("ASAR offset 溢出")?;
        ensure!(end <= self.length - self.payload_start, "ASAR 入口越界");
        self.file
            .seek(SeekFrom::Start(self.payload_start + offset))?;
        let mut source = vec![0; size as usize];
        self.file.read_exact(&mut source)?;
        Ok(source)
    }

    fn verify_entry(&mut self, path: &[&str], source: &[u8]) -> Result<()> {
        let entry = entry_mut(&mut self.header, path)?;
        verify_integrity(entry.get("integrity").context("入口没有完整性信息")?, source)
    }

    #[cfg(any(test, target_os = "macos"))]
    fn source(&mut self, path: &[&str]) -> Result<Vec<u8>> {
        let source = self.read(path)?;
        self.verify_entry(path, &source)?;
        Ok(source)
    }

    #[cfg(any(test, target_os = "macos"))]
    fn locate(&mut self) -> Result<(String, Vec<u8>)> {
        self.locate_anchors(&[BEFORE, NATIVE_PREFIX])
    }

    fn main_entries(&self) -> Result<Vec<String>> {
        Ok(self
            .header
            .pointer("/files/.vite/files/build/files")
            .and_then(Value::as_object)
            .context("ASAR 中没有主进程代码目录")?
            .keys()
            .filter(|name| name.starts_with("index") && name.ends_with(".js"))
            .cloned()
            .collect())
    }

    #[cfg(any(test, target_os = "macos"))]
    fn locate_anchors(&mut self, anchors: &[&str]) -> Result<(String, Vec<u8>)> {
        let names = self.main_entries()?;
        let mut found = None;
        for name in names {
            let source = self.source(&[".vite", "build", &name])?;
            if !anchors
                .iter()
                .any(|anchor| memchr::memmem::find(&source, anchor.as_bytes()).is_some())
            {
                continue;
            }
            let text = std::str::from_utf8(&source).context("主进程入口不是 UTF-8")?;
            ensure!(
                found.is_none()
                    && anchors
                        .iter()
                        .map(|anchor| text.matches(anchor).count())
                        .sum::<usize>()
                        == 1,
                "Computer Use 配置函数命中不唯一，拒绝修改"
            );
            found = Some((name, source));
        }
        found.context("未找到 Computer Use 配置函数，主进程结构可能已变化")
    }

    /// 规则路径按字面标记预筛再交给规则精确判定，不依赖带哈希的 chunk 名。
    /// 几百个入口只有命中的那个参与后续逻辑，完整性校验也只对它做
    fn locate_marker(&mut self, marker: &str, label: &str) -> Result<(String, String)> {
        let mut found = None;
        for name in self.main_entries()? {
            let path = [".vite", "build", name.as_str()];
            let source = self.read(&path)?;
            if memchr::memmem::find(&source, marker.as_bytes()).is_none() {
                continue;
            }
            ensure!(found.is_none(), "{label}入口不唯一，拒绝修改");
            self.verify_entry(&path, &source)?;
            let text = String::from_utf8(source).context("主进程入口不是 UTF-8")?;
            found = Some((name, text));
        }
        found.with_context(|| format!("未找到{label}入口"))
    }
}

pub(super) fn header_hash(path: &Path) -> Result<String> {
    Ok(hash(&Archive::open(path)?.raw_header))
}

const MAIN_MARKER: &str = ".selfHostedSessions?";
const PREWARM_MARKER: &str = "async sparePrewarmSupport(){";

/// 规则路径：结构化锚点现场识别，不需要逐版本记录
pub(super) fn rules_selection(
    path: &Path,
    platform: rules::Platform,
) -> Result<super::native::Selection> {
    let mut archive = Archive::open(path)?;
    let (_, main) = archive.locate_marker(MAIN_MARKER, "原生配置")?;
    let mask = rules::main_mask(&main, platform)?;
    let (_, prewarm) = archive.locate_marker(PREWARM_MARKER, "原生预热")?;
    ensure!(
        rules::prewarm_installed(&prewarm)? == (mask & rules::PREWARM != 0),
        "原生预热模块与配置开关状态不一致"
    );
    Ok(super::native::Selection {
        computer_use: mask & rules::COMPUTER_USE != 0,
        simulators: mask & rules::SIMULATORS != 0,
        prewarm: mask & rules::PREWARM != 0,
        fast_mode_status: mask & rules::FAST_STATUS != 0,
        chinese: false,
    })
}

pub(super) fn patch_with_rules(
    path: &Path,
    platform: rules::Platform,
    selection: super::native::Selection,
) -> Result<String> {
    let mask = selection.mask();
    ensure!(
        rules_selection(path, platform)?.mask() == 0,
        "必须从官方基线组合原生补丁"
    );
    // 预热的开关字段也在主进程配置函数里，只改独立模块不会生效
    if mask != 0 {
        let mut archive = Archive::open(path)?;
        let (name, main) = archive.locate_marker(MAIN_MARKER, "原生配置")?;
        let modified = rules::patch_main(&main, platform, mask)?;
        replace_entry(path, archive, &name, modified.as_bytes())?;
    }
    if mask & rules::PREWARM != 0 {
        let mut archive = Archive::open(path)?;
        let (name, prewarm) = archive.locate_marker(PREWARM_MARKER, "原生预热")?;
        let modified = rules::patch_prewarm(&prewarm)?;
        replace_entry(path, archive, &name, modified.as_bytes())?;
    }
    ensure!(
        rules_selection(path, platform)?.mask() == mask,
        "原生组合补丁回读不一致"
    );
    Ok(hash(&Archive::open(path)?.raw_header))
}

fn integrity(source: &[u8], block_size: u64) -> Result<Value> {
    ensure!(
        block_size > 0 && block_size <= MAX_ENTRY,
        "ASAR blockSize 不受支持"
    );
    Ok(
        json!({"algorithm":"SHA256", "hash":hash(source), "blockSize":block_size,
        "blocks":source.chunks(block_size as usize).map(hash).collect::<Vec<_>>() }),
    )
}

fn verify_integrity(value: &Value, source: &[u8]) -> Result<()> {
    let block_size = value
        .get("blockSize")
        .and_then(Value::as_u64)
        .context("缺少 ASAR blockSize")?;
    ensure!(
        *value == integrity(source, block_size)?,
        "ASAR 入口完整性校验失败"
    );
    Ok(())
}

#[cfg(any(test, target_os = "macos"))]
pub fn inspect(path: &Path, version: &str) -> Result<(PatchState, String)> {
    if let Some(recipe) = super::macos_recipe::get(version) {
        let mut archive = Archive::open(path)?;
        let (_, source) = archive.locate_anchors(&[&recipe.main.anchor])?;
        let mask = recipe.main.mask(std::str::from_utf8(&source)?)?;
        return Ok((if mask == 0 { PatchState::Pristine } else { PatchState::Patched }, hash(&archive.raw_header)));
    }
    let build = build(version)?;
    let mut archive = Archive::open(path)?;
    let (_, source) = archive.locate()?;
    let state = classify_hash(&hash(&source), build)?;
    Ok((state, hash(&archive.raw_header)))
}

#[cfg(any(test, target_os = "macos"))]
fn patch_source(source: &str, background: bool) -> Result<String> {
    ensure!(
        source.matches(BEFORE).count() == 1
            && !source.contains(AFTER)
            && !source.contains(BACKGROUND_AFTER),
        "Computer Use 锚点必须唯一且未修改"
    );
    if background {
        ensure!(
            source.matches(MODE_BEFORE).count() == 1 && !source.contains(MODE_AFTER),
            "后台模式锚点必须唯一且未修改"
        );
        Ok(source
            .replacen(BEFORE, BACKGROUND_AFTER, 1)
            .replacen(MODE_BEFORE, MODE_AFTER, 1))
    } else {
        Ok(source.replacen(BEFORE, AFTER, 1))
    }
}

#[cfg(any(test, target_os = "macos"))]
pub fn patch(path: &Path, version: &str) -> Result<String> {
    if super::macos_recipe::get(version).is_some() {
        return patch_features(path, version, super::native::Selection { computer_use: true, ..Default::default() });
    }
    let build = build(version)?;
    let mut archive = Archive::open(path)?;
    let (name, source) = archive.locate()?;
    ensure!(
        classify_hash(&hash(&source), build)? == PatchState::Pristine,
        "只允许从已验证的官方主进程入口生成补丁"
    );
    let modified =
        patch_source(std::str::from_utf8(&source)?, build.legacy_hash.is_some())?.into_bytes();
    ensure!(hash(&modified) == build.patched_hash, "补丁输出哈希不匹配");
    replace_entry(path, archive, &name, &modified)
}

fn replace_entry(path: &Path, mut archive: Archive, name: &str, modified: &[u8]) -> Result<String> {
    let entry = entry_mut(&mut archive.header, &[".vite", "build", name])?;
    let block_size = entry["integrity"]["blockSize"]
        .as_u64()
        .context("缺少 ASAR blockSize")?;
    entry["offset"] = json!((archive.length - archive.payload_start).to_string());
    entry["size"] = json!(modified.len());
    entry["integrity"] = integrity(modified, block_size)?;
    let header = serde_json::to_vec(&archive.header)?;
    ensure!(
        header.len() as u64 <= MAX_HEADER - 16,
        "修改后的 ASAR 文件头过大"
    );
    let padding = (4 - header.len() % 4) % 4;
    let pickle_size = 4 + header.len() as u32 + padding as u32;
    let tmp = path.with_extension(format!("asar.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut out = File::options().write(true).create_new(true).open(&tmp)?;
        out.set_permissions(archive.file.metadata()?.permissions())?;
        for n in [4, pickle_size + 4, pickle_size, header.len() as u32] {
            out.write_all(&n.to_le_bytes())?;
        }
        out.write_all(&header)?;
        out.write_all(&[0; 3][..padding])?;
        archive.file.seek(SeekFrom::Start(archive.payload_start))?;
        std::io::copy(&mut archive.file, &mut out)?;
        out.write_all(modified)?;
        out.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    Ok(hash(&header))
}



#[cfg(any(test, target_os = "macos"))]
const NATIVE_PREFIX: &str = "function Fen(e){let t=e.selfHostedSessions?e.builtinBrowserEnabled?Pen:Nen:e.builtinBrowserEnabled?Aen:Den;return{...t,";
#[cfg(any(test, target_os = "macos"))]
const PREWARM_BEFORE: &str = "if(t.e7().type!==\"1p\")return{unsupported:\"third_party\"};";
#[cfg(any(test, target_os = "macos"))]
const PREWARM_ORIGINAL: &str = "f834d7febfc57c4957968eaa33a96f2f0c9454d2b8e8fbc89036189fb005fc6d";
#[cfg(any(test, target_os = "macos"))]
const PREWARM_PATCHED: &str = "595d1ec6852a4970f7b2e2d97b998ca052e3060c3aeeaf4ab6bd51ea77cfd391";

#[cfg(any(test, target_os = "macos"))]
fn profile_hash(mask: u8) -> Result<String> {
    let profiles: Vec<Value> =
        serde_json::from_str(include_str!("resources/native-profiles.json"))?;
    profiles
        .iter()
        .find(|p| p["mask"].as_u64() == Some(u64::from(mask)))
        .and_then(|p| p["sha256"].as_str())
        .map(str::to_owned)
        .context("未知原生功能组合")
}

#[cfg(any(test, target_os = "macos"))]
fn profile_mask(hash: &str) -> Option<u8> {
    let profiles: Vec<Value> = serde_json::from_str(include_str!("resources/native-profiles.json")).ok()?;
    profiles.iter().find(|profile| profile["sha256"].as_str() == Some(hash))
        .and_then(|profile| profile["mask"].as_u64()).and_then(|mask| u8::try_from(mask).ok())
}

#[cfg(any(test, target_os = "macos"))]
pub(super) fn fast_controls_current(path: &Path, version: &str) -> Result<bool> {
    if super::macos_recipe::get(version).is_some() {
        return Ok(active_selection(path, version)?.fast_mode_status);
    }
    let mut archive = Archive::open(path)?;
    let (_, bytes) = archive.locate()?;
    let digest = hash(&bytes);
    let mask = profile_mask(&digest).context("原生功能组合未适配")?;
    Ok(mask & 8 != 0 && profile_hash(mask)? == digest)
}

#[cfg(any(test, target_os = "macos"))]
pub fn active_selection(path: &Path, version: &str) -> Result<super::native::Selection> {
    if let Some(recipe) = super::macos_recipe::get(version) {
        let mut archive = Archive::open(path)?;
        let (_, source) = archive.locate_anchors(&[&recipe.main.anchor])?;
        let mask = recipe.main.mask(std::str::from_utf8(&source)?)?;
        let anchor = recipe.prewarm.anchor.as_deref().context("缺少预热适配入口")?;
        let (_, prewarm) = archive.locate_anchors(&[anchor])?;
        recipe.prewarm.verify(std::str::from_utf8(&prewarm)?, mask & 4 != 0)?;
        return Ok(super::native::Selection { computer_use: mask & 1 != 0, simulators: mask & 2 != 0, prewarm: mask & 4 != 0, fast_mode_status: mask & 8 != 0, chinese: false });
    }
    let mut archive = Archive::open(path)?;
    let (_, bytes) = archive.locate()?;
    let digest = hash(&bytes);
    let build = build(version)?;
    let state = classify_hash(&digest, build)?;
    let mask = if version == "3.1623.4" {
        profile_mask(&digest).unwrap_or(1)
    } else {
        u8::from(state != PatchState::Pristine)
    };
    if version == "3.1623.4" {
        let (_, source) = archive.locate_anchors(&["async sparePrewarmSupport(){"])?;
        ensure!(
            hash(&source)
                == if mask & 4 != 0 {
                    PREWARM_PATCHED
                } else {
                    PREWARM_ORIGINAL
                },
            "原生预热模块校验失败"
        );
    }
    Ok(super::native::Selection {
        computer_use: mask & 1 != 0,
        simulators: mask & 2 != 0,
        prewarm: mask & 4 != 0,
        chinese: false,
        fast_mode_status: mask & 8 != 0,
    })
}

#[cfg(any(test, target_os = "macos"))]
fn native_source(source: &str, selection: super::native::Selection) -> Result<String> {
    ensure!(source.matches(BEFORE).count() == 1, "原生配置入口不唯一");
    let mut fields = Vec::new();
    if selection.computer_use {
        fields.extend([
            "1291166712:Ten({enabled:!0,appScopedEnabled:!0})",
            "1346958739:Ten({major:27,minor:9999})",
        ]);
    }
    if selection.simulators {
        fields.extend(["3577536076:Ten(!0)", "1403324732:Ten(!0)"]);
    }
    if selection.prewarm {
        fields.push("4237076858:Ten(!0)");
    }
    let mut result = if fields.is_empty() {
        source.to_owned()
    } else {
        source.replacen(
            BEFORE,
            &format!("{NATIVE_PREFIX}{}}}}}", fields.join(",")),
            1,
        )
    };
    if selection.computer_use {
        ensure!(
            result.matches(MODE_BEFORE).count() == 1 && !result.contains(MODE_AFTER),
            "原生后台入口不唯一"
        );
        result = result.replacen(MODE_BEFORE, MODE_AFTER, 1);
    }
    if selection.fast_mode_status {
        for (before, after) in [(FAST_0_BEFORE, FAST_0_AFTER), (FAST_1_BEFORE, FAST_1_AFTER), (FAST_2_BEFORE, FAST_2_AFTER), (FAST_3_BEFORE, FAST_3_AFTER)] {
            ensure!(result.matches(before).count() == 1 && !result.contains(after), "Fast Mode 原生映射不唯一");
            result = result.replacen(before, after, 1);
        }
    }
    Ok(result)
}

#[cfg(any(test, target_os = "macos"))]
pub fn patch_features(
    path: &Path,
    version: &str,
    selection: super::native::Selection,
) -> Result<String> {
    super::native::check_selection(version, selection)?;
    if let Some(recipe) = super::macos_recipe::get(version) {
        ensure!(active_selection(path, version)?.is_empty(), "必须从官方基线组合原生补丁");
        let mut archive = Archive::open(path)?;
        let (name, source) = archive.locate_anchors(&[&recipe.main.anchor])?;
        let modified = recipe.main.patch(std::str::from_utf8(&source)?, selection.mask())?;
        if selection.mask() != 0 { replace_entry(path, archive, &name, modified.as_bytes())?; }
        if selection.prewarm {
            let mut archive = Archive::open(path)?;
            let anchor = recipe.prewarm.anchor.as_deref().context("缺少预热适配入口")?;
            let (name, source) = archive.locate_anchors(&[anchor])?;
            let modified = recipe.prewarm.patch(std::str::from_utf8(&source)?)?;
            replace_entry(path, archive, &name, modified.as_bytes())?;
        }
        return Ok(hash(&Archive::open(path)?.raw_header));
    }
    if version != "3.1623.4" {
        return if selection.computer_use {
            patch(path, version)
        } else {
            Ok(hash(&Archive::open(path)?.raw_header))
        };
    }
    let mut archive = Archive::open(path)?;
    let (name, source) = archive.locate()?;
    ensure!(
        hash(&source) == build(version)?.original_hash,
        "必须从官方基线组合原生补丁"
    );
    let modified = native_source(std::str::from_utf8(&source)?, selection)?;
    ensure!(
        hash(modified.as_bytes()) == profile_hash(selection.mask())?,
        "原生组合补丁输出不匹配"
    );
    if selection.mask() != 0 {
        replace_entry(path, archive, &name, modified.as_bytes())?;
    }
    if selection.prewarm {
        let mut archive = Archive::open(path)?;
        let (name, source) = archive.locate_anchors(&["async sparePrewarmSupport(){"])?;
        ensure!(hash(&source) == PREWARM_ORIGINAL, "预热原生模块内容未适配");
        let text = std::str::from_utf8(&source)?;
        ensure!(
            text.matches(PREWARM_BEFORE).count() == 1,
            "预热部署门槛不唯一"
        );
        let modified = text.replacen(PREWARM_BEFORE, "", 1);
        ensure!(
            hash(modified.as_bytes()) == PREWARM_PATCHED,
            "预热模块输出校验失败"
        );
        replace_entry(path, archive, &name, modified.as_bytes())?;
    }
    Ok(hash(&Archive::open(path)?.raw_header))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_ENTRY: &[&str] = &[".vite", "build", "index.chunk-fixture.js"];

    #[test]
    fn native_combinations_preserve_unselected_features_and_checks() {
        for mask in 0..16 {
            let selection = super::super::native::Selection {
                computer_use: mask & 1 != 0,
                simulators: mask & 2 != 0,
                prewarm: mask & 4 != 0,
                chinese: false,
                fast_mode_status: mask & 8 != 0,
            };
            let input = format!("{BEFORE};{MODE_BEFORE};{FAST_0_BEFORE};{FAST_1_BEFORE};{FAST_2_BEFORE};{FAST_3_BEFORE};permissions();");
            let output = native_source(&input, selection).unwrap();
            assert!(output.ends_with(";permissions();"));
            assert_eq!(output.contains(FAST_2_AFTER), selection.fast_mode_status);
            assert_eq!(output.contains(FAST_0_AFTER), selection.fast_mode_status);
            assert_eq!(output.contains(FAST_1_AFTER), selection.fast_mode_status);
            assert_eq!(output.contains("3577536076:Ten(!0)"), selection.simulators);
            assert_eq!(output.contains("4237076858:Ten(!0)"), selection.prewarm);
            assert_eq!(output.contains(MODE_AFTER), selection.computer_use);
            assert_eq!(output.contains(MODE_BEFORE), !selection.computer_use);
            assert_eq!(profile_mask(&profile_hash(mask).unwrap()), Some(mask));
            if mask == 1 {
                assert_eq!(
                    output,
                    format!("{BACKGROUND_AFTER};{MODE_AFTER};{FAST_0_BEFORE};{FAST_1_BEFORE};{FAST_2_BEFORE};{FAST_3_BEFORE};permissions();")
                );
            }
        }
        assert!(native_source("unknown()", Default::default()).is_err());
    }

    #[test]
    fn only_the_feature_map_changes() {
        let source = format!("prefix;{BEFORE};suffix");
        assert_eq!(
            patch_source(&source, false).unwrap(),
            format!("prefix;{AFTER};suffix")
        );
        assert!(patch_source(AFTER, false).is_err());
        assert!(patch_source(&format!("{BEFORE}{BEFORE}"), false).is_err());
        assert!(patch_source("function Fen(e){return e}", false).is_err());
    }

    #[test]
    fn background_patch_changes_only_the_config_and_platform_gate() {
        let source = format!("prefix;{BEFORE};middle;{MODE_BEFORE};permissions();suffix");
        assert_eq!(
            patch_source(&source, true).unwrap(),
            format!("prefix;{BACKGROUND_AFTER};middle;{MODE_AFTER};permissions();suffix")
        );
        assert!(patch_source(BEFORE, true).is_err());
        assert!(patch_source(&format!("{BEFORE}{MODE_BEFORE}{MODE_BEFORE}"), true).is_err());
        assert!(patch_source(&format!("{BACKGROUND_AFTER}{MODE_AFTER}"), true).is_err());
        assert!(patch_source(&format!("{BEFORE}{MODE_BEFORE}{MODE_AFTER}"), true).is_err());
    }

    #[test]
    fn block_integrity_catches_corruption_and_bad_sizes() {
        let expected = integrity(b"abcdef", 4).unwrap();
        assert_eq!(expected["blocks"].as_array().unwrap().len(), 2);
        assert!(verify_integrity(&expected, b"abcdef").is_ok());
        assert!(verify_integrity(&expected, b"abcdeg").is_err());
        assert!(integrity(b"x", 0).is_err());
        assert!(integrity(b"x", u64::MAX).is_err());
    }

    #[test]
    fn malformed_headers_are_rejected_before_allocation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.asar");
        for words in [
            [4u32, u32::MAX, 0, 0],
            [4, 8, 4, 9],
            [4, 8, 4, 0],
            [0, 8, 4, 0],
        ] {
            fs::write(
                &path,
                words
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            assert!(Archive::open(&path).is_err());
        }
    }

    #[test]
    fn missing_entries_do_not_create_header_nodes() {
        let mut header = json!({"files":{}});
        let original = header.clone();
        assert!(entry_mut(&mut header, TEST_ENTRY).is_err());
        assert_eq!(header, original);
    }

    #[test]
    fn entry_ranges_and_storage_types_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.asar");
        let source = b"contents";
        for (offset, unpacked, expected) in [
            (0u64, false, true),
            (1, false, false),
            (u64::MAX, false, false),
            (0, true, false),
        ] {
            let mut leaf = json!({"offset":offset.to_string(),"size":source.len(),"integrity":integrity(source, 4).unwrap()});
            if unpacked {
                leaf["unpacked"] = json!(true);
            }
            let mut header = leaf;
            for key in TEST_ENTRY.iter().rev() {
                header = json!({"files":{*key:header}});
            }
            let header = serde_json::to_vec(&header).unwrap();
            let padding = (4 - header.len() % 4) % 4;
            let pickle = 4 + header.len() as u32 + padding as u32;
            let mut bytes: Vec<_> = [4, pickle + 4, pickle, header.len() as u32]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect();
            bytes.extend_from_slice(&header);
            bytes.extend_from_slice(&[0; 3][..padding]);
            bytes.extend_from_slice(source);
            fs::write(&path, bytes).unwrap();
            assert_eq!(
                Archive::open(&path).unwrap().source(TEST_ENTRY).is_ok(),
                expected
            );
        }
    }

    fn fixture(entries: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.asar");
        let mut files = serde_json::Map::new();
        let mut payload = Vec::new();
        for (name, text) in entries {
            files.insert(
                (*name).into(),
                json!({"offset":payload.len().to_string(),"size":text.len(),
                "integrity":integrity(text.as_bytes(), 32).unwrap()}),
            );
            payload.extend_from_slice(text.as_bytes());
        }
        let header =
            serde_json::to_vec(&json!({"files":{".vite":{"files":{"build":{"files":files}}}}}))
                .unwrap();
        let padding = (4 - header.len() % 4) % 4;
        let pickle = 4 + header.len() as u32 + padding as u32;
        let mut bytes: Vec<_> = [4, pickle + 4, pickle, header.len() as u32]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect();
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&[0; 3][..padding]);
        bytes.extend_from_slice(&payload);
        fs::write(&path, bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn content_locator_follows_renamed_and_split_chunks() {
        for name in [
            "index.js",
            "index.chunk-renamed.js",
            "index2.chunk-other.js",
        ] {
            let (_dir, path) = fixture(&[("index.pre.js", "bootstrap();"), (name, BEFORE)]);
            let (found, source) = Archive::open(&path).unwrap().locate().unwrap();
            assert_eq!(found, name);
            assert_eq!(source, BEFORE.as_bytes());
        }
    }

    #[test]
    fn locator_recognizes_the_patched_state() {
        for source in [AFTER, BACKGROUND_AFTER] {
            let (_dir, path) = fixture(&[("index.chunk-renamed.js", source)]);
            assert_eq!(
                Archive::open(&path).unwrap().locate().unwrap().1,
                source.as_bytes()
            );
        }
    }

    #[test]
    fn ambiguous_or_missing_functions_are_rejected() {
        for entries in [
            vec![("index.js", BEFORE), ("index.chunk-two.js", BEFORE)],
            vec![("index.js", BEFORE), ("index.chunk-two.js", AFTER)],
            vec![
                ("index.js", AFTER),
                ("index.chunk-two.js", BACKGROUND_AFTER),
            ],
            vec![("index.js", "unrelated();"), ("other.js", BEFORE)],
        ] {
            let (_dir, path) = fixture(&entries);
            assert!(Archive::open(&path).unwrap().locate().is_err());
        }
        let duplicate = format!("{BEFORE}{BEFORE}");
        let (_dir, path) = fixture(&[("index.js", &duplicate)]);
        assert!(Archive::open(&path).unwrap().locate().is_err());
    }

    #[test]
    fn build_hashes_are_not_interchangeable() {
        for supported in BUILDS {
            assert_eq!(
                classify_hash(supported.original_hash, build(supported.version).unwrap()).unwrap(),
                PatchState::Pristine
            );
            assert_eq!(
                classify_hash(supported.patched_hash, supported).unwrap(),
                PatchState::Patched
            );
            if let Some(legacy_hash) = supported.legacy_hash {
                assert_eq!(
                    classify_hash(legacy_hash, supported).unwrap(),
                    PatchState::Legacy
                );
            }
        }
        assert!(classify_hash(BUILDS[0].original_hash, &BUILDS[1]).is_err());
        assert!(build("3.1623.5").is_err());
    }

    #[test]
    fn matching_shape_without_verified_content_cannot_be_patched() {
        let (_dir, path) = fixture(&[("index.chunk-renamed.js", BEFORE)]);
        let before = fs::read(&path).unwrap();
        assert!(inspect(&path, "3.1623.4").is_err());
        assert!(patch(&path, "3.1623.4").is_err());
        assert!(patch(&path, "3.1623.5").is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    #[ignore = "需要官方 Desktop 副本"]
    fn verified_macos_combinations_match_all_profiles() {
        let app = std::env::var_os("CLAUDEPP_OFFICIAL_FIXTURE").expect("缺少官方测试副本路径");
        let original = std::path::PathBuf::from(app).join("Contents/Resources/app.asar");
        let version = "3.6556.2";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.asar");
        for mask in 0..16u8 {
            fs::copy(&original, &path).unwrap();
            let selected = super::super::native::Selection { computer_use: mask & 1 != 0, simulators: mask & 2 != 0, prewarm: mask & 4 != 0, fast_mode_status: mask & 8 != 0, chinese: false };
            let header = patch_features(&path, version, selected).unwrap();
            assert_eq!(active_selection(&path, version).unwrap(), selected);
            assert_eq!(inspect(&path, version).unwrap().1, header);
            if mask != 0 { assert!(patch_features(&path, version, selected).is_err()); }
        }
        let mut archive = Archive::open(&path).unwrap();
        let recipe = super::super::macos_recipe::get(version).unwrap();
        let (name, mut source) = archive.locate_anchors(&[&recipe.main.anchor]).unwrap();
        source.push(b' ');
        replace_entry(&path, archive, &name, &source).unwrap();
        assert!(active_selection(&path, version).is_err());
        assert!(inspect(&path, version).is_err());
    }

    const RULES_MAIN: &str = concat!(
        r#"var A=1,B=2,C=3,D=4;Ky("1291166712",a);Ky("40173473",a);Zy("4272007212",!1);Zy("2784298314",!1);"#,
        r#"function fv(e){return{value:e,on:!!e,off:!e,source:"force","#,
        r#"experiment:null,experimentResult:null}}"#,
        "function pick(e){return e.selfHostedSessions?e.builtinBrowserEnabled?A:B:",
        "e.builtinBrowserEnabled?C:D}",
    );
    const RULES_PREWARM: &str = concat!(
        r#"async sparePrewarmSupport(){if(!t.x0("4237076858"))return{unsupported:"gate_off"};"#,
        r#"if(t.Ret().type!=="1p")return{unsupported:"third_party"};let e;}"#,
    );

    #[test]
    fn rules_patch_only_the_unique_entries_and_update_integrity() {
        let (_dir, path) = fixture(&[
            ("index.chunk-renamed.js", RULES_MAIN),
            ("index.pre.js", RULES_PREWARM),
            ("index.other.js", "unchanged"),
        ]);
        let platform = rules::Platform::Windows;
        let selection = super::super::native::Selection {
            computer_use: true,
            simulators: true,
            prewarm: true,
            ..Default::default()
        };
        assert!(rules_selection(&path, platform).unwrap().is_empty());
        let header = patch_with_rules(&path, platform, selection).unwrap();
        assert_eq!(rules_selection(&path, platform).unwrap(), selection);
        assert_eq!(header_hash(&path).unwrap(), header);
        let mut archive = Archive::open(&path).unwrap();
        assert_eq!(
            archive.source(&[".vite", "build", "index.other.js"]).unwrap(),
            b"unchanged"
        );
        let bytes = fs::read(&path).unwrap();
        assert!(patch_with_rules(&path, platform, selection).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn rules_refuse_ambiguous_archives_without_writing() {
        let (_dir, path) = fixture(&[
            ("index.one.js", RULES_MAIN),
            ("index.two.js", RULES_MAIN),
            ("index.pre.js", RULES_PREWARM),
        ]);
        let platform = rules::Platform::Windows;
        let bytes = fs::read(&path).unwrap();
        assert!(rules_selection(&path, platform).is_err());
        assert!(patch_with_rules(
            &path,
            platform,
            super::super::native::Selection {
                computer_use: true,
                ..Default::default()
            }
        )
        .is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    /// 主进程开关与独立预热模块必须同进同出，否则选了预热却没生效
    #[test]
    fn prewarm_module_and_switch_must_agree() {
        let (_dir, path) = fixture(&[
            ("index.chunk-renamed.js", RULES_MAIN),
            ("index.pre.js", RULES_PREWARM),
        ]);
        let platform = rules::Platform::Windows;
        patch_with_rules(
            &path,
            platform,
            super::super::native::Selection {
                computer_use: true,
                ..Default::default()
            },
        )
        .unwrap();
        let mut archive = Archive::open(&path).unwrap();
        let (name, source) = archive.locate_marker(PREWARM_MARKER, "原生预热").unwrap();
        let stripped = rules::patch_prewarm(&source).unwrap();
        replace_entry(&path, archive, &name, stripped.as_bytes()).unwrap();
        assert!(rules_selection(&path, platform).is_err());
    }
}
