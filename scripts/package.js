/**
 * 打包发布产物：插件 zip。
 * 用法：node scripts/package.js
 */
const { spawnSync } = require('node:child_process');
const path = require('node:path');
const fs = require('node:fs');

const root = path.resolve(__dirname, '..');
const outDir = path.join(root, 'dist-package');
fs.mkdirSync(outDir, { recursive: true });

// 插件：使用 wxt zip 生成 zip
console.log('\n===== 打包插件 =====');
const extZip = spawnSync('pnpm', ['--filter', '@ai-gateway/extension', 'zip'], {
  cwd: root,
  stdio: 'inherit',
  shell: process.platform === 'win32',
});
if (extZip.status !== 0) process.exit(extZip.status ?? 1);

console.log(`\n全部产物位于: ${outDir}`);
