"use strict";

// Postinstall: replaces the Node launcher at bin/devlanes with the native binary
// from @devlanes/<platform>-<arch>, so the package manager's bin link runs it
// without starting Node. Any failure leaves the launcher in place and never fails
// the install.
const { execFileSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const { version } = require("./package.json");
const binDir = path.join(__dirname, "bin");

// Yarn PnP keeps packages in zip archives. Yarn may also rerun install scripts in
// a node_modules shared between platforms (for example a mounted Docker volume),
// and a swapped binary would then be the wrong one, so Yarn keeps the launcher.
function skipped() {
  return (
    process.platform === "win32" ||
    Boolean(process.versions.pnp) ||
    /\byarn\//.test(process.env.npm_config_user_agent || "")
  );
}

/** Fails unless `file args` prints `<name> <version>` as its first two words. */
function checkVersion(file, args, name) {
  const output = execFileSync(file, args, { stdio: "pipe", timeout: 30_000 }).toString();
  const [reportedName, reportedVersion] = output.trim().split(/\s+/);
  if (reportedName !== name || reportedVersion !== version) {
    throw new Error(`${name} reported ${JSON.stringify(output.trim())}, expected ${version}`);
  }
}

/**
 * Puts `source` at bin/<name>: a hard link (else a copy) to a temporary name in
 * the same directory, checked by running it, then renamed over the target. The
 * rename never writes into an existing file, which pnpm may share with its store.
 */
function place(source, name, args) {
  const target = path.join(binDir, name);
  const temp = path.join(binDir, `.${name}.${process.pid}.tmp`);
  fs.rmSync(temp, { force: true });
  try {
    try {
      fs.linkSync(source, temp);
    } catch {
      fs.copyFileSync(source, temp);
      fs.chmodSync(temp, 0o755);
    }
    checkVersion(temp, args, name);
    fs.renameSync(temp, target);
  } finally {
    // A rerun links the file that is already the target; rename then succeeds
    // without removing the temporary name.
    fs.rmSync(temp, { force: true });
  }
}

function main() {
  if (skipped()) return;
  const platform = `${process.platform}-${process.arch}`;
  const manifest = require.resolve(`@devlanes/${platform}/package.json`);
  const sourceDir = path.join(path.dirname(manifest), "bin");
  // `devlanes certs trust --hosts` looks for devlanes-hosts next to the resolved
  // devlanes executable, so the helper moves first and the binary only with it.
  place(path.join(sourceDir, "devlanes-hosts"), "devlanes-hosts", ["version"]);
  place(path.join(sourceDir, "devlanes"), "devlanes", ["--version"]);
}

try {
  main();
} catch (error) {
  const reason = String(error && error.message ? error.message : error).split("\n")[0];
  console.error(`devlanes: keeping the Node launcher: ${reason}`);
}
