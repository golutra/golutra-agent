#!/usr/bin/env node

import { runNative } from "./run.js";

// 无参数是面向人的入口；显式子命令继续走脚本 CLI，保持自动化语义稳定。
await runNative(process.argv.length === 2 ? "golutra-tui" : "golutra");
