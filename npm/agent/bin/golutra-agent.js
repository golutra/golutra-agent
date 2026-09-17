#!/usr/bin/env node

import { runNative } from "./run.js";

// 原生入口统一选择交互或脚本模式；桌面和 npm 不维护两套分派逻辑。
await runNative("golutra-agent");
