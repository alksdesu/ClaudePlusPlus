const test = require('node:test');
const assert = require('node:assert/strict');
const vm = require('node:vm');
const fs = require('node:fs');
const path = require('node:path');
const recipe = JSON.parse(fs.readFileSync(path.join(__dirname, '../src-tauri/src/computer_use/resources/macos-3.6556.2.json'), 'utf8'));
const stateChange = recipe.fast_state.changes.find(p => p.before.startsWith('nt='));
const state = (source, values) => vm.runInNewContext(source.replace(/^nt=/, ''), { ge: null, E: false, D: undefined, xe: false, w: true, Ze: undefined, u: {}, t: {}, K: false, re: null, G: false, ...values });

test('native local Fast state wins over stale defaults without replacing the selector', () => {
  assert.equal(state(stateChange.before, { D: 'off', G: true }), true);
  assert.equal(state(stateChange.after, { D: 'off', G: true }), false);
  for (const D of ['on', 'cooldown']) assert.equal(state(stateChange.after, { D }), true);
  assert.equal(state(stateChange.after, { D: 'on', ge: false }), false);
  assert.equal(state(stateChange.after, { D: 'on', xe: true }), false);
  assert.deepEqual(recipe.fast_widget.changes, []);
  assert.equal(recipe.fast_widget.original, recipe.fast_widget.modified);
});

test('remote, draft and unknown Fast states keep the official behavior', () => {
  for (const values of [{ E: true, D: 'off', G: true }, { E: true, D: 'on' }, { w: false, t: null, D: 'on' }, { D: 'pending', G: true }, { D: undefined, Ze: true }, { u: null }]) {
    assert.equal(state(stateChange.after, values), state(stateChange.before, values));
  }
});

test('the capability mapping still respects Code policy and local spawning', () => {
  const source = recipe.main.fast_changes.find(p => p.before.startsWith('claude_code_fast_mode:')).after.split(':').slice(1).join(':');
  for (const n of [false, true]) for (const allowed of [false, true]) {
    const value = vm.runInNewContext(source, { n, aT: () => ({ allowsLocalSpawn: allowed }), t: enabled => enabled ? 'available' : 'blocked_by_org_admin' });
    assert.equal(value, n && allowed ? 'available' : 'blocked_by_org_admin');
  }
});

test('Chinese uses the official locale resolver and existing resource paths', () => {
  const lists = recipe.language.changes.filter(p => p.before.startsWith('['));
  assert.equal(lists.length, 1);
  assert.equal(lists[0].count, 2);
  const languages = JSON.parse(lists[0].after);
  assert.ok(languages.includes('zh-Hans'));
  assert.ok(languages.includes('zh-Hant'));
  const resolver = recipe.language.changes.find(p => p.before.startsWith('function xy('));
  let called;
  const context = { by: languages, vy: (requested, available) => { called = [requested, available]; return 'zh-Hant'; } };
  assert.equal(vm.runInNewContext(`${resolver.after};xy(['zh-TW'])`, context), 'zh-Hant');
  assert.equal(called[0][0], 'zh-TW');
  assert.equal(called[1], languages);
});

test('the new recipe preserves background renderer and limits prewarm removal to deployment type', () => {
  assert.deepEqual(recipe.background.changes, []);
  assert.equal(recipe.prewarm.changes.length, 1);
  assert.match(recipe.prewarm.changes[0].before, /type!=="1p"/);
  assert.equal(recipe.prewarm.changes[0].after, '');
  assert.equal(recipe.main.profiles.length, 16);
});
