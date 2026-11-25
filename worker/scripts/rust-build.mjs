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
const fixtureFeatures = ['order-fixture', 'credit-fixture']
if (args.includes('--production')) {
  if (args.length !== 1) throw new Error('--production must be used without fixture options.')
  cargoArgs.push('--no-default-features')
} else {
  if (args.some((arg) => !fixtureFeatures.some((feature) => arg === `--${feature}`)))
    throw new Error('Use --production for deployment or fixture options for local acceptance.')
  const selected = fixtureFeatures.filter((feature) => args.includes(`--${feature}`))
  if (selected.length) cargoArgs.push('--features', selected.join(','))
}
execFileSync(binary, ['--release', '--no-opt', '.', ...cargoArgs], {
  cwd,
  env: rustEnvironment(),
  stdio: 'inherit',
})
