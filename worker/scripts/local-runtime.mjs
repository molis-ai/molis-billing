import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { Miniflare, convertV4MiniflareOptions } from 'miniflare'
import { compatibilityDate } from './environment.mjs'

const buildDirectory = fileURLToPath(new URL('../rust/build/', import.meta.url))

export function startLocalRuntime({ database = true } = {}) {
  const options = convertV4MiniflareOptions({
    modulesRoot: buildDirectory,
    modules: [
      { type: 'ESModule', path: join(buildDirectory, 'index.js') },
      { type: 'CompiledWasm', path: join(buildDirectory, 'index_bg.wasm') },
    ],
    compatibilityDate,
    bindings: { ENVIRONMENT_ID: 'billing-isolated-test' },
    d1Databases: database ? { DB: 'billing-isolated-test' } : {},
    d1Persist: false,
  })
  return new Miniflare(options)
}
