import { fileURLToPath } from 'node:url'
import targets from '../../config/platform-targets.json' with { type: 'json' }
import { deploymentDatabaseId, readProperties } from '../../scripts/environment-properties.mjs'

export const workerDirectory = fileURLToPath(new URL('../', import.meta.url))
export const compatibilityDate = '2026-09-19'
export const localPort = Number(new URL(targets.dev.billingOrigin).port)
export const localPersistPath = fileURLToPath(new URL('../.wrangler/state/', import.meta.url))
const root = new URL('../../', import.meta.url)
const path = (relative) => fileURLToPath(new URL(relative, root))

export function billingEnvironment(name, dryRun) {
  const file = `config/${name}.properties`
  const { databaseId, ...settings } = readProperties(new URL(file, root))
  const { authWorkerName, ...target } = targets[name]
  for (const [key, value] of Object.entries(target)) {
    if (settings[key] !== value)
      throw new Error(`Expected ${key} to match the platform target in ${file}.`)
  }
  for (const key of Object.keys(settings)) {
    if (!Object.hasOwn(target, key)) throw new Error(`Unknown property ${key} in ${file}.`)
  }

  const vars = {
    BILLING_ORIGIN: target.billingOrigin,
    AUTH_ORIGIN: target.authOrigin,
    ENVIRONMENT_ID: target.environmentId,
    MOLIS_ENVIRONMENT: name,
  }
  const platform = {
    name: target.workerName,
    main: path('worker/rust/src/runtime/entrypoint.js'),
    compatibility_date: compatibilityDate,
    workers_dev: false,
    preview_urls: false,
    ...(name === 'dev'
      ? {}
      : {
          routes: [{ pattern: new URL(target.billingOrigin).hostname, custom_domain: true }],
          services: [{ binding: 'AUTH', service: authWorkerName, entrypoint: 'AuthInternal' }],
        }),
    vars,
    d1_databases: [
      {
        binding: 'DB',
        database_name: target.databaseName,
        database_id: deploymentDatabaseId(name, databaseId, dryRun),
        migrations_dir: path('worker/migrations'),
      },
    ],
  }
  const secrets = dryRun
    ? {}
    : readProperties(new URL(`secrets/${name}.properties`, root), { optional: true })
  for (const key of Object.keys(secrets)) {
    if (
      !/^[A-Z][A-Z0-9_]*$/u.test(key) ||
      Object.hasOwn(vars, key) ||
      ['DB', 'AUTH'].includes(key)
    ) {
      throw new Error(`Invalid Worker secret name in secrets/${name}.properties.`)
    }
  }
  return { platform, secrets }
}
