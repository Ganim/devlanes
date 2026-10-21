const path = require('path');

module.exports = {
  apps: [
    {
      name: 'api',
      script: path.join(__dirname, 'server.js'),
    },
  ],
};
