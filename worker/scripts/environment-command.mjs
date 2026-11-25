import { spawn } from 'node:child_process'
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { assertPortAvailable, selectEnvironment } from '../../scripts/environment-properties.mjs'
import { billingEnvironment, localPersistPath, localPort, workerDirectory } from './environment.mjs'

const [operation, ...args] = process.argv.slice(2)
const { name, dryRun } = selectEnvironment(operation, args)
const local = operation === 'start'
const { platform, secrets } = billingEnvironment(name, dryRun)
if (local) await assertPortAvailable(localPort)

const wranglerEnv = {
  ...process.env,
  CLOUDFLARE_LOAD_DEV_VARS_FROM_DOT_ENV: 'false',
  CLOUDFLARE_INCLUDE_PROCESS_ENV: 'false',
  WRANGLER_LOG_SANITIZE: 'true',
  WRANGLER_SEND_METRICS: 'false',
}
for (const key of [
  'CLOUDFLARE_ENV',
  'WRANGLER_CI_OVERRIDE_NAME',
  'WRANGLER_CI_GENERATE_PREVIEW_ALIAS',
]) {
  delete wranglerEnv[key]
}

let receivedSignal
function run(script, args = []) {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [script, ...args], {
      cwd: workerDirectory,
      env: wranglerEnv,
      stdio: 'inherit',
    })
    const interrupt = () => stop('SIGINT')
    const terminate = () => stop('SIGTERM')
    function stop(signal) {
      receivedSignal = signal
      child.kill(signal)
    }
    process.on('SIGINT', interrupt)
    process.on('SIGTERM', terminate)
    child.on('error', reject)
    child.on('close', (code, signal) => {
      process.off('SIGINT', interrupt)
      process.off('SIGTERM', terminate)
      if (code === 0 || receivedSignal) resolve()
      else reject(new Error(`${script} exited with ${signal ?? code}.`))
    })
  })
}

await run('scripts/rust-build.mjs', name === 'prod' ? ['--production'] : [])
if (!receivedSignal) {
  const staging = mkdtempSync(join(tmpdir(), 'molis-billing-'))
  try {
    const config = join(staging, 'wrangler.json')
    writeFileSync(config, JSON.stringify(platform), { mode: 0o600 })
    const command = [local ? 'dev' : 'deploy', '--config', config]
    if (local) {
      command.push(
        '--local',
        '--port',
        String(localPort),
        '--ip',
        '127.0.0.1',
        '--persist-to',
        localPersistPath,
      )
    }
    if (dryRun) command.push('--dry-run')
    if (Object.keys(secrets).length) {
      if (local) {
        const devVars = Object.entries(secrets)
          .map(([key, value]) => `${key}=${JSON.stringify(value)}\n`)
          .join('')
        writeFileSync(join(staging, '.dev.vars'), devVars, { mode: 0o600 })
      } else {
        const secretsFile = join(staging, 'secrets.json')
        writeFileSync(secretsFile, JSON.stringify(secrets), { mode: 0o600 })
        command.push('--secrets-file', secretsFile)
      }
    }
    await run('node_modules/wrangler/bin/wrangler.js', command)
  } finally {
    rmSync(staging, { recursive: true, force: true })
  }
}
if (receivedSignal) process.exitCode = receivedSignal === 'SIGINT' ? 130 : 143
