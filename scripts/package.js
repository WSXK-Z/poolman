/**
 * 打包发布产物：插件 zip + 服务 zip。
 * 用法：node scripts/package.js
 */
const { spawnSync } = require('node:child_process');
const path = require('node:path');
const fs = require('node:fs');
const { execSync } = require('node:child_process');

const root = path.resolve(__dirname, '..');
const outDir = path.join(root, 'dist-package');
fs.mkdirSync(outDir, { recursive: true });

// 1. 插件：使用 wxt zip 生成 zip
console.log('\n===== 打包插件 =====');
const extZip = spawnSync('pnpm', ['--filter', '@ai-gateway/extension', 'zip'], {
  cwd: root,
  stdio: 'inherit',
  shell: process.platform === 'win32',
});
if (extZip.status !== 0) process.exit(extZip.status ?? 1);

// 2. 服务：压缩 dist + package.json + ecosystem.config.js
console.log('\n===== 打包服务 =====');
const serverDir = path.join(root, 'apps', 'server');
const serverDist = path.join(serverDir, 'dist');
if (!fs.existsSync(serverDist)) {
  console.error('未找到 apps/server/dist，请先执行构建');
  process.exit(1);
}

const serverZipName = `ai-gateway-server-${new Date().toISOString().slice(0, 10)}.zip`;
const serverZip = path.join(outDir, serverZipName);

try {
  execSync(
    `powershell -NoProfile -Command "Compress-Archive -Path '${serverDir}\\dist\\*','${serverDir}\\package.json','${serverDir}\\ecosystem.config.js' -DestinationPath '${serverZip}' -Force"`,
    { stdio: 'inherit' },
  );
  console.log(`服务包已生成: ${serverZip}`);
} catch {
  console.error('服务打包失败（Windows 下请确认 powershell 可用）');
  process.exit(1);
}

console.log(`\n全部产物位于: ${outDir}`);
