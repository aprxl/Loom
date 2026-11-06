// Runs the playground's wasm under Node's WASI, as the `loom-play` command:
// `node check.mjs WASM NAME MODE < FILE`. docs/play/build.py runs every sample
// of the docs through it. One program per process: Node 22's WASI crashed when
// one process instantiated the module twice, or wrote its output to files.
import { WASI } from "node:wasi";
import fs from "node:fs";

const [wasmPath, name, mode] = process.argv.slice(2);
const wasi = new WASI({ version: "preview1", args: ["loom-play", name, mode], env: {}, returnOnExit: true });
const instance = await WebAssembly.instantiate(await WebAssembly.compile(fs.readFileSync(wasmPath)), wasi.getImportObject());
process.exitCode = wasi.start(instance);
