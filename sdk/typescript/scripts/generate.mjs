import { mkdir, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { compileFromFile } from "json-schema-to-typescript";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const sdkRoot = resolve(scriptDir, "..");
const schemaPath = resolve(sdkRoot, "../../schemas/sdk-protocol.schema.json");
const outputPath = resolve(sdkRoot, "src/generated.ts");
const source = await compileFromFile(schemaPath, {
  bannerComment: "// Generated from Golutra Rust protocol schemas. Do not edit manually.",
  style: {
    bracketSpacing: true,
    printWidth: 100,
    semi: true,
    singleQuote: false,
    tabWidth: 2,
    trailingComma: "all",
    useTabs: false,
  },
});

await mkdir(dirname(outputPath), { recursive: true });
await writeFile(outputPath, source, "utf8");
