import { execFileSync } from 'node:child_process'
import { fileURLToPath } from 'node:url'
import { rustEnvironment } from './rust-toolchain.mjs'

const wasmTarget = ['--target', 'wasm32-unknown-unknown']
const commands = {
  check: ['check', '--locked', ...wasmTarget],
  clippy: ['clippy', '--locked', '--lib', ...wasmTarget, '--', '-D', 'warnings'],
  test: ['test', '--locked', '--lib'],
  format: ['fmt'],
  'format:check': ['fmt', '--check'],
}
const action = process.argv[2]
if (!Object.hasOwn(commands, action)) throw new Error('Unknown Rust action')
execFileSync('cargo', commands[action], {
  cwd: fileURLToPath(new URL('../rust/', import.meta.url)),
  env: rustEnvironment(),
  stdio: 'inherit',
})
