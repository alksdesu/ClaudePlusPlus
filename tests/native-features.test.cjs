const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const ts = require('typescript');
const output = fs.mkdtempSync(path.join(os.tmpdir(), 'claudeplusplus-ui-'));
for (const name of ['text', 'desktop-update', 'native-features', 'shell']) {
  const source = fs.readFileSync(path.join(__dirname, '../src/ui', name + '.ts'), 'utf8');
  fs.writeFileSync(path.join(output, name + '.js'), ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 } }).outputText);
}
const ui = require(path.join(output, 'native-features.js'));
const { esc } = require(path.join(output, 'text.js'));
process.on('exit', () => fs.rmSync(output, { recursive: true, force: true }));
function fixture() {
  return { status: { installation: { running: false, canInstall: true, canRestore: true, requiresSigning: true, identities: [{ fingerprint: 'valid', name: 'Test' }], appPath: '/Applications/Claude.app' }, selection: ui.emptySelection(), supportedFeatures: ['computerUse', 'chinese', 'simulators', 'prewarm', 'fastModeStatus'] }, draft: { ...ui.emptySelection(), chinese: true }, identity: 'valid', group: 'all', search: '', scanning: false, busy: false, error: null, confirm: false, result: null };
}
test('batch application requires a real supported change and a signing identity', () => {
  const v = fixture();
  assert.equal(ui.canApply(v), true);
  for (const patch of [{ busy: true }, { scanning: true }, { identity: '' }, { status: null }]) assert.equal(ui.canApply({ ...v, ...patch }), false);
  assert.equal(ui.canApply({ ...v, draft: ui.emptySelection() }), false);
  v.status.installation.running = true;
  assert.equal(ui.canApply(v), false);
});
test('unverified platform selections cannot be applied', () => {
  const v = fixture();v.status.supportedFeatures = ['computerUse'];
  assert.equal(ui.canApply(v), false);
  assert.equal(ui.selectionSupported(v.draft, ['computerUse']), false);
});
test('Windows does not require or display signing controls', () => {
  const v = fixture();v.status.installation.requiresSigning = false;v.identity = '';
  assert.equal(ui.canApply(v), true);
  assert.equal(ui.renderFeatures(v).includes('native-signing-identity'), false);
});
test('removing the complete selection requires a restorable installation', () => {
  const v = fixture();v.status.selection = v.draft;v.draft = ui.emptySelection();v.identity = '';
  assert.equal(ui.canApply(v), true);
  v.status.installation.canRestore = false;
  assert.equal(ui.canApply(v), false);
});
test('the catalog never renders install toggles for missing native chains', () => {
  const v = fixture(), html = ui.renderFeatures(v);
  for (const feature of ui.features.filter(f => !f.key)) {
    assert.equal(html.includes('data-native-feature="' + feature.id + '"'), false);
    assert.equal(html.includes(feature.reason), true);
  }
});
test('search, native errors, and identity labels are escaped', () => {
  const v = fixture();v.search = '\" autofocus onfocus=\"bad';v.error = '<script>bad</script>';
  const html = ui.renderFeatures(v);
  assert.equal(html.includes('<script>'), false);
  assert.equal(html.includes('value="" autofocus'), false);
  assert.equal(esc('"<&'), '&quot;&lt;&amp;');
});
test('filters and pending-state presentation do not mutate the selection', () => {
  const v = fixture();v.group = 'interaction';const before = JSON.stringify(v);
  const html = ui.renderFeatures(v);
  assert.equal(html.includes('data-feature-id="simulators"'), true);
  assert.equal(html.includes('data-feature-id="chinese"'), false);
  assert.equal(JSON.stringify(v), before);
});

test('legacy native installation can upgrade without toggling features off', () => {
  const v = fixture();v.status.installation.state = 'upgradeAvailable';v.status.selection = { ...v.draft };
  assert.equal(ui.canApply(v), true);
});

test('missing signing capability fails closed instead of enabling installation', () => {
  const v = fixture();delete v.status.installation.requiresSigning;v.identity = '';
  assert.equal(ui.canApply(v), false);
  assert.equal(ui.renderFeatures(v).includes('native-signing-identity'), true);
});

test('Fast status is independently selectable and remains platform-gated', () => {
  const v = fixture();v.draft = { ...ui.emptySelection(), fastModeStatus: true };
  assert.equal(ui.canApply(v), true);
  assert.equal(ui.renderFeatures(v).includes('data-native-feature="fastModeStatus" checked'), true);
  v.status.supportedFeatures = ['computerUse'];
  assert.equal(ui.canApply(v), false);
});

const updates = require(path.join(output, 'desktop-update.js'));
test('only macOS renders automatic restore and reapply controls', () => {
  const base = { supported: false, enabled: false, phase: 'watching', message: '', desiredFeatures: [], pendingFeatures: [], error: null, busy: false };
  const windows = updates.renderDesktopUpdate(base, false);
  assert.ok(windows.includes('Windows 不需要'));
  assert.equal(windows.includes('data-action="desktop-update-toggle"'), false);
  const mac = updates.renderDesktopUpdate({ ...base, supported: true, enabled: true }, false);
  assert.ok(mac.includes('仅 macOS 需要'));
  assert.ok(mac.includes('data-action="desktop-update-toggle" checked'));
});
test('maintenance status and errors are escaped and busy operations cannot be toggled', () => {
  const html = updates.renderDesktopUpdate({ supported: true, enabled: true, phase: 'applying', message: '<script>bad</script>', desiredFeatures: [], pendingFeatures: ['<img>'], error: '<script>error</script>', busy: true }, false);
  assert.equal(html.includes('<script>'), false);
  assert.equal(html.includes('<img>'), false);
  assert.ok(html.includes('checked disabled'));
});

test('queued maintenance blocks competing patch installation but permits explicit restoration', () => {
  const v = fixture();
  v.status.update = { enabled: true, phase: 'awaitingUpdate' };
  assert.equal(ui.canApply(v), false);
  v.status.selection = v.draft;
  v.draft = ui.emptySelection();
  assert.equal(ui.canApply(v), true);
});
