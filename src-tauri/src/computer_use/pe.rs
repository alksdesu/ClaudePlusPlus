use std::fs;
use std::ops::Range;
use std::path::Path;

use anyhow::{ensure, Context, Result};

const FUSE_SENTINEL: &[u8] = b"dL7pKGdnNz796PbbjQWNKmHXBZaB9tsX";
const MAX_EXE: u64 = 512 * 1024 * 1024;

fn slice(bytes: &[u8], offset: usize, size: usize) -> Result<&[u8]> {
    let end = offset.checked_add(size).context("PE 范围溢出")?;
    bytes.get(offset..end).context("PE 文件被截断")
}

fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(slice(bytes, offset, 2)?.try_into()?))
}

fn u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(slice(bytes, offset, 4)?.try_into()?))
}

struct Pe<'a> {
    bytes: &'a [u8],
    sections: Vec<(u32, u32, u32)>,
    resources: usize,
    resource_size: usize,
}

impl<'a> Pe<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self> {
        ensure!(slice(bytes, 0, 2)? == b"MZ", "不是 PE 可执行文件");
        let pe = u32_at(bytes, 0x3c)? as usize;
        ensure!(slice(bytes, pe, 4)? == b"PE\0\0", "PE 签名错误");
        ensure!(u16_at(bytes, pe + 4)? == 0x8664, "仅适配 x64 Desktop");
        let count = u16_at(bytes, pe + 6)? as usize;
        ensure!((1..=96).contains(&count), "PE 节数量异常");
        let optional = pe + 24;
        let optional_size = u16_at(bytes, pe + 20)? as usize;
        ensure!(
            optional_size >= 136 && u16_at(bytes, optional)? == 0x20b,
            "需要 PE32+ 文件"
        );
        ensure!(u32_at(bytes, optional + 108)? >= 3, "PE 缺少资源目录");
        let rva = u32_at(bytes, optional + 128)?;
        let resource_size = u32_at(bytes, optional + 132)? as usize;
        ensure!(
            resource_size > 0 && resource_size <= 32 * 1024 * 1024,
            "PE 资源大小异常"
        );
        let mut sections = Vec::new();
        for i in 0..count {
            let at = optional + optional_size + i * 40;
            let address = u32_at(bytes, at + 12)?;
            let size = u32_at(bytes, at + 16)?;
            let raw = u32_at(bytes, at + 20)?;
            slice(bytes, raw as usize, size as usize)?;
            sections.push((address, size, raw));
        }
        let mut result = Self {
            bytes,
            sections,
            resources: 0,
            resource_size,
        };
        result.resources = result.offset(rva, resource_size)?;
        Ok(result)
    }

    fn offset(&self, rva: u32, size: usize) -> Result<usize> {
        let matches: Vec<_> = self
            .sections
            .iter()
            .filter_map(|&(start, len, raw)| {
                let relative = rva.checked_sub(start)?;
                ((relative as u64 + size as u64) <= len as u64)
                    .then_some(raw as usize + relative as usize)
            })
            .collect();
        ensure!(matches.len() == 1, "PE RVA 不唯一或越界");
        slice(self.bytes, matches[0], size)?;
        Ok(matches[0])
    }

    fn resource(&self, rel: usize, size: usize) -> Result<&[u8]> {
        ensure!(
            rel.checked_add(size)
                .is_some_and(|end| end <= self.resource_size),
            "PE 资源条目越界"
        );
        slice(self.bytes, self.resources + rel, size)
    }

    fn entries(&self, rel: usize) -> Result<Vec<(u32, u32)>> {
        let head = self.resource(rel, 16)?;
        let count = u16_at(head, 12)? as usize + u16_at(head, 14)? as usize;
        ensure!(count <= 4096, "PE 资源目录条目过多");
        let entries = self.resource(rel + 16, count * 8)?;
        (0..count)
            .map(|i| Ok((u32_at(entries, i * 8)?, u32_at(entries, i * 8 + 4)?)))
            .collect()
    }

    fn name(&self, id: u32) -> Result<Option<String>> {
        if id & 0x80000000 == 0 {
            return Ok(None);
        }
        let rel = (id & 0x7fffffff) as usize;
        let count = u16_at(self.resource(rel, 2)?, 0)? as usize;
        ensure!(count <= 128, "PE 资源名过长");
        let raw = self.resource(rel + 2, count * 2)?;
        let words: Vec<_> = raw
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        Ok(Some(String::from_utf16(&words)?))
    }

    fn child(&self, directory: usize, name: &str) -> Result<usize> {
        let mut found = Vec::new();
        for (id, target) in self.entries(directory)? {
            if self.name(id)?.as_deref() == Some(name) {
                ensure!(target & 0x80000000 != 0, "PE 命名资源不是目录");
                found.push((target & 0x7fffffff) as usize);
            }
        }
        ensure!(found.len() == 1, "PE 资源 {name} 未唯一命中");
        Ok(found[0])
    }

    fn integrity_range(&self) -> Result<Range<usize>> {
        let kind = self.child(0, "INTEGRITY")?;
        let name = self.child(kind, "ELECTRONASAR")?;
        let languages = self.entries(name)?;
        ensure!(languages.len() == 1, "PE 完整性资源语言不唯一");
        let (_, target) = languages[0];
        ensure!(target & 0x80000000 == 0, "PE 完整性资源不是数据");
        let data = self.resource(target as usize, 16)?;
        let size = u32_at(data, 4)? as usize;
        ensure!((1..=1024 * 1024).contains(&size), "PE 完整性数据大小异常");
        let offset = self.offset(u32_at(data, 0)?, size)?;
        Ok(offset..offset + size)
    }
}

fn header_hash(bytes: &[u8]) -> Result<(String, Range<usize>)> {
    let positions: Vec<_> = memchr::memmem::find_iter(bytes, FUSE_SENTINEL).collect();
    ensure!(positions.len() == 1, "Electron fuse 标记未唯一命中");
    let wire = slice(bytes, positions[0] + FUSE_SENTINEL.len(), 8)?;
    ensure!(
        wire[0] == 1 && wire[1] >= 6 && wire[6] == b'1' && wire[7] == b'1',
        "Electron ASAR 校验未开启"
    );
    let range = Pe::new(bytes)?.integrity_range()?;
    let records: Vec<serde_json::Value> = serde_json::from_slice(&bytes[range.clone()])?;
    ensure!(records.len() == 1, "ASAR 完整性记录不唯一");
    let record = &records[0];
    ensure!(
        record["file"].as_str() == Some("resources\\app.asar")
            && record["alg"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("sha256")),
        "ASAR 完整性记录格式不受支持"
    );
    let hash = record["value"].as_str().context("缺少 ASAR 头部哈希")?;
    ensure!(
        hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "ASAR 头部哈希格式错误"
    );
    let matches: Vec<_> =
        memchr::memmem::find_iter(&bytes[range.clone()], hash.as_bytes()).collect();
    ensure!(matches.len() == 1, "完整性哈希位置不唯一");
    let start = range.start + matches[0];
    Ok((hash.into(), start..start + 64))
}

fn read(path: &Path) -> Result<Vec<u8>> {
    ensure!(
        fs::metadata(path)?.len() <= MAX_EXE,
        "Desktop 可执行文件过大"
    );
    fs::read(path).context("读取 Desktop 可执行文件")
}

pub fn inspect(path: &Path) -> Result<String> {
    header_hash(&read(path)?).map(|(hash, _)| hash)
}

pub fn update(path: &Path, expected: &str, replacement: &str) -> Result<()> {
    ensure!(
        replacement.len() == 64 && replacement.bytes().all(|b| b.is_ascii_hexdigit()),
        "新的 ASAR 哈希格式错误"
    );
    let mut bytes = read(path)?;
    let (current, range) = header_hash(&bytes)?;
    ensure!(current == expected, "exe 中的 ASAR 基准已变化");
    bytes[range].copy_from_slice(replacement.as_bytes());
    ensure!(
        header_hash(&bytes)?.0 == replacement,
        "exe 完整性记录回读失败"
    );
    fs::write(path, bytes).context("更新 exe 的 ASAR 完整性记录")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        b[..2].copy_from_slice(b"MZ");
        put32(&mut b, 0x3c, 128);
        b[128..132].copy_from_slice(b"PE\0\0");
        put16(&mut b, 132, 0x8664);
        put16(&mut b, 134, 1);
        put16(&mut b, 148, 240);
        put16(&mut b, 152, 0x20b);
        put32(&mut b, 260, 16);
        put32(&mut b, 280, 0x1000);
        put32(&mut b, 284, 2048);
        put32(&mut b, 404, 0x1000);
        put32(&mut b, 408, 2048);
        put32(&mut b, 412, 512);
        for (rel, name, target) in [(0, 128, 32), (32, 160, 64)] {
            put16(&mut b, 512 + rel + 12, 1);
            put32(&mut b, 512 + rel + 16, 0x80000000 | name);
            put32(&mut b, 512 + rel + 20, 0x80000000 | target);
        }
        put16(&mut b, 512 + 64 + 14, 1);
        put32(&mut b, 512 + 80, 1033);
        put32(&mut b, 512 + 84, 96);
        for (rel, name) in [(128, "INTEGRITY"), (160, "ELECTRONASAR")] {
            put16(&mut b, 512 + rel, name.len() as u16);
            for (i, word) in name.encode_utf16().enumerate() {
                put16(&mut b, 512 + rel + 2 + i * 2, word);
            }
        }
        let data = format!(
            r#"[{{"file":"resources\\app.asar","alg":"SHA256","value":"{}"}}]"#,
            "a".repeat(64)
        );
        put32(&mut b, 608, 0x1100);
        put32(&mut b, 612, data.len() as u32);
        b[768..768 + data.len()].copy_from_slice(data.as_bytes());
        b[3000..3000 + FUSE_SENTINEL.len()].copy_from_slice(FUSE_SENTINEL);
        let at = 3000 + FUSE_SENTINEL.len();
        b[at..at + 8].copy_from_slice(&[1, 6, b'0', b'1', b'0', b'0', b'1', b'1']);
        b
    }

    fn put16(b: &mut [u8], at: usize, n: u16) {
        b[at..at + 2].copy_from_slice(&n.to_le_bytes());
    }
    fn put32(b: &mut [u8], at: usize, n: u32) {
        b[at..at + 4].copy_from_slice(&n.to_le_bytes());
    }

    #[test]
    fn updates_only_the_named_resource_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claude.exe");
        let bytes = fixture();
        fs::write(&path, &bytes).unwrap();
        let (_, range) = header_hash(&bytes).unwrap();
        update(&path, &"a".repeat(64), &"b".repeat(64)).unwrap();
        let after = fs::read(&path).unwrap();
        assert_eq!(&bytes[..range.start], &after[..range.start]);
        assert_eq!(&bytes[range.end..], &after[range.end..]);
        assert_eq!(inspect(&path).unwrap(), "b".repeat(64));
        assert!(update(&path, &"a".repeat(64), &"c".repeat(64)).is_err());
    }

    #[test]
    fn rejects_bad_ranges_resources_and_disabled_fuses() {
        for at in [0x3c, 280, 512 + 20, 512 + 84, 608, 612] {
            let mut bytes = fixture();
            put32(&mut bytes, at, u32::MAX);
            assert!(header_hash(&bytes).is_err());
        }
        let mut bytes = fixture();
        bytes[3000 + FUSE_SENTINEL.len() + 6] = b'0';
        assert!(header_hash(&bytes).is_err());
        for len in [0, 64, 200, 768] {
            assert!(header_hash(&fixture()[..len]).is_err());
        }
    }
}
