import assert from 'node:assert/strict'
import { EventEmitter, once } from 'node:events'
import { after, before, test } from 'node:test'
import { startLocalRuntime } from '../scripts/local-runtime.mjs'
import { applySchema } from './d1-fixture.ts'

let runtime: ReturnType<typeof startLocalRuntime>

before(async () => {
  runtime = startLocalRuntime()
  await applySchema(await runtime.getD1Database('DB', 'billing'))
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

test('readiness requires the single baseline on isolated D1', async (context) => {
  const worker = startLocalRuntime()
  context.after(() => worker.dispose())
  const db = await worker.getD1Database('DB', 'billing')
  const probe = () => worker.dispatchFetch('http://billing.test/api/v1/readiness')
  const empty = await probe()
  assert.equal(empty.status, 503)
  assert.equal((await empty.json()).error.code, 'NOT_READY')
  await applySchema(db)
  assert.equal((await probe()).status, 200)
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
    requestId: response.headers.get('X-Request-Id'),
  })
})

test('unknown routes return a generic JSON error', async () => {
  const response = await runtime.dispatchFetch('http://billing.test/api/v1/unknown')
  assert.equal(response.status, 404)
  assert.deepEqual(await response.json(), {
    error: { code: 'NOT_FOUND', message: 'Route not found' },
    requestId: response.headers.get('X-Request-Id'),
  })
})

function captureBillingLogs() {
  const messages: { level: string; log: Record<string, unknown>; message: string }[] = []
  const events = new EventEmitter()
  return {
    collect({ level, message }: { level: string; message: string }) {
      if (!message.startsWith('{') || !message.includes('"molis-billing"')) return
      messages.push({ level, log: JSON.parse(message), message })
      events.emit('log')
    },
    async waitFor(count: number) {
      const signal = AbortSignal.timeout(5_000)
      while (messages.length < count) await once(events, 'log', { signal })
      return messages
    },
  }
}

test('Rust and JavaScript request logs share the envelope and omit successful health checks', async (context) => {
  const logs = captureBillingLogs()
  const worker = startLocalRuntime({
    bindings: { MOLIS_ENVIRONMENT: 'test' },
    handleStructuredLogs: logs.collect,
  })
  context.after(() => worker.dispose())
  const fetch = async (path: string, id: string, status: number) => {
    const response = await worker.dispatchFetch(`http://billing.test${path}`, {
      headers: { 'X-Request-Id': id, Authorization: 'Bearer do-not-log-token' },
    })
    assert.equal(response.status, status)
    assert.equal(response.headers.get('X-Request-Id'), id)
    if (status >= 400) assert.equal((await response.json()).requestId, id)
    else await response.body?.cancel()
  }
  await fetch('/api/v1/health', 'quiet-health', 200)
  await fetch('/api/v1/readiness', 'unready', 503)
  await applySchema(await worker.getD1Database('DB', 'billing'))
  await fetch('/api/v1/readiness', 'ready', 200)
  await fetch('/api/v1/orders/do-not-log-order?token=do-not-log-query', 'blocked', 404)

  const messages = await logs.waitFor(3)
  assert.equal(messages.length, 3)
  for (const [id, status, code, route, level] of [
    ['unready', 503, 'NOT_READY', '/api/v1/readiness', 'error'],
    ['ready', 200, 'none', '/api/v1/readiness', 'log'],
    ['blocked', 404, 'NOT_FOUND', 'not_found', 'log'],
  ] as const) {
    const entry = messages.find(({ log }) => log.request_id === id)
    assert.ok(entry, id)
    const { duration_ms: duration, ...fields } = entry.log
    assert.ok(typeof duration === 'number' && duration >= 0)
    assert.equal(entry.level, level)
    assert.doesNotMatch(entry.message, /do-not-log/)
    assert.deepEqual(fields, {
      schema: 1,
      service: 'molis-billing',
      env: 'test',
      event: 'billing.http.request',
      request_id: id,
      method: 'GET',
      route,
      status,
      error_code: code,
    })
  }
})

test('scheduled maintenance emits one summary and correlates failures with it', async (context) => {
  const logs = captureBillingLogs()
  const worker = startLocalRuntime({ triggerHandlers: true, handleStructuredLogs: logs.collect })
  context.after(() => worker.dispose())
  const url = new URL('http://billing.test/cdn-cgi/local/scheduled')
  url.searchParams.set('cron', '*/5 * * * *')
  const cron = async () => {
    const response = await worker.dispatchFetch(url)
    assert.equal(response.status, 200, await response.text())
  }

  await cron() // The isolated database has no tables yet.
  const failureLogs = await logs.waitFor(4)
  const summaries = () => failureLogs.filter(({ log }) => log.event === 'billing.maintenance')
  assert.equal(summaries().length, 1)
  const failure = summaries()[0]
  assert.equal(failure.level, 'error')
  assert.equal(failure.log.failed, true)
  for (const key of ['expire', 'sync', 'purge']) {
    assert.deepEqual(failure.log[key], { outcome: 'failed', code: 'BILLING_UNAVAILABLE' })
  }
  const failures = failureLogs.filter(({ log }) => log.event === 'billing.maintenance.failure')
  assert.deepEqual(failures.map(({ log }) => log.task).sort(), [
    'credits.sync',
    'orders.expire',
    'receipts.purge',
  ])
  for (const { level, log } of failures) {
    assert.equal(level, 'error')
    assert.deepEqual(log, {
      schema: 1,
      service: 'molis-billing',
      env: 'unknown',
      event: 'billing.maintenance.failure',
      request_id: failure.log.request_id,
      task: log.task,
      code: 'BILLING_UNAVAILABLE',
    })
  }

  await applySchema(await worker.getD1Database('DB', 'billing'))
  await cron()
  await logs.waitFor(5)
  assert.equal(summaries().length, 2)
  const success = summaries()[1]
  assert.equal(success.level, 'log')
  assert.equal(success.log.failed, false)
  assert.notEqual(success.log.request_id, failure.log.request_id)
  for (const { log } of summaries()) {
    assert.equal(log.schema, 1)
    assert.equal(log.service, 'molis-billing')
    assert.equal(log.env, 'unknown')
    assert.match(String(log.request_id), /^[0-9a-f-]{36}$/u)
    assert.equal(log.budget_exhausted, false)
    assert.ok(typeof log.duration_ms === 'number' && log.duration_ms >= 0)
  }
  for (const key of ['expire', 'sync', 'purge']) {
    assert.deepEqual(success.log[key], { outcome: 'complete', processed: 0, failed: 0 })
  }
})
