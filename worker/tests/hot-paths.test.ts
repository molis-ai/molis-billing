import assert from 'node:assert/strict'
import { createHmac, randomBytes } from 'node:crypto'
import { test } from 'node:test'
import { startLocalRuntime } from '../scripts/local-runtime.mjs'
import { applySchema, seedPlans } from './d1-fixture.ts'

test('steady-state requests use one D1 snapshot and one batch per mutation', async (t) => {
  const secret = randomBytes(32).toString('base64url')
  const signing = randomBytes(32).toString('base64url')
  const runtime = startLocalRuntime({
    meter: true,
    bindings: {
      MOLIS_ENVIRONMENT: 'test',
      MOLIS_INTERNAL_SECRET: secret,
      MOLIS_MOCK_WEBHOOK_SECRET: signing,
    },
    auth: () =>
      Response.json({
        data: {
          userId: 'meter',
          owner: { kind: 'USER', id: 'meter' },
          product: 'WORK',
          role: 'SELF',
        },
      }),
  })
  t.after(() => runtime.dispose())
  const caller = await runtime.getWorker('caller')
  const db = await runtime.getD1Database('DB', 'billing')
  await applySchema(db)
  await seedPlans(db)
  const now = Date.UTC(2028, 0, 1)
  let key = 0
  async function call(label: string, path: string, method: string, body?: object) {
    const headers: Record<string, string> = {
      'X-Molis-Internal-Secret': secret,
      'X-Test-Now': String(now),
      'Idempotency-Key': 'meter-' + ++key,
    }
    if (!path.startsWith('/internal')) headers.Authorization = 'Bearer local'
    if (body) headers['Content-Type'] = 'application/json'
    const response = await caller.fetch('https://billing.internal' + path, {
      method,
      headers,
      body: body && JSON.stringify(body),
    })
    const result = await response.json()
    assert.ok(response.ok, JSON.stringify(result))
    const counts = JSON.parse(response.headers.get('X-Test-D1-Counts')!)
    console.log('D1_SAMPLE', label, JSON.stringify(counts))
    assert.deepEqual(counts, { reads: 1, writes: method === 'GET' ? 0 : 1 })
    if (method === 'POST') {
      const id = response.headers.get('X-Request-Id')
      assert.ok(id)
      assert.ok(
        await db.prepare('SELECT id FROM billing_audit WHERE request_id=?').bind(id).first(),
      )
      if (path.startsWith('/internal') || label === 'pay')
        assert.ok(
          await db
            .prepare('SELECT id FROM billing_credit_ledger WHERE request_id=?')
            .bind(id)
            .first(),
        )
    }
    return result.data
  }
  const orders = '/api/v1/orders'
  const order = () =>
    call('create', orders, 'POST', { planId: 'work-personal-test', interval: 'MONTH' })
  const paid = await order()
  await call('pay', orders + '/' + paid.id + '/mock-pay', 'POST')
  const renewal = await order()
  await call('pay-renewal', orders + '/' + renewal.id + '/mock-pay', 'POST')
  await call('refund', orders + '/' + renewal.id + '/refund', 'POST')
  const callbackOrder = await order()
  const payload = JSON.stringify({
    eventId: 'pay.' + callbackOrder.id,
    kind: 'PAYMENT',
    paymentId: 'mock.' + callbackOrder.id,
    orderId: callbackOrder.id,
    amountMinor: callbackOrder.amountMinor,
    currency: callbackOrder.currency,
  })
  const callback = await runtime.dispatchFetch('https://billing.internal/api/v1/webhooks/mock', {
    method: 'POST',
    headers: {
      'X-Test-Now': String(now),
      'X-Molis-Payment-Signature': createHmac('sha256', signing).update(payload).digest('hex'),
    },
    body: payload,
  })
  assert.equal(callback.status, 200, await callback.text())
  const callbackCounts = JSON.parse(callback.headers.get('X-Test-D1-Counts')!)
  console.log('D1_SAMPLE webhook', JSON.stringify(callbackCounts))
  assert.deepEqual(callbackCounts, { reads: 1, writes: 1 })
  assert.equal(
    await db
      .prepare('SELECT credits_complete FROM billing_order_service_period WHERE order_id=?')
      .bind(paid.id)
      .first('credits_complete'),
    1,
  )
  await call('balance', '/api/v1/credits/balance', 'GET')
  const credits = '/internal/v1/credits'
  const usage = {
    actorUserId: 'meter',
    callingService: 'local',
    product: 'WORK',
    owner: { kind: 'USER', id: 'meter' },
    referenceId: 'task',
    occurredAt: now,
  }
  const { hold } = await call('hold', credits + '/holds', 'POST', {
    ...usage,
    amount: 10,
    expiresAt: now + 1000,
  })
  await call('consume', credits + '/consume', 'POST', { ...usage, amount: 1 })
  await call('settle', credits + '/holds/' + hold.id + '/settle', 'POST', {
    ...usage,
    segments: [{ occurredAt: now, amount: 2 }],
  })
  await call('release', credits + '/holds/' + hold.id + '/release', 'POST', usage)
})
