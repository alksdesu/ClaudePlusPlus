const { readdirSync } = require('node:fs');
const { join } = require('node:path');
const { spawnSync } = require('node:child_process');

const files = readdirSync(__dirname).filter(name => name.endsWith('.test.cjs')).sort().map(name => join(__dirname, name));
if (files.length === 0) throw new Error('No frontend tests found');
const result = spawnSync(process.execPath, ['--test', ...process.argv.slice(2), ...files], { stdio: 'inherit' });
if (result.error) throw result.error;
process.exitCode = result.status ?? 1;
