import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { Miniflare, convertV4MiniflareOptions } from 'miniflare'
import { compatibilityDate } from './environment.mjs'

const rustDirectory = new URL('../rust/', import.meta.url)
const rustPath = (relative) => fileURLToPath(new URL(relative, rustDirectory))

export function startLocalRuntime({
  database = true,
  meter = false,
  bindings = {},
  auth,
  triggerHandlers = false,
  handleStructuredLogs,
} = {}) {
  return new Miniflare(
    convertV4MiniflareOptions({
      workers: [
        {
          name: 'billing',
          modulesRoot: rustPath('.'),
          modules: [
            ...(meter
              ? [
                  {
                    type: 'ESModule',
                    path: rustPath('meter.js'),
                    contents: readFileSync(
                      new URL('../tests/d1-meter.js', import.meta.url),
                      'utf8',
                    ),
                  },
                ]
              : []),
            { type: 'ESModule', path: rustPath('src/runtime/entrypoint.js') },
            { type: 'ESModule', path: rustPath('build/index.js') },
            { type: 'CompiledWasm', path: rustPath('build/index_bg.wasm') },
          ],
          compatibilityDate,
          bindings: { ENVIRONMENT_ID: 'billing-isolated-test', ...bindings },
          d1Databases: database ? { DB: 'billing-isolated-test' } : {},
          serviceBindings: auth ? { AUTH: auth } : {},
        },
        {
          name: 'caller',
          compatibilityDate,
          modules: true,
          script: 'export default { fetch(request, env) { return env.BILLING.fetch(request) } }',
          serviceBindings: { BILLING: { name: 'billing', entrypoint: 'BillingInternal' } },
        },
      ],
      d1Persist: false,
      unsafeTriggerHandlers: triggerHandlers,
      handleStructuredLogs,
    }),
  )
}
