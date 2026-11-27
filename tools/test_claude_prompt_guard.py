import json
import os
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = None
MARKER = 'CPP_HERON_PROBE_REMOTE_TEXT_7810'


def run(binary, label, source, model="claude-opus-5-5", profile=None, resume=None, persist=False, entrypoint="cli", settings=None, extra_env=None):
    import time
    started = time.monotonic()
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            self.send_response(404)
            self.end_headers()

        def do_POST(self):
            data = json.loads(self.rfile.read(int(self.headers['content-length'])))
            if '/messages/count_tokens' in self.path:
                body = {'input_tokens': 1}
            else:
                system = json.dumps(data.get('system'), ensure_ascii=False)
                requests.append({'path': self.path, 'markerPresent': MARKER in system, 'haikuGuidance': 'The reasoning effort setting changes how much you think before you act' in system, 'systemChars': len(system), 'model': data.get('model'), 'speed': data.get('speed')})
                body = {'id': 'msg_cpp_probe', 'type': 'message', 'role': 'assistant', 'model': data.get('model', 'claude-opus-5-5'), 'content': [{'type': 'text', 'text': 'OK'}], 'stop_reason': 'end_turn', 'stop_sequence': None, 'usage': {'input_tokens': 1, 'output_tokens': 1}}
            self.send_response(200)
            if data.get('stream'):
                self.send_header('Content-Type', 'text/event-stream')
                self.end_headers()
                events = [('message_start', {'type': 'message_start', 'message': {**body, 'content': [], 'stop_reason': None}}), ('content_block_start', {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'text', 'text': ''}}), ('content_block_delta', {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'text_delta', 'text': 'OK'}}), ('content_block_stop', {'type': 'content_block_stop', 'index': 0}), ('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': 'end_turn', 'stop_sequence': None}, 'usage': {'output_tokens': 1}}), ('message_stop', {'type': 'message_stop'})]
                for event, payload in events:
                    self.wfile.write(('event: ' + event + '\ndata: ' + json.dumps(payload) + '\n\n').encode())
            else:
                self.send_header('Content-Type', 'application/json')
                self.end_headers()
                self.wfile.write(json.dumps(body).encode())

    server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    config = ROOT / ('profile-' + (profile or label))
    config.mkdir(exist_ok=True)
    value = json.loads((config / '.claude.json').read_text()) if (config / '.claude.json').exists() else {'hasCompletedOnboarding': True, 'theme': 'dark', 'clientDataCache': {}, 'cachedGrowthBookFeatures': {}}
    for kind in ['clientDataCache', 'cachedGrowthBookFeatures']:
        value.setdefault(kind, {}).pop('tengu_heron_brook', None)
    if source != 'none':
        value['clientDataCache' if source == 'client_data' else 'cachedGrowthBookFeatures']['tengu_heron_brook'] = MARKER
    (config / '.claude.json').write_text(json.dumps(value))
    (config / 'settings.json').write_text('{}')
    env = {k: v for k, v in os.environ.items() if not k.startswith(('ANTHROPIC_', 'CLAUDE_', 'DISABLE_', 'BUN_', 'OTEL_', 'HTTP_PROXY', 'HTTPS_PROXY', 'ALL_PROXY'))}
    env.update({'CLAUDE_CONFIG_DIR': str(config), 'CLAUDE_SECURESTORAGE_CONFIG_DIR': str(config), 'ANTHROPIC_API_KEY': 'sk-ant-api03-not-a-real-key-CPP-probe', 'ANTHROPIC_BASE_URL': f'http://127.0.0.1:{server.server_port}', 'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'CLAUDE_CODE_GB_DISK_CACHE_WHEN_TELEMETRY_OFF': '1', 'CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL': '1', 'CLAUDE_CODE_DISABLE_AUTO_MEMORY': '1', 'CLAUDE_CODE_ENTRYPOINT': entrypoint})
    command = [str(binary), '-p', 'Reply OK.', '--output-format', 'json', '--model', model, '--max-turns', '1', '--setting-sources', 'user', '--strict-mcp-config', '--mcp-config', '{"mcpServers":{}}', '--no-session-persistence']
    if extra_env:
        env.update(extra_env)
    if settings is not None:
        command += ['--settings', json.dumps(settings)]
    if persist:
        command.remove('--no-session-persistence')
    if resume:
        command += ['--resume', resume]
    try:
        p = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True, timeout=70)
        parsed = json.loads(p.stdout) if p.returncode == 0 and p.stdout.strip().startswith('{') else {}
        result = {'label': label, 'source': source, 'exit': p.returncode, 'requests': requests, 'session_id': parsed.get('session_id'), 'result': parsed.get('result'), 'stderr': p.stderr[-300:], 'elapsed': round(time.monotonic() - started, 3)}
    except subprocess.TimeoutExpired as e:
        result = {'label': label, 'source': source, 'timeout': True, 'requests': requests, 'stdoutTail': str(e.stdout)[-1500:], 'stderrTail': str(e.stderr)[-1500:]}
    finally:
        server.shutdown()
        server.server_close()
    (ROOT / (label + '.json')).write_text(json.dumps(result, indent=2))
    return result


def check(result, marker, guidance=False):
    assert result.get("exit") == 0 and result["requests"], result
    assert all(request["markerPresent"] == marker for request in result["requests"]), result
    if guidance:
        assert result["requests"][-1]["haikuGuidance"], result


def main():
    import argparse
    import tempfile
    import claude_prompt_guard as guard

    global ROOT
    parser = argparse.ArgumentParser(description="在隔离目录和本机模拟接口验收 heron 补丁")
    parser.add_argument("--binary", action="append", required=True, type=Path)
    parser.add_argument("--report", type=Path)
    options = parser.parse_args()
    results = []
    checks = []
    with tempfile.TemporaryDirectory(prefix="claudepp-heron-test-") as temporary:
        ROOT = Path(temporary)
        for index, fixture in enumerate(options.binary):
            fixture = fixture.resolve(strict=True)
            assert guard.inspect_file(fixture)["state"] == "original"
            original_unit = guard.unit_for(fixture)
            folder = ROOT / str(index)
            folder.mkdir()
            candidate_unit = folder / original_unit.name
            guard.copy_unit(original_unit, candidate_unit)
            binary = candidate_unit / fixture.relative_to(original_unit) if original_unit.is_dir() else candidate_unit
            before = guard.digest(binary)
            entrypoint = "claude-desktop-3p" if original_unit.is_dir() else "cli"
            with guard.locked_state(binary, ROOT / "state") as state:
                guard.apply(binary, state)
                assert guard.apply(binary, state)["changed"] is False
                checks.append({"fixture": index, "check": "idempotent_apply", "passed": True})
                for source in ["client_data", "growthbook"]:
                    for name, executable, expected in [("original", fixture, True), ("guarded", binary, False)]:
                        item = run(executable, f"{index}-{name}-{source}", source, entrypoint=entrypoint)
                        check(item, expected)
                        results.append(item)
                for name, executable in [("original", fixture), ("guarded", binary)]:
                    item = run(executable, f"{index}-{name}-haiku", "none", model="claude-haiku-5-5", entrypoint=entrypoint)
                    check(item, False, guidance=True)
                    results.append(item)
                profile = f"{index}-resume"
                first = run(fixture, f"{index}-persist", "client_data", profile=profile, persist=True, entrypoint=entrypoint)
                check(first, True)
                assert first["session_id"]
                results.append(first)
                for attempt in range(2):
                    item = run(binary, f"{index}-resume-{attempt}", "client_data", profile=profile, resume=first["session_id"], persist=True, entrypoint=entrypoint)
                    check(item, False)
                    results.append(item)
                snapshots = []
                for path in (ROOT / ("profile-" + profile)).rglob("*.jsonl"):
                    for line in path.read_text().splitlines():
                        item = json.loads(line)
                        if item.get("type") == "attachment" and item["attachment"].get("type") == "prompt_snapshot":
                            snapshots.append(item["attachment"])
                assert snapshots and snapshots[-1].get("cppHeronGuard") == 1
                checks.append({"fixture": index, "check": "snapshot_stamp_persisted", "passed": True})
                guard.restore(binary, state)
                assert guard.digest(binary) == before
                guard.verify_signature(candidate_unit)
                assert guard.restore(binary, state)["changed"] is False
                checks.append({"fixture": index, "check": "exact_restore", "passed": True})
                restored = run(binary, f"{index}-restored-resume", "client_data", profile=profile, resume=first["session_id"], persist=True, entrypoint=entrypoint)
                check(restored, False)
                results.append(restored)
                guard.apply(binary, state)
                with binary.open("ab") as file:
                    file.write(b"external-change")
                try:
                    guard.restore(binary, state)
                except ValueError:
                    checks.append({"fixture": index, "check": "external_change_refused", "passed": True})
                else:
                    raise AssertionError("restore accepted a changed executable")
        for invalid in [b"", b"not a binary", bytes.fromhex("cffaedfe") + bytes(28)]:
            try:
                guard.inspect_bytes(invalid)
            except ValueError:
                continue
            raise AssertionError("accepted malformed binary")
        checks.append({"check": "malformed_binaries_refused", "passed": True})
    report = {"requests": results, "checks": checks, "passed": True}
    if options.report:
        options.report.write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print(f"PASS: {len(results)} runtime cases, {len(checks)} transaction/validation checks")


if __name__ == "__main__":
    main()
