use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};

const SENTINEL: &[u8; 32] = b"AGbevlPCksUGKNL8TSn7wGmJEuJsXb2A";
const SLOT_SIZE: usize = 66;

fn digest(info: &plist::Value) -> Result<[u8; 32]> {
    let entries = info
        .as_dictionary()
        .and_then(|d| d.get("ElectronAsarIntegrity"))
        .and_then(plist::Value::as_dictionary)
        .context("缺少 ASAR 完整性表")?;
    let mut keys: Vec<_> = entries.keys().collect();
    keys.sort_by_key(|key| key.encode_utf16().collect::<Vec<_>>());
    let mut hash = Sha256::new();
    for key in keys {
        let value = entries[key]
            .as_dictionary()
            .context("ASAR 完整性条目无效")?;
        hash.update(key.as_bytes());
        for field in ["algorithm", "hash"] {
            hash.update(
                value
                    .get(field)
                    .and_then(plist::Value::as_string)
                    .context("ASAR 完整性字段无效")?
                    .as_bytes(),
            );
        }
    }
    Ok(hash.finalize().into())
}

fn bytes(data: &[u8], offset: usize, size: usize) -> Result<&[u8]> {
    data.get(offset..offset.checked_add(size).context("Mach-O 范围溢出")?)
        .context("Mach-O 文件被截断")
}

fn u32_at(data: &[u8], offset: usize, big: bool) -> Result<u32> {
    let raw = bytes(data, offset, 4)?.try_into()?;
    Ok(if big {
        u32::from_be_bytes(raw)
    } else {
        u32::from_le_bytes(raw)
    })
}

fn u64_at(data: &[u8], offset: usize, big: bool) -> Result<u64> {
    let raw = bytes(data, offset, 8)?.try_into()?;
    Ok(if big {
        u64::from_be_bytes(raw)
    } else {
        u64::from_le_bytes(raw)
    })
}

fn name(data: &[u8]) -> &[u8] {
    &data[..data.iter().position(|b| *b == 0).unwrap_or(data.len())]
}

fn thin_slot(data: &[u8]) -> Result<Option<usize>> {
    ensure!(
        u32_at(data, 0, false)? == 0xfeedfacf,
        "不支持的 Mach-O 架构格式"
    );
    let count = u32_at(data, 16, false)? as usize;
    let end = 32usize
        .checked_add(u32_at(data, 20, false)? as usize)
        .context("Mach-O 命令范围溢出")?;
    ensure!(
        end <= data.len() && count <= (end - 32) / 8,
        "Mach-O 命令表无效"
    );
    let mut at = 32;
    let mut slot = None;
    for _ in 0..count {
        let cmd = u32_at(data, at, false)?;
        let size = u32_at(data, at + 4, false)? as usize;
        ensure!(size >= 8 && size <= end - at, "Mach-O 命令长度无效");
        if cmd == 0x19 {
            ensure!(size >= 72, "Mach-O 段被截断");
            let sections = u32_at(data, at + 64, false)? as usize;
            ensure!(sections <= (size - 72) / 80, "Mach-O 节表被截断");
            for i in 0..sections {
                let section = at + 72 + i * 80;
                if name(bytes(data, section, 16)?) != b"__asar_integrity" {
                    continue;
                }
                ensure!(
                    slot.is_none()
                        && name(bytes(data, section + 16, 16)?) == b"__DATA_CONST"
                        && name(bytes(data, at + 8, 16)?) == b"__DATA_CONST",
                    "ASAR 摘要节不唯一或位置异常"
                );
                ensure!(
                    u64_at(data, section + 40, false)? == SLOT_SIZE as u64,
                    "ASAR 摘要节长度变化"
                );
                let offset = u32_at(data, section + 48, false)? as usize;
                ensure!(offset >= end, "ASAR 摘要节与命令表重叠");
                let value = bytes(data, offset, SLOT_SIZE)?;
                ensure!(&value[..32] == SENTINEL, "ASAR 摘要节标记不匹配");
                slot = Some(offset);
            }
        }
        at += size;
    }
    ensure!(at == end, "Mach-O 命令表长度不一致");
    Ok(slot)
}

fn slots(data: &[u8]) -> Result<Vec<usize>> {
    let magic = u32_at(data, 0, true)?;
    let (big, wide) = match magic {
        0xcafebabe => (true, false),
        0xcafebabf => (true, true),
        0xbebafeca => (false, false),
        0xbfbafeca => (false, true),
        _ => return Ok(thin_slot(data)?.into_iter().collect()),
    };
    let count = u32_at(data, 4, big)? as usize;
    ensure!(count > 0 && count <= 32, "Mach-O 架构数量异常");
    let stride = if wide { 32 } else { 20 };
    let table_end = 8 + count * stride;
    bytes(data, 0, table_end)?;
    let mut ranges = Vec::new();
    let mut result = Vec::new();
    for i in 0..count {
        let at = 8 + i * stride;
        let (offset, size) = if wide {
            (u64_at(data, at + 8, big)?, u64_at(data, at + 16, big)?)
        } else {
            (
                u64::from(u32_at(data, at + 8, big)?),
                u64::from(u32_at(data, at + 12, big)?),
            )
        };
        let offset = usize::try_from(offset)?;
        let size = usize::try_from(size)?;
        let slice = bytes(data, offset, size)?;
        ensure!(
            offset >= table_end
                && ranges
                    .iter()
                    .all(|&(start, end)| offset + size <= start || offset >= end),
            "Mach-O 架构范围重叠"
        );
        ranges.push((offset, offset + size));
        if let Some(slot) = thin_slot(slice)? {
            result.push(offset + slot);
        }
    }
    Ok(result)
}

fn validate(data: &[u8], expected: &[u8; 32]) -> Result<Vec<usize>> {
    let mut active = Vec::new();
    for offset in slots(data)? {
        match (data[offset + 32], data[offset + 33]) {
            (0, 0) => {}
            (1, 1) => {
                ensure!(
                    &data[offset + 34..offset + SLOT_SIZE] == expected,
                    "Electron 框架与 ASAR 完整性表不一致"
                );
                active.push(offset + 34);
            }
            _ => anyhow::bail!("未知 Electron ASAR 摘要格式"),
        }
    }
    Ok(active)
}

fn framework(app: &Path) -> Result<PathBuf> {
    let path = app
        .join("Contents/Frameworks/Electron Framework.framework/Electron Framework")
        .canonicalize()?;
    ensure!(
        path.starts_with(app.canonicalize()?) && fs::metadata(&path)?.len() <= 1024 * 1024 * 1024,
        "Electron 框架路径或大小异常"
    );
    Ok(path)
}

pub(super) fn verify(app: &Path, info: &plist::Value) -> Result<()> {
    validate(&fs::read(framework(app)?)?, &digest(info)?)?;
    Ok(())
}

pub(super) fn update(app: &Path, before: &plist::Value, after: &plist::Value) -> Result<()> {
    let path = framework(app)?;
    let mut data = fs::read(&path)?;
    let active = validate(&data, &digest(before)?)?;
    if active.is_empty() {
        return Ok(());
    }
    let expected = digest(after)?;
    for offset in active {
        data[offset..offset + 32].copy_from_slice(&expected);
    }
    validate(&data, &expected)?;
    fs::write(path, data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thin() -> Vec<u8> {
        let mut data = vec![0; 256 + SLOT_SIZE];
        for (at, value) in [
            (0, 0xfeedfacfu32),
            (16, 1),
            (20, 152),
            (32, 0x19),
            (36, 152),
            (96, 1),
            (152, 256),
        ] {
            data[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        data[40..52].copy_from_slice(b"__DATA_CONST");
        data[104..120].copy_from_slice(b"__asar_integrity");
        data[120..132].copy_from_slice(b"__DATA_CONST");
        data[144..152].copy_from_slice(&(SLOT_SIZE as u64).to_le_bytes());
        data[256..288].copy_from_slice(SENTINEL);
        data[288..290].copy_from_slice(&[1, 1]);
        data
    }

    #[test]
    fn dictionary_digest_orders_paths_and_includes_algorithms() {
        let value = plist::Value::from_reader_xml(std::io::Cursor::new(br#"<?xml version="1.0"?><plist version="1.0"><dict><key>ElectronAsarIntegrity</key><dict><key>b</key><dict><key>hash</key><string>2</string><key>algorithm</key><string>SHA256</string></dict><key>a</key><dict><key>algorithm</key><string>SHA256</string><key>hash</key><string>1</string></dict></dict></dict></plist>"#)).unwrap();
        assert_eq!(
            digest(&value).unwrap(),
            <[u8; 32]>::from(Sha256::digest(b"aSHA2561bSHA2562"))
        );
    }

    #[test]
    fn digest_validation_rejects_corruption_and_keeps_flags() {
        let mut data = thin();
        assert_eq!(validate(&data, &[0; 32]).unwrap(), vec![290]);
        assert!(validate(&data, &[1; 32]).is_err());
        data[289] = 2;
        assert!(validate(&data, &[0; 32]).is_err());
        data[289] = 1;
        for offset in validate(&data, &[0; 32]).unwrap() {
            data[offset..offset + 32].fill(7);
        }
        assert!(validate(&data, &[7; 32]).is_ok());
        assert_eq!(&data[288..290], &[1, 1]);
        assert!(slots(&data[..150]).is_err());
    }

    #[test]
    fn universal_framework_validates_every_architecture() {
        let slice = thin();
        let mut data = vec![0; 64];
        for (at, value) in [
            (0, 0xcafebabeu32),
            (4, 2),
            (16, 64),
            (20, slice.len() as u32),
            (36, 64 + slice.len() as u32),
            (40, slice.len() as u32),
        ] {
            data[at..at + 4].copy_from_slice(&value.to_be_bytes());
        }
        data.extend_from_slice(&slice);
        data.extend_from_slice(&slice);
        assert_eq!(validate(&data, &[0; 32]).unwrap().len(), 2);
        data[64 + slice.len() + 290] = 1;
        assert!(validate(&data, &[0; 32]).is_err());
    }
}
