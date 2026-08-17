import { existsSync, readFileSync, realpathSync } from "node:fs";
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const packageRoot = realpathSync(path.join(path.dirname(fileURLToPath(import.meta.url)), ".."));
const rootPackage = JSON.parse(
  readFileSync(path.join(packageRoot, "package.json"), "utf8"),
);

const PLATFORM_PACKAGES = {
  "linux-x64": "@golutra/agent-linux-x64",
  "linux-arm64": "@golutra/agent-linux-arm64",
  "darwin-x64": "@golutra/agent-darwin-x64",
  "darwin-arm64": "@golutra/agent-darwin-arm64",
  "win32-x64": "@golutra/agent-win32-x64",
  "win32-arm64": "@golutra/agent-win32-arm64",
};

export async function runNative(binaryName) {
  if (isVersionRequest(process.argv.slice(2))) {
    console.log(`${binaryName} ${rootPackage.version}`);
    return;
  }

  const platformKey = `${process.platform}-${process.arch}`;
  const platformPackage = PLATFORM_PACKAGES[platformKey];
  if (!platformPackage) {
    fail(
      `Golutra does not publish a native package for ${platformKey}. ` +
        "Supported targets: linux-x64, linux-arm64, darwin-x64, darwin-arm64, win32-x64, win32-arm64.",
    );
  }

  let platformRoot;
  try {
    const packageJson = require.resolve(`${platformPackage}/package.json`, {
      paths: [packageRoot],
    });
    platformRoot = path.dirname(realpathSync(packageJson));
  } catch {
    fail(
      `The native package ${platformPackage} is missing. ` +
        `Reinstall with: npm install -g @golutra/agent@${rootPackage.version}`,
    );
  }

  const executable = path.join(
    platformRoot,
    "vendor",
    "bin",
    process.platform === "win32" ? `${binaryName}.exe` : binaryName,
  );
  if (!existsSync(executable)) {
    fail(`The native executable is missing from ${platformPackage}: ${executable}`);
  }

  const child = spawn(executable, process.argv.slice(2), {
    env: {
      ...process.env,
      GOLUTRA_MANAGED_PACKAGE_ROOT: packageRoot,
      GOLUTRA_PACKAGE_TARGET: platformKey,
    },
    stdio: "inherit",
  });

  const forwardSignal = (signal) => {
    if (!child.killed) {
      child.kill(signal);
    }
  };
  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
    process.on(signal, () => forwardSignal(signal));
  }

  child.on("error", (error) => {
    console.error(`Failed to start ${binaryName}: ${error.message}`);
    process.exitCode = 1;
  });

  const result = await new Promise((resolve) => {
    child.on("exit", (code, signal) => resolve({ code, signal }));
  });
  if (result.signal) {
    process.kill(process.pid, result.signal);
  }
  process.exitCode = result.code ?? 1;
}

function isVersionRequest(args) {
  return args.length === 1 && (args[0] === "--version" || args[0] === "-V");
}

function fail(message) {
  console.error(message);
  process.exit(1);
}
