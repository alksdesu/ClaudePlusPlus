#!/usr/bin/env python3
"""屏蔽本机 Claude Code 的远端 heron 提示词，支持校验与原样还原。"""

import argparse
import contextlib
import ctypes
import fcntl
import hashlib
import json
import mmap
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import uuid


MARKER = b'/*claudepp:heron-guard:2*/'
SNAPSHOT = b'type:"prompt_snapshot",'
STAMPED_SNAPSHOT = b'type:"prompt_snapshot",cppHeronGuard:1,'
SNAPSHOT_READER = re.compile(
    rb'(function [\w$]+\(([\w$]+)\)\{let ([\w$]+)=\2\.findLast\(([\w$]+)\)\?\.attachment;)'
    rb'if\(\3===void 0(?:\|\|\3\.cppHeronGuard!==1)?\)return;'
)
TRAILER = b'\n---- Bun! ----\n'
GETTER = re.compile(
    rb'function (\w+)\(\)\{let (\w+)=(\w+)\(\)\?\.tengu_heron_brook;'
    rb'if\(typeof \2==="string"&&\2\.trim\(\)!==""\)'
    rb'return\{value:\2\.trim\(\),fromClientData:!0\};'
    rb'let (\w+)=(\w+)\("tengu_heron_brook",""\);'
    rb'if\(\4\.trim\(\)!==""\)return\{value:\4\.trim\(\),fromClientData:!1\};return null\}'
)
SECTION = re.compile(
    rb'\w+\("heron_brook",\(\)=>\w+\((?:\w+\(\))?\)\?\?\w+\(\w+,\w+\)\)'
)
STATE_ROOT = Path.home() / 'Library/Application Support/com.claudeplusplus.manager/prompt-guard'


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for block in iter(lambda: f.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def snapshot_reader(source):
    found = []
    for match in SNAPSHOT_READER.finditer(source):
        predicate = (rb'function ' + re.escape(match[4])
                     + rb'\(([\w$]+)\)\{return \1\.type==="attachment"&&\1\.attachment\.type==="prompt_snapshot"\}')
        if re.search(predicate, source):
            found.append(match)
    require(len(found) <= 1, '提示词快照读取入口不唯一')
    return found[0] if found else None


def segment(data):
    require(len(data) >= 32 and data[:4] == b'\xcf\xfa\xed\xfe', '需要 thin 64-bit Mach-O')
    count, total = struct.unpack_from('<II', data, 16)
    require(32 + total <= len(data), 'Mach-O 命令表越界')
    pos = 32
    found = []
    for _ in range(count):
        require(pos + 8 <= 32 + total, 'Mach-O 命令截断')
        command, size = struct.unpack_from('<II', data, pos)
        require(size >= 8 and pos + size <= 32 + total, 'Mach-O 命令长度异常')
        if command == 0x19:
            require(size >= 72, 'Mach-O segment 截断')
            sections = struct.unpack_from('<I', data, pos + 64)[0]
            require(72 + 80 * sections <= size, 'Mach-O section 表越界')
            for i in range(sections):
                at = pos + 72 + 80 * i
                if data[at:at + 16].rstrip(b'\0') == b'__bun':
                    length = struct.unpack_from('<Q', data, at + 40)[0]
                    offset = struct.unpack_from('<I', data, at + 48)[0]
                    require(offset + length <= len(data), 'Bun section 越界')
                    found.append((offset, length))
        pos += size
    require(len(found) == 1, '需要唯一的 Bun section')
    return found[0]


def inspect_bytes(data):
    offset, length = segment(data)
    end = offset + length - len(TRAILER)
    require(data[end:end + len(TRAILER)] == TRAILER, 'Bun trailer 不匹配')
    require(end - offset >= 40, 'Bun 元数据截断')
    byte_count, table, size, entry, argv, argc, flags = struct.unpack_from('<Q6I', data, end - 32)
    base = end - 32 - byte_count
    require(base == offset + 8, 'Bun 数据起点不匹配')
    require(struct.unpack_from('<Q', data, offset)[0] == length - 8, 'Bun section 长度记录不匹配')
    require(flags in (0x1fff, 0x3fff, 0x1fef, 0x3fef), 'Bun 布局未适配，拒绝修改')
    require(size % 52 == 0 and 0 < size // 52 < 100000, 'Bun 模块表异常')
    count = size // 52
    require(entry < count, 'Bun 入口越界')

    def bounded(at, n):
        require(0 <= at <= byte_count and 0 <= n <= byte_count - at, 'Bun 数据引用越界')
        return base + at

    bounded(argv, argc)
    table_at = bounded(table, size + count * 4)
    matches = []
    writers = []
    readers = []
    modules = []
    regions = []
    for i in range(count):
        record = table_at + i * 52
        fields = struct.unpack_from('<12I4B', data, record)
        for j in range(0, 12, 2):
            bounded(fields[j], fields[j + 1])
            if fields[j + 1]:
                regions.append((fields[j], fields[j] + fields[j + 1]))
        start, source_length = fields[2:4]
        source = data[base + start:base + start + source_length]
        reader = snapshot_reader(source) if fields[12:15] == (1, 1, 1) else None
        if GETTER.search(source) or MARKER in source or source.count(SNAPSHOT) == 3 or reader:
            require(fields[12:15] == (1, 1, 1), '提示词模块编码或类型未适配')
            name = data[base + fields[0]:base + fields[0] + fields[1]].decode()
            item = dict(index=i, record=record, start=base + start,
                        size=source_length, source=source, name=name,
                        hash_at=table_at + size + i * 4)
            modules.append(item)
            if source.count(SNAPSHOT) == 3:
                writers.append(item)
            if GETTER.search(source) or MARKER in source:
                matches.append(item)
            if reader:
                readers.append(item)
    require(len(matches) == 1, '需要唯一的 heron 字段消费模块')
    module = matches[0]
    require(len(writers) == 1, '提示词快照写入模块未适配')
    writer = writers[0]
    require(len(readers) == 1, '提示词快照读取模块未适配')
    reader = readers[0]
    snapshot_count = sum(item['source'].count(SNAPSHOT) for item in modules)
    require((snapshot_count, reader['source'].count(SNAPSHOT)) in ((4, 1), (3, 0)), '提示词快照写入数量已变化')
    at = table + size + count * 4
    builtins = struct.unpack_from('<I', data, bounded(at, 4))[0]
    at += 4
    bounded(at, builtins * 12 + 20)
    at += builtins * 12 + 8 + 4 + 8
    graph_record = bounded(at, 12)
    graph, graph_length, graph_count = struct.unpack_from('<III', data, graph_record)
    bounded(graph, graph_length)
    require(graph_count <= count, 'Bun 预链接模块数量异常')
    bounded(at + 12, graph_count * 4)
    module.update(graph_record=graph_record, graph=graph, graph_length=graph_length,
                  base=base, flags_at=end - 4, flags=flags, writer=writer, reader=reader, modules=modules)
    version = re.search(rb'// Version: (\d+\.\d+\.\d+)', module['source'])
    require(version is not None, '未找到 CLI 版本')
    module['version'] = version[1].decode()
    module['state'] = 'guarded' if MARKER in module['source'] else 'original'
    if module['state'] == 'original':
        require(flags & 16 and graph_length > len(writer['source']) + len(reader['source']) + 256, '缺少可复用的预链接区')
        require(all(hi <= graph or lo >= graph + graph_length for lo, hi in regions), '预链接区与模块数据重叠')
    if module['state'] == 'guarded':
        require(module['source'].count(MARKER) == 1 and not GETTER.search(module['source']), '补丁源码状态异常')
        registration = SECTION.findall(module['source'])
        if registration:
            require(len(registration) == 1 and b'{...' + registration[0] + b',cacheBreak:!0}' in module['source'], 'heron 缓存仍可复用')
        require(data[graph_record:graph_record + 8] == bytes(8), '旧预链接表仍启用')
        require(b'.cppHeronGuard!==1' in reader['source'], '旧快照仍可读取')
        require(sum(item['source'].count(STAMPED_SNAPSHOT) for item in modules) == snapshot_count, '新快照没有完整标记')
        for item in modules:
            require(data[item['record'] + 24:item['record'] + 40] == bytes(16), '旧字节码仍启用')
            require(data[item['hash_at']:item['hash_at'] + 4] == bytes(4), '旧源码哈希仍启用')
        require(not flags & 16, '源码布局标记未更新')
    return module


def patch_file(path):
    with path.open('r+b') as f, mmap.mmap(f.fileno(), 0) as data:
        info = inspect_bytes(data)
        require(info['state'] == 'original', '已经应用补丁')
        source = info['source']
        require(len(GETTER.findall(source)) == 1, '提示词结构不唯一')
        legacy_cache = SECTION.findall(source)
        if legacy_cache:
            require(len(legacy_cache) == 1 and re.search(rb'if\(!\w+\.cacheBreak&&', source), '未确认提示词缓存读取条件')
        else:
            require(source.count(b'"heron_brook",()=>{let ') == 1 and b'cacheBreak' not in source, 'heron 注册结构未适配')
        arena = info['base'] + info['graph']
        for item in info['modules']:
            source = item['source']
            if item['record'] == info['record']:
                source = GETTER.sub(lambda m: b'function ' + m[1] + b'(){return null;' + MARKER + b'}', source)
                if legacy_cache:
                    source = SECTION.sub(lambda m: b'{...' + m[0] + b',cacheBreak:!0}', source)
            match = snapshot_reader(source)
            if match:
                replacement = match[1] + b'if(' + match[3] + b'===void 0||' + match[3] + b'.cppHeronGuard!==1)return;'
                source = source[:match.start()] + replacement + source[match.end():]
            source = source.replace(SNAPSHOT, STAMPED_SNAPSHOT)
            if len(source) <= item['size']:
                data[item['start']:item['start'] + item['size']] = source.ljust(item['size'])
            else:
                require(arena + len(source) + 1 <= info['base'] + info['graph'] + info['graph_length'], '预链接空闲区不足')
                data[arena:arena + len(source) + 1] = source + b'\0'
                struct.pack_into('<II', data, item['record'] + 8, arena - info['base'], len(source))
                arena += len(source) + 1
            # Bun 的字节码与预链接记录会绕过修改后的源码。
            data[item['record'] + 24:item['record'] + 40] = bytes(16)
            data[item['hash_at']:item['hash_at'] + 4] = bytes(4)
        data[info['graph_record']:info['graph_record'] + 8] = bytes(8)
        struct.pack_into('<I', data, info['flags_at'], info['flags'] & ~16)
        data.flush()
        require(inspect_bytes(data)['state'] == 'guarded', '补丁回读失败')
    return {'version': info['version'], 'module': info['name']}


def inspect_file(path):
    with path.open('rb') as f, mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as data:
        info = inspect_bytes(data)
        return {k: info[k] for k in ('state', 'version', 'name')}


def run(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def verify_signature(unit):
    run('codesign', '--verify', '--deep', '--strict', str(unit))


def copy_unit(source, destination):
    if source.is_dir():
        shutil.copytree(source, destination, symlinks=True)
    else:
        shutil.copy2(source, destination)


def unit_for(binary):
    if binary.parent.name == 'MacOS' and binary.parent.parent.name == 'Contents':
        bundle = binary.parent.parent.parent
        if bundle.suffix == '.app':
            return bundle
    return binary


def swap_units(left, right):
    libc = ctypes.CDLL('/usr/lib/libSystem.B.dylib', use_errno=True)
    swap = libc.renameatx_np
    swap.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
    swap.restype = ctypes.c_int
    if swap(-2, os.fsencode(left), -2, os.fsencode(right), 2):
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error))


def save(path, value):
    temporary = path.with_suffix('.tmp')
    with temporary.open('w') as f:
        json.dump(value, f, indent=2)
        f.flush()
        os.fsync(f.fileno())
    os.replace(temporary, path)


@contextlib.contextmanager
def locked_state(binary, state_root):
    directory = state_root / hashlib.sha256(os.fsencode(binary)).hexdigest()[:24]
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    require(not directory.is_symlink(), '记录目录不能是符号链接')
    with (directory / 'lock').open('a') as f:
        fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
        yield directory


def apply(binary, directory):
    journal = directory / 'installation.json'
    info = inspect_file(binary)
    if journal.exists():
        old = json.loads(journal.read_text())
        require(old['binary'] == str(binary), '安装记录路径不匹配')
        if digest(binary) == old['modified'] and info['state'] == 'guarded':
            verify_signature(unit_for(binary))
            return {'state': 'guarded', 'version': info['version'], 'changed': False}
        require(old['phase'] == 'restored', '已有记录与当前文件不符；先核对更新或还原状态')
    require(info['state'] == 'original', '没有对应备份的已有补丁，拒绝覆盖')
    unit = unit_for(binary)
    verify_signature(unit)
    original = digest(binary)
    transaction = directory / str(uuid.uuid4())
    transaction.mkdir(mode=0o700)
    backup = transaction / unit.name
    copy_unit(unit, backup)
    relative = binary.relative_to(unit) if unit.is_dir() else None
    backed_binary = backup / relative if relative is not None else backup
    require(digest(backed_binary) == original, '备份哈希不匹配')
    verify_signature(backup)
    with tempfile.TemporaryDirectory(prefix='.claudepp-heron-', dir=unit.parent) as temporary:
        candidate = Path(temporary) / unit.name
        copy_unit(unit, candidate)
        executable = candidate / relative if relative is not None else candidate
        result = patch_file(executable)
        run('codesign', '--force', '--sign', '-', '--preserve-metadata=entitlements,flags,runtime', str(candidate))
        verify_signature(candidate)
        require(run(str(executable), '--version').strip() == info['version'] + ' (Claude Code)', '修改后 CLI 无法启动')
        modified = digest(executable)
        record = dict(schema=1, phase='prepared', binary=str(binary), unit=str(unit), backup=str(backup),
                      relative=str(relative) if relative is not None else None, original=original, modified=modified, **result)
        save(journal, record)
        require(digest(binary) == original, '安装过程中 CLI 被外部更新')
        swap_units(unit, candidate)
        try:
            require(digest(binary) == modified, '安装结果不匹配')
            verify_signature(unit)
        except Exception:
            swap_units(unit, candidate)
            record['phase'] = 'restored'
            save(journal, record)
            raise
        record['phase'] = 'installed'
        save(journal, record)
    return {'state': 'guarded', 'changed': True, 'backup': str(backup), **result}


def restore(binary, directory):
    journal = directory / 'installation.json'
    record = json.loads(journal.read_text())
    require(record['schema'] == 1 and record['binary'] == str(binary), '安装记录不匹配')
    current = digest(binary)
    if current == record['original']:
        record['phase'] = 'restored'
        save(journal, record)
        return {'state': 'original', 'changed': False}
    require(current == record['modified'], 'CLI 被外部更新，拒绝覆盖')
    backup = Path(record['backup'])
    require(backup.resolve().is_relative_to(directory.resolve()), '备份不在记录目录内')
    backed_binary = backup / record['relative'] if record['relative'] is not None else backup
    require(digest(backed_binary) == record['original'], '原始备份损坏')
    verify_signature(backup)
    unit = unit_for(binary)
    require(str(unit) == record['unit'], '应用布局已改变')
    with tempfile.TemporaryDirectory(prefix='.claudepp-heron-', dir=unit.parent) as temporary:
        candidate = Path(temporary) / unit.name
        copy_unit(backup, candidate)
        require(digest(binary) == current, '还原过程中 CLI 被外部更新')
        swap_units(unit, candidate)
        try:
            verify_signature(unit)
            require(digest(binary) == record['original'], '还原结果不匹配')
        except Exception:
            swap_units(unit, candidate)
            raise
        record['phase'] = 'restored'
        save(journal, record)
    return {'state': 'original', 'changed': True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['status', 'apply', 'restore'])
    parser.add_argument('binary', type=Path)
    parser.add_argument('--state-dir', type=Path, default=STATE_ROOT)
    args = parser.parse_args()
    require(sys.platform == 'darwin', '当前工具仅支持 macOS')
    binary = args.binary.resolve(strict=True)
    require(binary.is_file(), '需要可执行文件路径')
    if args.action == 'status':
        result = inspect_file(binary)
        verify_signature(unit_for(binary))
    else:
        with locked_state(binary, args.state_dir.resolve()) as directory:
            result = (apply if args.action == 'apply' else restore)(binary, directory)
    print(json.dumps(result, ensure_ascii=False))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f'错误：{error}', file=sys.stderr)
        sys.exit(1)
