import { execFileSync } from 'node:child_process'
import { existsSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { rustEnvironment } from './rust-toolchain.mjs'

const cwd = fileURLToPath(new URL('../rust/', import.meta.url))
const binary = join(
  cwd,
  '.tools',
  'bin',
  process.platform === 'win32' ? 'worker-build.exe' : 'worker-build',
)
if (!existsSync(binary)) throw new Error('Run npm run setup to install worker-build.')
execFileSync(binary, ['--release', '--no-opt', '.', '--locked'], {
  cwd,
  env: rustEnvironment(),
  stdio: 'inherit',
})
