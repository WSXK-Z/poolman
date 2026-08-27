/**
 * 同时启动插件（WXT）与本地服务（tsx watch）开发模式。
 * 用法：node scripts/dev.js
 */
const { spawn } = require('node:child_process');

const TARGETS = [
  { name: 'extension', args: ['--filter', '@ai-gateway/extension', 'dev'] },
  { name: 'server', args: ['--filter', '@ai-gateway/server', 'dev'] },
];

const children = [];

for (const t of TARGETS) {
  const child = spawn('pnpm', t.args, { stdio: 'inherit', shell: process.platform === 'win32' });
  children.push(child);
  child.on('exit', (code) => {
    console.log(`[${t.name}] 已退出，code=${code}`);
    // 一个进程退出则全部退出
    for (const c of children) {
      if (c !== child && !c.killed) c.kill();
    }
    process.exit(code ?? 0);
  });
  child.on('error', (err) => {
    console.error(`[${t.name}] 启动失败:`, err.message);
    process.exit(1);
  });
}

process.on('SIGINT', () => {
  for (const c of children) c.kill();
  process.exit(0);
});
