const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const ts = require('typescript');

const source = fs.readFileSync(path.join(__dirname, '../src-tauri/src/computer_use/fast_status.rs'), 'utf8');
const replacement = source.match(/const AFTER: &str = r#"([\s\S]*?)"#;/)[1];
const expression = replacement.slice('rt='.length);
const display = (state, options = {}) => vm.runInNewContext(`(${expression})`, {
  _e: options.picked ?? null, E: options.remote ?? false, O: state,
  Se: options.credits ?? false, w: !options.draft && !options.remote,
  $e: options.agent, u: {}, t: options.draft ? undefined : {}, re: false,
  ie: options.preference ?? null, ne: options.config ?? false,
});

test('the original native selector receives the live local Fast state', () => {
  assert.equal(display('on'), true);
  assert.equal(display('off', { preference: true }), false);
  assert.equal(display('cooldown'), true);
  assert.equal(display('on', { credits: true }), false);
  assert.equal(display('on', { picked: false }), false);
  assert.equal(display('off', { picked: true }), true);
});

test('drafts, unknown states and remote sessions retain native behavior', () => {
  assert.equal(display('on', { draft: true }), false);
  assert.equal(display('on', { draft: true, preference: true }), true);
  assert.equal(display('on', { remote: true }), true);
  assert.equal(display('off', { remote: true, preference: true }), false);
  assert.equal(display(undefined, { preference: true }), true);
  assert.equal(display('unknown'), false);
});

test('the native widget is preserved; the status-only experiment is upgradeable', () => {
  assert.ok(source.includes('hash(status.as_bytes()) == STATUS_ORIGINAL'));
  assert.equal(source.includes('fs::write(visible_path'), false);
  assert.ok(source.includes('LEGACY'));
});

const uiSource = fs.readFileSync(path.join(__dirname, '../src/ui/fastmode.ts'), 'utf8');
const compiled = ts.transpileModule(uiSource, { compilerOptions: { module: ts.ModuleKind.CommonJS } }).outputText;
const context = { exports: {} };
vm.runInNewContext(compiled, context);
const label = (state, native) => context.exports.rendererPresentation({ state, native }).label;

test('macOS distinguishes signed status installation from unsupported platforms', () => {
  assert.equal(label('pristine', true), '可安装 · 需重签');
  assert.equal(label('patched', true), '原生开关已安装');
  assert.equal(label('mismatch', true), '锚点失配');
  assert.equal(label('unsupported', true), '当前未适配');
  assert.equal(label('patched', false), '已 patch');
});

const mainSource = fs.readFileSync(path.join(__dirname, '../src/main.ts'), 'utf8');
const mainAst = ts.createSourceFile('main.ts', mainSource, ts.ScriptTarget.Latest, true);
const refreshFunctions = mainAst.statements.filter(n => ts.isFunctionDeclaration(n) && ['refreshNative', 'refreshFastMode'].includes(n.name?.text)).map(n => n.getText(mainAst)).join('\n');
const refreshCode = ts.transpileModule(refreshFunctions + '\nglobalThis.refreshUnderTest = refreshNative;', { compilerOptions: { target: ts.ScriptTarget.ES2022 } }).outputText;
test('native rescans also refresh Fast status after install, removal and errors', async () => {
  const state = { nativeScanning: false, nativeDirty: false, signingIdentity: '', fastmode: { renderer: { state: 'pristine' } } };
  let rendererState = 'patched', failNative = false, failFast = false;
  const context = { state, render() {}, invoke: async command => {
    if (command === 'fastmode_status') {
      if (failFast) throw Error('unreadable');
      return { renderer: { state: rendererState, native: true } };
    }
    if (failNative) throw Error('native scan failed');
    return { selection: { fastModeStatus: rendererState === 'patched' }, installation: { identities: [] } };
  } };
  vm.runInNewContext(refreshCode, context);
  await context.refreshUnderTest();
  assert.equal(state.fastmode.renderer.state, 'patched');
  rendererState = 'pristine';
  await context.refreshUnderTest();
  assert.equal(state.fastmode.renderer.state, 'pristine');
  failNative = true;
  await context.refreshUnderTest();
  assert.equal(state.native, null);
  assert.equal(state.fastmode.renderer.state, 'pristine');
  failFast = true;
  await context.refreshUnderTest();
  assert.equal(state.fastmode, null);
  assert.equal(state.nativeScanning, false);
});

const archiveSource = fs.readFileSync(path.join(__dirname, '../src-tauri/src/computer_use/asar.rs'), 'utf8');
const nativeAnchor = (number, side) => archiveSource.match(new RegExp(`const FAST_${number}_${side}: &str = r#"([\\s\\S]*?)"#;`))[1];
test('the complete 3p catalog pipeline preserves the original native Fast definition', () => {
  const fast = { type: 'toggle', name: 'Fast mode', options: [{ id: 'fast', name: 'Enable fast mode', selector_label: 'Fast' }, { id: 'off', name: 'Off' }] };
  const input = { id: 'claude-opus-5-5', name: 'Opus 5.5', fast_mode: fast, thinking: { type: 'effort' } };
  const parse = side => vm.runInNewContext(`(() => { let n={};${nativeAnchor(0, side)}if(e[t]&&typeof e[t]==='object')n[t]=e[t];return n;})()`, { e: input });
  const project = (t, side) => vm.runInNewContext(`(() => { let n={};${nativeAnchor(3, side)};})()`, { t, ww: x => x && typeof x === 'object' ? x : undefined, o9t: x => x });
  const emit = (s, t, side) => vm.runInNewContext(`(() => { let ${nativeAnchor(1, side)}unused:0};return g;})()`, { s, t, i: input, f: input.name, h: '', e: {}, Xw: () => input.thinking });
  assert.equal(emit(project(parse('BEFORE'), 'BEFORE'), 'code', 'BEFORE').fast_mode, undefined);
  assert.equal(emit(project(parse('AFTER'), 'BEFORE'), 'code', 'AFTER').fast_mode, undefined);
  const projected = project(parse('AFTER'), 'AFTER');
  assert.deepEqual(JSON.parse(JSON.stringify(emit(projected, 'code', 'AFTER').fast_mode)), fast);
  assert.equal(emit(projected, 'chat', 'AFTER').fast_mode, undefined);
});

test('native Fast availability keeps code policy and local-spawn restrictions', () => {
  const expression = nativeAnchor(2, 'AFTER').split(':').slice(1).join(':');
  const availability = (enabled, local) => vm.runInNewContext(expression, { n: enabled, iw: () => ({ allowsLocalSpawn: local }), t: allowed => allowed ? 'available' : 'blocked_by_org_admin' });
  assert.equal(availability(true, true), 'available');
  assert.equal(availability(false, true), 'blocked_by_org_admin');
  assert.equal(availability(true, false), 'blocked_by_org_admin');
});
