/**
 * 构建全部应用（共享包 -> 插件）。
 * 用法：node scripts/build-all.js
 */
const { spawnSync } = require('node:child_process');
const path = require('node:path');

const root = path.resolve(__dirname, '..');

const steps = [
  { name: 'shared', args: ['--filter', '@ai-gateway/shared', 'build'] },
  { name: 'utils', args: ['--filter', '@ai-gateway/utils', 'build'] },
  { name: 'extension', args: ['--filter', '@ai-gateway/extension', 'build'] },
];

for (const step of steps) {
  console.log(`\n===== 构建 ${step.name} =====`);
  const result = spawnSync('pnpm', step.args, { cwd: root, stdio: 'inherit', shell: process.platform === 'win32' });
  if (result.status !== 0) {
    console.error(`构建 ${step.name} 失败`);
    process.exit(result.status ?? 1);
  }
}

console.log('\n全部构建完成 ✅');
