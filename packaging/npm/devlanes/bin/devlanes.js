#!/usr/bin/env node
"use strict";

// Runs the native binary from the @devlanes/<platform>-<arch> package that npm
// installed as an optional dependency.
const { spawn } = require("node:child_process");
const path = require("node:path");

const platform = `${process.platform}-${process.arch}`;
let binary;
try {
  const manifest = require.resolve(`@devlanes/${platform}/package.json`);
  binary = path.join(path.dirname(manifest), "bin", "devlanes");
} catch {
  console.error(
    `devlanes: no binary installed for ${platform}. Release builds exist for ` +
      "darwin-arm64, darwin-x64, linux-arm64 and linux-x64 (glibc).\n" +
      "If npm ran with --omit=optional or --no-optional, reinstall without it.",
  );
  process.exit(1);
}

const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });

// The terminal already delivers SIGINT and SIGQUIT to the whole process group,
// so forwarding them would reach the binary twice. Signals sent only to this
// wrapper are passed on.
for (const signal of ["SIGINT", "SIGQUIT"]) process.on(signal, () => {});
for (const signal of ["SIGTERM", "SIGHUP"]) {
  process.on(signal, () => child.kill(signal));
}

child.on("error", (error) => {
  console.error(`devlanes: cannot run ${binary}: ${error.message}`);
  process.exit(1);
});
child.on("exit", (code, signal) => {
  if (signal) {
    process.removeAllListeners(signal);
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 1);
});
