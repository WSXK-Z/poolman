/**
 * PM2 生产配置
 * 使用：cd apps/server && pnpm build && pm2 start ecosystem.config.js
 */
module.exports = {
  apps: [
    {
      name: 'ai-gateway',
      script: 'dist/index.js',
      instances: 1,
      exec_mode: 'fork',
      cwd: __dirname,
      env: {
        NODE_ENV: 'production',
        HOST: '127.0.0.1',
        PORT: 3000,
        WS_PORT: 3001,
        LOG_LEVEL: 'info',
      },
      max_memory_restart: '300M',
      error_file: '../../logs/server-error.log',
      out_file: '../../logs/server-out.log',
      time: true,
    },
  ],
};
