import assert from 'node:assert/strict'
import { after, before, test } from 'node:test'
import { startLocalRuntime } from '../scripts/local-runtime.mjs'

let runtime: ReturnType<typeof startLocalRuntime>

before(() => {
  runtime = startLocalRuntime()
})
after(() => runtime.dispose())

test('health returns the Worker identity and security headers', async () => {
  const response = await runtime.dispatchFetch('http://billing.test/api/v1/health')
  assert.equal(response.status, 200)
  assert.deepEqual(await response.json(), {
    data: { status: 'UP', service: 'molis-billing', environment: 'billing-isolated-test' },
  })
  for (const [name, value] of Object.entries({
    'content-type': 'application/json; charset=utf-8',
    'cache-control': 'no-store',
    'x-content-type-options': 'nosniff',
    'x-frame-options': 'DENY',
    'referrer-policy': 'no-referrer',
  })) {
    assert.equal(response.headers.get(name), value, name)
  }
})

test('readiness probes the isolated D1 database', async () => {
  const response = await runtime.dispatchFetch('http://billing.test/api/v1/readiness')
  assert.equal(response.status, 200)
  assert.deepEqual(await response.json(), { data: { status: 'READY', database: 'UP' } })
})

test('a missing D1 binding fails readiness while the Worker stays healthy', async (context) => {
  const worker = startLocalRuntime({ database: false })
  context.after(() => worker.dispose())

  const health = await worker.dispatchFetch('http://billing.test/api/v1/health')
  assert.equal(health.status, 200)
  await health.body?.cancel()

  const response = await worker.dispatchFetch('http://billing.test/api/v1/readiness')
  assert.equal(response.status, 503)
  assert.deepEqual(await response.json(), {
    error: { code: 'NOT_READY', message: 'Database unavailable' },
  })
})

test('unknown routes return a generic JSON error', async () => {
  const response = await runtime.dispatchFetch('http://billing.test/api/v1/unknown')
  assert.equal(response.status, 404)
  assert.deepEqual(await response.json(), {
    error: { code: 'NOT_FOUND', message: 'Route not found' },
  })
})
