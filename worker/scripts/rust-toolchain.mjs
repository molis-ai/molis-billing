import { execFileSync, spawnSync } from 'node:child_process'
import { existsSync } from 'node:fs'
import { homedir } from 'node:os'
import { delimiter, dirname, join } from 'node:path'

export const toolchain = '1.96.0'
export const cargoBin = join(process.env.CARGO_HOME || join(homedir(), '.cargo'), 'bin')
const executableSuffix = process.platform === 'win32' ? '.exe' : ''
const rustupPath = join(cargoBin, 'rustup' + executableSuffix)
export const rustup = existsSync(rustupPath) ? rustupPath : 'rustup'

export function installedToolchain() {
  // An existing stable toolchain is usable when it provides the same pinned release.
  for (const name of [toolchain, 'stable']) {
    const located = spawnSync(rustup, ['which', '--toolchain', name, 'rustc'], {
      encoding: 'utf8',
    })
    if (located.status !== 0) continue
    const version = spawnSync(located.stdout.trim(), ['--version'], { encoding: 'utf8' })
    if (version.status === 0 && version.stdout.startsWith(`rustc ${toolchain} `)) return name
  }
  return undefined
}

export function rustEnvironment() {
  const selected = installedToolchain()
  if (!selected) throw new Error(`Rust ${toolchain} is required. Run npm run setup first.`)
  const rustc = execFileSync(rustup, ['which', '--toolchain', selected, 'rustc'], {
    encoding: 'utf8',
  }).trim()
  return {
    ...process.env,
    RUSTUP_TOOLCHAIN: selected,
    RUSTC: rustc,
    RUSTDOC: join(dirname(rustc), 'rustdoc' + executableSuffix),
    PATH: [dirname(rustc), cargoBin, process.env.PATH].join(delimiter),
  }
}
