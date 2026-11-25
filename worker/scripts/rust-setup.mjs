import { execFileSync, spawnSync } from 'node:child_process'
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import {
  cargoBin,
  installedToolchain,
  rustEnvironment,
  rustup,
  toolchain,
} from './rust-toolchain.mjs'

const run = (command, args, env) => execFileSync(command, args, { env, stdio: 'inherit' })
const rustupBinary = join(cargoBin, process.platform === 'win32' ? 'rustup.exe' : 'rustup')
if (spawnSync(rustup, ['--version'], { stdio: 'ignore' }).status !== 0) {
  if (process.platform === 'win32') {
    throw new Error('Install rustup from https://rustup.rs and rerun npm run setup')
  }
  const staging = mkdtempSync(join(tmpdir(), 'molis-billing-rust-'))
  try {
    const installer = join(staging, 'rustup-init.sh')
    run('curl', [
      '--proto',
      '=https',
      '--tlsv1.2',
      '-fsSL',
      'https://sh.rustup.rs',
      '-o',
      installer,
    ])
    run('sh', [
      installer,
      '-y',
      '--no-modify-path',
      '--profile',
      'minimal',
      '--default-toolchain',
      'none',
    ])
  } finally {
    rmSync(staging, { recursive: true, force: true })
  }
}

const manager = existsSync(rustupBinary) ? rustupBinary : rustup
const selected = installedToolchain()
if (!selected) {
  run(manager, [
    'toolchain',
    'install',
    toolchain,
    '--profile',
    'minimal',
    '--component',
    'rustfmt,clippy',
    '--target',
    'wasm32-unknown-unknown',
  ])
} else {
  const installed = execFileSync(
    manager,
    ['component', 'list', '--toolchain', selected, '--installed'],
    {
      encoding: 'utf8',
    },
  ).split('\n')
  if (!installed.includes('rust-std-wasm32-unknown-unknown')) {
    run(manager, ['target', 'add', '--toolchain', selected, 'wasm32-unknown-unknown'])
  }
  const missing = ['rustfmt', 'clippy'].filter(
    (component) =>
      !installed.some((entry) => entry === component || entry.startsWith(component + '-')),
  )
  if (missing.length) run(manager, ['component', 'add', '--toolchain', selected, ...missing])
}

const version = '0.8.6'
const root = fileURLToPath(new URL('../rust/.tools/', import.meta.url))
const binaryName = process.platform === 'win32' ? 'worker-build.exe' : 'worker-build'
const binary = join(root, 'bin', binaryName)

function matchesVersion(path) {
  const result = spawnSync(path, ['--version'], { encoding: 'utf8' })
  return result.status === 0 && result.stdout.trim() === version
}

if (!matchesVersion(binary)) {
  const global = join(cargoBin, binaryName)
  if (matchesVersion(global)) {
    mkdirSync(dirname(binary), { recursive: true })
    copyFileSync(global, binary)
  } else {
    run(
      'cargo',
      [
        'install',
        'worker-build',
        '--version',
        '=' + version,
        '--locked',
        '--root',
        root,
        '--force',
      ],
      rustEnvironment(),
    )
  }
}
