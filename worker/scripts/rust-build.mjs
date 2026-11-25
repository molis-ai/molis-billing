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
const args = process.argv.slice(2)
const cargoArgs = ['--locked']
if (args.length === 1 && args[0] === '--production') {
  cargoArgs.push('--no-default-features')
} else if (args.length === 1 && args[0] === '--credit-fixture') {
  cargoArgs.push('--features', 'credit-fixture')
} else if (args.length) {
  throw new Error('Use --production for deployment or --credit-fixture for local acceptance.')
}
execFileSync(binary, ['--release', '--no-opt', '.', ...cargoArgs], {
  cwd,
  env: rustEnvironment(),
  stdio: 'inherit',
})
