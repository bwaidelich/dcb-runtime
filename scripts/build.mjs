// Builds the runtime component and its Node transpilation:
//
//   build/runtime.js        the playground's model.js and evaluate.js,
//                           concatenated before src/contract.js and
//                           src/runtime.js – one module, one scope, as
//                           the playground's own tests load them
//   build/dcb-runtime.wasm  the WebAssembly component (wit/runtime.wit)
//   build/node/             the component transpiled for Node, its store
//                           import mapped to hosts/node/memory-store.mjs
import { readFileSync, writeFileSync, mkdirSync, rmSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const at = (...parts) => join(root, ...parts);
const jco = (...args) => execFileSync(at('node_modules', '.bin', 'jco'), args, { stdio: 'inherit', cwd: root });

rmSync(at('build'), { recursive: true, force: true });
mkdirSync(at('build'), { recursive: true });

const sources = [
  'playground/app/model.js',
  'playground/app/evaluate.js',
  'src/contract.js',
  'src/runtime.js',
];
writeFileSync(at('build', 'runtime.js'),
  sources.map((file) => `// ---- ${file}\n${readFileSync(at(file), 'utf8')}`).join('\n'));

jco('componentize', 'build/runtime.js', '--wit', 'wit/runtime.wit', '--world-name', 'runtime',
  '--disable', 'all', '-o', 'build/dcb-runtime.wasm');
jco('transpile', 'build/dcb-runtime.wasm', '-o', 'build/node', '--quiet',
  '--map', 'dcb:runtime/store@0.2.0=../../hosts/node/memory-store.mjs');
writeFileSync(at('build', 'node', 'package.json'), '{ "type": "module" }\n');
