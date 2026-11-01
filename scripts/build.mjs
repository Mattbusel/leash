// Build both contracts to res/*.wasm with cargo-near (non-reproducible, no embedded ABI).
// --no-abi: the ABI step links a native library, which fails on Windows; the wasm is the same.
import { execFileSync } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
for (const c of ['registry', 'wallet']) {
  console.log(`building contracts/${c}`);
  execFileSync('cargo', ['near', 'build', 'non-reproducible-wasm', '--no-abi', '--out-dir', path.join(root, 'res')], {
    cwd: path.join(root, 'contracts', c),
    stdio: 'inherit',
  });
}
