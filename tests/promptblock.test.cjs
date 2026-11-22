const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const ts = require('typescript');
const output = fs.mkdtempSync(path.join(os.tmpdir(), 'claudeplusplus-prompts-'));
for (const name of ['text', 'promptblock']) {
  const source = fs.readFileSync(path.join(__dirname, '../src/ui', name + '.ts'), 'utf8');
  fs.writeFileSync(path.join(output, name + '.js'), ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 } }).outputText);
}
const ui = require(path.join(output, 'promptblock.js'));
process.on('exit', () => fs.rmSync(output, { recursive: true, force: true }));
const install = (id, state) => ({ id, state, kind: id, exePath: '/tmp/claude', cliVersion: 'test', appliedAt: null, segmentHits: {}, note: null });

test('legacy macOS guards can upgrade but unknown layouts cannot', () => {
  const items = ['pristine', 'legacy', 'unsupported', 'damaged', 'patched'].map(s => install(s, s));
  assert.deepEqual(ui.applyTargets(items).map(i => i.id), ['pristine', 'legacy']);
});

test('both Desktop distributions block writes while Desktop is running', () => {
  for (const id of ['bundled', 'bundled-1p']) {
    const state = { busy: false, running: [{ running: true }], promptblock: { supported: true, installs: [install(id, 'pristine')] } };
    assert.match(ui.renderPromptBlock(state), /data-action="promptblock-apply" disabled/);
    state.running = [];
    assert.doesNotMatch(ui.renderPromptBlock(state), /data-action="promptblock-apply" disabled/);
  }
});

test('legacy restore labels and diagnostic text remain correct and escaped', () => {
  const item = { ...install('local-bin', 'legacy'), appliedAt: 'old', segmentHits: { heron_brook: 9, security_policy: 2 }, note: '<img onerror=bad>' };
  const html = ui.renderPromptBlock({ busy: false, running: [], promptblock: { supported: true, installs: [item] } });
  assert.match(html, /还原官方内容/);
  assert.doesNotMatch(html, /<img onerror/);
  assert.match(html, /&lt;img/);
  assert.doesNotMatch(html, /覆写 11 处/);
});

test('signing details are limited to macOS', () => {
  const state = { busy: false, running: [], promptblock: { supported: true, platform: 'windows', installs: [] } };
  assert.doesNotMatch(ui.renderPromptBlock(state), /重新签名/);
  state.promptblock.platform = 'macos';
  assert.match(ui.renderPromptBlock(state), /重新签名/);
});
