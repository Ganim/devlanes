// pm2 ecosystem for acme
module.exports = {
  apps: [
    {
      name: 'api',
      script: 'dist/main.ts',
      env: {
        PORT: 3000, // the API port
      },
    },
    /* a background worker */
    {
      name: "worker",
      script: "./bin/worker",
      interpreter: 'none',
      kill_timeout: 1600,
    },
  ],
};
