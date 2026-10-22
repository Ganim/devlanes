#!/usr/bin/env node
"use strict";

// Runs the native binary from the @devlanes/<platform>-<arch> package that npm
// installed as an optional dependency.
const { spawn } = require("node:child_process");
const path = require("node:path");

const windows = process.platform === "win32";
const platform = `${process.platform}-${process.arch}`;
let binary;
try {
  const manifest = require.resolve(`@devlanes/${platform}/package.json`);
  binary = path.join(path.dirname(manifest), "bin", windows ? "devlanes.exe" : "devlanes");
} catch {
  console.error(
    `devlanes: no binary installed for ${platform}. Release builds exist for ` +
      "darwin-arm64, darwin-x64, linux-arm64 and linux-x64 (glibc), win32-arm64 and win32-x64.\n" +
      "If npm ran with --omit=optional or --no-optional, reinstall without it.",
  );
  process.exit(1);
}

const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });

if (windows) {
  // Ctrl-C, Ctrl-Break and closing the console window reach every process
  // attached to the console, the binary included, which handles them itself.
  // The listeners only keep Node from exiting before it; Windows has no
  // SIGTERM to forward, and child.kill() would end the binary without notice.
  for (const signal of ["SIGINT", "SIGBREAK", "SIGHUP"]) process.on(signal, () => {});
} else {
  // The terminal already delivers SIGINT and SIGQUIT to the whole process group,
  // so forwarding them would reach the binary twice. Signals sent only to this
  // wrapper are passed on.
  for (const signal of ["SIGINT", "SIGQUIT"]) process.on(signal, () => {});
  for (const signal of ["SIGTERM", "SIGHUP"]) {
    process.on(signal, () => child.kill(signal));
  }
}

child.on("error", (error) => {
  console.error(`devlanes: cannot run ${binary}: ${error.message}`);
  process.exit(1);
});
child.on("exit", (code, signal) => {
  // Windows processes end with a code only, such as 0xC000013A after Ctrl-C,
  // which process.exit passes on unchanged.
  if (signal && !windows) {
    process.removeAllListeners(signal);
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 1);
});
