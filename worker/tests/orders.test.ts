import assert from 'node:assert/strict'
import { createHmac, randomBytes } from 'node:crypto'
import { test, type TestContext } from 'node:test'
import { startLocalRuntime } from '../scripts/local-runtime.mjs'
import { applySchema, seedPlans } from './d1-fixture.ts'

const origin = 'https://billing.internal'
const base = '/api/v1'
const auditPath = '/internal/v1/audit-events'
const personal = 'work-personal-test'
const team = 'work-team-test'
const windowMs = 1_800_000
const minute = 60_000
const at = (iso: string) => Date.parse(iso)

type Scope = {
  now?: number
  commitTime?: number
  key?: string | null
  actor?: string
  tenant?: string
  requestId?: string
}

type Order = {
  id: string
  status: string
  amountMinor: number
  currency: string
  paymentExpiresAt: number
  servicePeriod: { orderId: string; startsAt: number; endsAt: number; revokedAt: number | null }
}

async function setup(t: TestContext, environment = 'test', withWebhookSecret = true) {
  const internal = randomBytes(32).toString('base64url')
  const signing = randomBytes(32).toString('base64url')
  let auditPermission = true
  const auditChecks: (string | null)[] = []
  const runtime = startLocalRuntime({
    bindings: {
      MOLIS_ENVIRONMENT: environment,
      MOLIS_INTERNAL_SECRET: internal,
      BILLING_CREDIT_FIXTURE: '1',
      ...(withWebhookSecret && { MOLIS_MOCK_WEBHOOK_SECRET: signing }),
    },
    auth: async (request: Request) => {
      const url = new URL(request.url)
      assert.equal(url.origin, 'https://auth.internal')
      assert.equal(request.method, 'POST')
      assert.equal(request.headers.get('X-Molis-Internal-Secret'), internal)
      const actor = request.headers.get('Authorization')?.replace('Bearer ', '')
      if (actor === 'expired')
        return Response.json({ error: { code: 'SESSION_EXPIRED' } }, { status: 401 })
      if (actor === 'left') return Response.json({ error: { code: 'FORBIDDEN' } }, { status: 403 })
      if (url.pathname === '/internal/v1/platform/audit/authorize') {
        auditChecks.push(request.headers.get('X-Request-Id'))
        assert.deepEqual(await request.json(), {})
        assert.equal(request.headers.has('Cookie'), false)
        if (actor === 'bridge-missing')
          return Response.json({ error: { code: 'NOT_FOUND' } }, { status: 404 })
        if (actor === 'session-only') return Response.json({ data: { userId: actor } })
        if (actor !== 'platform-auditor' || !auditPermission)
          return Response.json({ error: { code: 'FORBIDDEN' } }, { status: 403 })
        return Response.json({ data: { userId: actor, permission: 'AUDIT_READ' } })
      }
      assert.equal(url.pathname, '/internal/v1/subjects/verify')
      const { product, owner } = await request.json()
      return Response.json({
        data: {
          userId: owner.kind === 'USER' ? 'user-fixture' : actor,
          role: owner.kind === 'USER' ? 'SELF' : actor === 'member' ? 'MEMBER' : 'ADMIN',
          owner: { kind: owner.kind, id: owner.id ?? 'user-fixture' },
          product,
        },
      })
    },
  })
  t.after(() => runtime.dispose())
  const [caller, db] = await Promise.all([
    runtime.getWorker('caller'),
    runtime.getD1Database('DB', 'billing'),
  ])
  await applySchema(db)
  await seedPlans(db)

  let keySequence = 0
  function call(path: string, scope: Scope = {}, method = 'GET', body?: object | string) {
    const url =
      origin + base + path + (scope.tenant ? `?ownerKind=TENANT&ownerId=${scope.tenant}` : '')
    return caller.fetch(url, {
      method,
      headers: {
        'X-Order-Fixture': '1',
        'X-Molis-Internal-Secret': internal,
        Authorization: `Bearer ${scope.actor ?? 'admin-1'}`,
        ...(scope.now !== undefined && {
          'X-Order-Fixture-Time': String(scope.now),
          'X-Fixture-Now': String(scope.now),
        }),
        ...(scope.commitTime !== undefined && {
          'X-Order-Fixture-Commit-Time': String(scope.commitTime),
        }),
        ...(method === 'POST' &&
          scope.key !== null && {
            'Idempotency-Key': scope.key ?? `request-${++keySequence}`,
          }),
        ...(scope.requestId && { 'X-Request-Id': scope.requestId }),
        ...(body && { 'Content-Type': 'application/json' }),
      },
      ...(body && { body: typeof body === 'string' ? body : JSON.stringify(body) }),
    })
  }

  function create(scope: Scope, interval = 'MONTH', billingTimeZone?: string) {
    return call('/orders', scope, 'POST', {
      planId: scope.tenant ? team : personal,
      interval,
      billingTimeZone,
    })
  }

  function pay(order: Pick<Order, 'id'>, scope: Scope) {
    return call(`/orders/${order.id}/mock-pay`, scope, 'POST')
  }

  function refund(order: Order, scope: Scope) {
    return call(`/orders/${order.id}/refund`, scope, 'POST', {})
  }

  function event(
    order: Pick<Order, 'id' | 'amountMinor' | 'currency'>,
    changes: Record<string, unknown> = {},
  ) {
    return {
      eventId: `pay.${order.id}`,
      kind: 'PAYMENT',
      paymentId: `mock.${order.id}`,
      orderId: order.id,
      amountMinor: order.amountMinor,
      currency: order.currency,
      ...changes,
    }
  }

  function notify(payload: object, now: number, signature?: string, provider = 'mock') {
    const body = JSON.stringify(payload)
    return runtime.dispatchFetch(origin + base + `/webhooks/${provider}`, {
      method: 'POST',
      headers: {
        'X-Order-Fixture': '1',
        'X-Order-Fixture-Time': String(now),
        'X-Molis-Payment-Signature':
          signature ?? createHmac('sha256', signing).update(body).digest('hex'),
      },
      body,
    })
  }

  function sweep(now: number) {
    return runtime.dispatchFetch(origin + base + '/webhooks/order-fixture-expire', {
      method: 'POST',
      headers: { 'X-Order-Fixture': '1', 'X-Order-Fixture-Time': String(now) },
    })
  }

  const count = (table: string) => db.prepare(`SELECT COUNT(*) AS n FROM ${table}`).first('n')
  const internalHeaders = { 'X-Molis-Internal-Secret': internal }
  return {
    call,
    create,
    pay,
    refund,
    event,
    notify,
    sweep,
    count,
    db,
    caller,
    runtime,
    internalHeaders,
    auditChecks,
    revokeAuditPermission: () => (auditPermission = false),
  }
}

async function data(response: Response | Promise<Response>, status = 200) {
  const result = await response
  const json = await result.json()
  assert.equal(result.headers.get('cache-control'), 'no-store')
  assert.equal(result.status, status, JSON.stringify(json))
  return json.data
}

async function error(response: Response | Promise<Response>, status: number, code: string) {
  const result = await response
  const json = await result.json()
  assert.equal(result.headers.get('cache-control'), 'no-store')
  assert.equal(result.status, status, JSON.stringify(json))
  assert.equal(json.error?.code, code)
}

if (process.env.BILLING_ORDER_TEST_NO_MOCK === '1') {
  test('the production artifact rejects Mock and fixture routes even with test bindings', async (t) => {
    const f = await setup(t)
    const fixtureTime = at('2000-01-31T02:00:00Z')
    await data(f.call('/catalog/plans', { now: fixtureTime }))
    const pending = await data(f.create({ now: fixtureTime }), 201)
    assert.notEqual(pending.createdAt, fixtureTime)
    assert.equal(pending.status, 'PENDING_PAYMENT')
    await error(f.pay(pending, { now: fixtureTime }), 404, 'NOT_FOUND')
    await error(f.notify(f.event(pending), fixtureTime), 404, 'NOT_FOUND')
    await error(f.sweep(fixtureTime), 404, 'NOT_FOUND')
    assert.equal((await data(f.call('/credits/balance', { now: fixtureTime }))).available, 0)
    assert.equal(await f.count('billing_payment_event'), 0)
    assert.equal(await f.count('billing_credit_ledger'), 0)
    assert.equal(await f.count('billing_order'), 1)
  })
} else {
  test('Billing validates browser-route inputs without the Auth proxy', async (t) => {
    const f = await setup(t)
    for (const path of [
      '/catalog/plans?unknown=WORK',
      '/subscriptions?product=WORK&product=WORK',
      '/credits/balance?limit=1',
      '/orders?ownerKind=TEAM',
      '/orders?ownerKind=TENANT',
      '/orders?ownerKind=TENANT&ownerId=invalid%2Fid',
      '/orders?limit=0',
      '/credits/ledger?cursor=invalid',
      '/credits/ledger?limit=1&limit=2',
    ]) {
      await error(f.call(path), 400, 'INVALID_REQUEST')
    }
    const create = { planId: personal, interval: 'MONTH' }
    for (const [path, scope, payload] of [
      ['/orders', { key: null }, create],
      ['/orders', { key: 'bad key' }, create],
      ['/orders', {}, { ...create, actorUserId: 'spoofed' }],
      ['/orders', {}, '{'],
      ['/orders', {}, '{"planId":"first","planId":"second","interval":"MONTH"}'],
      ['/orders/missing/mock-pay', { key: null }, {}],
      ['/orders/missing/refund', {}, { reason: 'extra field' }],
      ['/entitlements/check', {}, '[]'],
    ] as [string, Scope, object | string][]) {
      const response = await f.call(path, { ...scope, requestId: 'browser.input' }, 'POST', payload)
      assert.equal(response.headers.get('X-Request-Id'), 'browser.input')
      assert.equal((await response.clone().json()).requestId, 'browser.input')
      await error(response, 400, 'INVALID_REQUEST')
    }
    const response = await f.call('/orders', { requestId: 'wrong.method' }, 'PATCH')
    assert.equal((await response.clone().json()).requestId, 'wrong.method')
    await error(response, 404, 'NOT_FOUND')
    assert.equal(await f.count('billing_order'), 0)
  })

  test('platform audit pages real payment, refund and Credit writes with fresh permission checks', async (t) => {
    const f = await setup(t)
    const now = at('2027-06-12T07:30:00Z')
    const tenant = 'team-fixture'
    const first = await data(f.create({ now }), 201)
    await data(f.pay(first, { now, requestId: 'audit-payment' }))
    const group = await data(f.create({ now, tenant }), 201)
    await data(f.pay(group, { now, tenant }))
    const renewal = await data(f.create({ now, tenant }, 'YEAR'), 201)
    await data(f.pay(renewal, { now, tenant }))
    await data(f.refund(renewal, { now, tenant, requestId: 'audit-refund' }))
    await data(
      f.caller.fetch(origin + '/internal/v1/credits/consume', {
        method: 'POST',
        headers: {
          ...f.internalHeaders,
          'Content-Type': 'application/json',
          'Idempotency-Key': 'audit-usage',
          'X-Fixture-Now': String(now),
          'X-Request-Id': 'audit-usage',
        },
        body: JSON.stringify({
          callingService: 'work-fixture',
          actorUserId: 'user-fixture',
          product: 'WORK',
          owner: { kind: 'USER', id: 'user-fixture' },
          referenceId: 'audit-usage',
          occurredAt: now,
          amount: 3,
        }),
      }),
    )

    const headers = {
      ...f.internalHeaders,
      Authorization: 'Bearer platform-auditor',
      'X-Request-Id': 'audit-reader',
    }
    const call = (query = '', overrides = {}) =>
      f.caller.fetch(origin + auditPath + query, { headers: { ...headers, ...overrides } })

    await error(f.runtime.dispatchFetch(origin + auditPath, { headers }), 404, 'NOT_FOUND')
    await error(f.caller.fetch(origin + auditPath, { method: 'POST', headers }), 404, 'NOT_FOUND')
    await error(
      f.caller.fetch(origin + auditPath, { headers: f.internalHeaders }),
      401,
      'UNAUTHENTICATED',
    )
    await error(call('', { 'X-Molis-Internal-Secret': 'x'.repeat(43) }), 404, 'NOT_FOUND')
    assert.deepEqual(f.auditChecks, [])
    for (const [actor, status, code] of [
      ['admin-1', 403, 'FORBIDDEN'],
      ['member', 403, 'FORBIDDEN'],
      ['expired', 401, 'SESSION_EXPIRED'],
      ['bridge-missing', 503, 'AUTH_UNAVAILABLE'],
      ['session-only', 503, 'AUTH_UNAVAILABLE'],
    ] as const) {
      await error(
        call('', { Authorization: `Bearer ${actor}`, 'X-Platform-Permission': 'AUDIT_READ' }),
        status,
        code,
      )
    }
    for (const query of ['?limit=0', '?limit=101', '?limit=1&limit=2', '?cursor=invalid']) {
      await error(call(query), 400, 'INVALID_REQUEST')
    }

    const { results } = await f.db
      .prepare(
        `SELECT id, calling_service AS callingService, actor_user_id AS actorUserId,
        operation, target_id AS targetId, product, owner_kind AS ownerKind, owner_id AS ownerId,
        request_id AS requestId, details_json, created_at AS createdAt
        FROM billing_audit ORDER BY created_at DESC, id DESC`,
      )
      .all()
    const expected = results.map(({ details_json, ...row }) => ({
      ...row,
      details: JSON.parse(String(details_json)),
    }))
    assert.equal(expected.length, 10)
    assert.ok(expected.every(({ createdAt }) => createdAt === now))
    assert.deepEqual(await data(call()), { items: expected, nextCursor: null })
    const firstPage = await data(call('?limit=1'))
    assert.deepEqual(firstPage.items, expected.slice(0, 1))
    let cursor = firstPage.nextCursor
    const collected = [...firstPage.items]
    for (let index = 1; index < expected.length; index += 1) {
      assert.ok(cursor)
      const page = await data(call(`?limit=1&cursor=${encodeURIComponent(cursor)}`))
      assert.equal(page.items.length, 1)
      collected.push(...page.items)
      cursor = page.nextCursor
    }
    assert.equal(cursor, null)
    assert.deepEqual(collected, expected)
    assert.equal(new Set(collected.map(({ id }) => id)).size, expected.length)
    assert.ok(
      collected.some(
        (item) =>
          item.operation === 'order.pay' &&
          item.targetId === first.id &&
          item.requestId === 'audit-payment',
      ),
    )
    assert.ok(
      collected.some(
        (item) =>
          item.operation === 'order.refund' &&
          item.targetId === renewal.id &&
          item.requestId === 'audit-refund',
      ),
    )
    assert.ok(
      collected.some((item) => item.operation === 'CREDIT_GRANT' && item.actorUserId === null),
    )
    assert.ok(
      collected.some(
        (item) =>
          item.operation === 'CREDIT_CONSUME' &&
          item.actorUserId === 'user-fixture' &&
          item.callingService === 'work-fixture',
      ),
    )
    assert.equal(f.auditChecks.length, 19)
    assert.ok(f.auditChecks.every((id) => id === 'audit-reader'))

    f.revokeAuditPermission()
    await error(
      call(`?limit=1&cursor=${encodeURIComponent(firstPage.nextCursor)}`),
      403,
      'FORBIDDEN',
    )
    assert.equal(f.auditChecks.length, 20)
  })

  test('A02 personal month starts at payment and commits initial Credit atomically', async (t) => {
    const f = await setup(t)
    const now = at('2027-01-31T02:00:00Z')
    const paidAt = now + 20 * minute
    const pending = await data(f.create({ now, key: 'create' }), 201)
    assert.equal(pending.amountMinor, 1000)
    assert.equal(pending.monthlyCredits, 100)
    assert.equal(pending.currency, 'CNY')
    assert.equal(pending.status, 'PENDING_PAYMENT')
    assert.equal(pending.servicePeriod, null)
    assert.equal(pending.paymentExpiresAt, now + windowMs)
    assert.deepEqual(await data(f.call('/subscriptions', { now })), {
      product: 'WORK',
      ownerKind: 'USER',
      ownerId: 'user-fixture',
      billingTimeZone: null,
      currentPeriod: null,
      futurePeriod: null,
      paidThroughAt: null,
    })
    assert.equal(await f.count('billing_credit_period'), 0)
    assert.equal((await data(f.call('/credits/balance', { now }))).available, 0)

    await error(
      f.pay(pending, { now: paidAt, key: 'pay-once', requestId: 'fixture-fail-credit' }),
      503,
      'BILLING_UNAVAILABLE',
    )
    for (const table of [
      'billing_subscription',
      'billing_order_service_period',
      'billing_payment_event',
      'billing_credit_period',
      'billing_credit_ledger',
    ]) {
      assert.equal(await f.count(table), 0, table)
    }
    assert.equal(await f.count('billing_audit'), 1)
    assert.equal(await f.count('billing_idempotency_receipt'), 1)
    assert.equal(
      (await data(f.call(`/orders/${pending.id}`, { now: paidAt }))).status,
      'PENDING_PAYMENT',
    )

    const paid = await data(f.pay(pending, { now: paidAt, key: 'pay-once' }))
    const end = at('2027-02-28T02:20:00Z')
    assert.equal(paid.status, 'PAID')
    assert.equal(paid.paidAt, paidAt)
    assert.equal(paid.refundable, false)
    assert.deepEqual(paid.servicePeriod, {
      orderId: pending.id,
      startsAt: paidAt,
      endsAt: end,
      revokedAt: null,
    })
    assert.deepEqual(
      await f.db
        .prepare(
          `SELECT p.granted, p.available, p.starts_at, p.expires_at,
      l.kind, l.delta_granted, (a.request_id = payment_audit.request_id) AS shared_request
      FROM billing_credit_period p JOIN billing_credit_ledger l ON l.credit_period_id=p.id
      JOIN billing_audit a ON a.id=l.audit_id JOIN billing_payment_event e ON e.order_id=p.order_id
      JOIN billing_audit payment_audit ON payment_audit.id=e.audit_id`,
        )
        .first(),
      {
        granted: 100,
        available: 100,
        starts_at: paidAt,
        expires_at: end,
        kind: 'GRANT',
        delta_granted: 100,
        shared_request: 1,
      },
    )
    assert.equal((await data(f.call('/credits/balance', { now: paidAt }))).available, 100)
    const read = await data(f.call('/subscriptions', { now: paidAt }))
    assert.equal(read.paidThroughAt, end)
    assert.equal(read.currentPeriod.orderId, pending.id)
    assert.equal((await data(f.call('/subscriptions', { now: end }))).currentPeriod, null)
    await error(f.refund(pending, { now: paidAt }), 409, 'ORDER_NOT_REFUNDABLE')
  })

  test('A04 persisted quote, pagination, retry after re-login, expiry and concurrent signed events', async (t) => {
    const f = await setup(t)
    const now = at('2027-02-03T10:00:00Z')
    const pending = await data(f.create({ now, key: 'original' }), 201)
    const changed = await data(f.create({ now, key: 'new' }, 'YEAR'), 201)
    assert.notEqual(pending.id, changed.id)
    assert.equal(pending.paymentExpiresAt, changed.paymentExpiresAt)
    assert.deepEqual(
      await data(f.create({ now: now + minute, key: 'original', actor: 'new-session' }), 201),
      pending,
    )
    await error(f.create({ now, key: 'original' }, 'YEAR'), 409, 'IDEMPOTENCY_CONFLICT')
    const first = await data(f.call('/orders?limit=1', { now }))
    const second = await data(
      f.call(`/orders?limit=1&cursor=${encodeURIComponent(first.nextCursor)}`, { now }),
    )
    assert.notEqual(first.items[0].id, second.items[0].id)
    assert.equal(second.nextCursor, null)

    const justBefore = pending.paymentExpiresAt - 1
    await error(
      f.pay(pending, { now: justBefore, actor: 'expired', key: 'continue' }),
      401,
      'SESSION_EXPIRED',
    )
    await error(f.notify(f.event(pending), justBefore, '0'.repeat(64)), 403, 'FORBIDDEN')
    await error(
      f.notify(f.event(pending, { amountMinor: 999 }), justBefore),
      400,
      'INVALID_REQUEST',
    )
    await error(f.notify(f.event(pending, { currency: 'USD' }), justBefore), 400, 'INVALID_REQUEST')
    await error(
      f.notify(f.event(pending, { paymentId: 'mock.other' }), justBefore),
      400,
      'INVALID_REQUEST',
    )
    const results = await Promise.all([
      data(f.pay(pending, { now: justBefore, actor: 'new-session', key: 'continue' })),
      data(f.pay(pending, { now: justBefore, key: 'continue' })),
      data(f.notify(f.event(pending), justBefore)),
      data(f.notify(f.event(pending), justBefore)),
    ])
    for (const result of results) assert.deepEqual(result, results[0])
    assert.equal(results[0].paidAt, justBefore)
    assert.equal(await f.count('billing_payment_event'), 1)
    assert.equal(await f.count('billing_order_service_period'), 1)
    assert.equal(await f.count('billing_credit_ledger'), 1)
    assert.deepEqual(
      await data(f.notify(f.event(pending, { eventId: 'alternate-notice' }), justBefore)),
      results[0],
    )
    assert.equal(await f.count('billing_payment_event'), 2)
    assert.equal(await f.count('billing_order_service_period'), 1)
    await error(
      f.notify(f.event(pending, { amountMinor: 2000 }), justBefore),
      409,
      'IDEMPOTENCY_CONFLICT',
    )
    await error(
      f.pay(pending, { now: justBefore, actor: 'expired', key: 'continue' }),
      401,
      'SESSION_EXPIRED',
    )

    const deadline = changed.paymentExpiresAt
    assert.equal(
      (await data(f.call(`/orders/${changed.id}`, { now: deadline - 1 }))).status,
      'PENDING_PAYMENT',
    )
    assert.equal((await data(f.call(`/orders/${changed.id}`, { now: deadline }))).status, 'EXPIRED')
    assert.deepEqual(
      await f.db.prepare('SELECT status FROM billing_order WHERE id=?').bind(changed.id).first(),
      { status: 'PENDING_PAYMENT' },
    )
    await Promise.all([
      error(f.pay(changed, { now: deadline }), 409, 'ORDER_EXPIRED'),
      error(f.notify(f.event(changed), deadline), 409, 'ORDER_EXPIRED'),
      data(f.sweep(deadline)),
    ])
    assert.equal((await data(f.sweep(deadline))).expired, 0)
    assert.deepEqual(
      await f.db
        .prepare('SELECT status, paid_at FROM billing_order WHERE id=?')
        .bind(changed.id)
        .first(),
      { status: 'EXPIRED', paid_at: null },
    )
    assert.equal(
      await f.db
        .prepare("SELECT COUNT(*) n FROM billing_audit WHERE operation='order.expire'")
        .first('n'),
      1,
    )
    assert.deepEqual(await data(f.pay(pending, { now: deadline, key: 'continue' })), results[0])
    assert.equal((await data(f.call(`/orders/${pending.id}`, { now: deadline }))).status, 'PAID')
    const crossing = await data(f.create({ now: deadline }), 201)
    await error(
      f.pay(crossing, {
        now: crossing.paymentExpiresAt - 1,
        commitTime: crossing.paymentExpiresAt,
      }),
      409,
      'ORDER_EXPIRED',
    )
    assert.equal(
      (await data(f.call(`/orders/${crossing.id}`, { now: crossing.paymentExpiresAt }))).status,
      'EXPIRED',
    )
  })

  test('A05 two admins can pay only one future term, with a stable boundary and no advance grant', async (t) => {
    const f = await setup(t)
    const tenant = 'team-fixture'
    const start = at('2027-06-12T07:30:00Z')
    const now = at('2027-07-05T07:30:00Z')
    const startFuture = at('2027-07-12T07:30:00Z')
    const endFuture = at('2027-08-12T07:30:00Z')
    const first = await data(f.create({ now: start, tenant }), 201)
    await data(f.pay(first, { now: start, tenant }))

    const [a, b] = await Promise.all([
      data(f.create({ now, tenant, actor: 'admin-1' }), 201),
      data(f.create({ now, tenant, actor: 'admin-2' }), 201),
    ])
    const [pa, pb] = await Promise.all([
      f.pay(a, { now, tenant, actor: 'admin-1' }),
      f.pay(b, { now, tenant, actor: 'admin-2' }),
    ])
    assert.deepEqual([pa.status, pb.status].sort(), [200, 409])
    const won = await data(pa.ok ? pa : pb)
    const lost = pa.ok ? b : a
    await error(pa.ok ? pb : pa, 409, 'RENEWAL_ALREADY_PURCHASED')
    await error(f.notify(f.event(lost), now), 409, 'RENEWAL_ALREADY_PURCHASED')
    assert.equal(
      (await data(f.call(`/orders/${lost.id}`, { now, tenant }))).status,
      'PENDING_PAYMENT',
    )
    assert.deepEqual(won.servicePeriod, {
      orderId: won.id,
      startsAt: startFuture,
      endsAt: endFuture,
      revokedAt: null,
    })
    assert.equal(await f.count('billing_credit_period'), 1)
    assert.equal(await f.count('billing_payment_event'), 2)
    assert.equal(
      (await data(f.call('/credits/balance', { now: startFuture - 1, tenant }))).available,
      500,
    )
    assert.equal(await f.count('billing_credit_period'), 1)
    const subscription = await data(f.call('/subscriptions', { now, tenant }))
    assert.equal(subscription.paidThroughAt, endFuture)
    assert.equal(subscription.futurePeriod.orderId, won.id)
    assert.equal(subscription.currentPeriod.orderId, first.id)
    const members = await data(f.call('/subscriptions', { now, tenant, actor: 'member' }))
    assert.equal(Object.hasOwn(members.currentPeriod, 'orderId'), false)
    assert.equal(Object.hasOwn(members.futurePeriod, 'orderId'), false)
    await error(f.call(`/orders/${first.id}`, { now, tenant, actor: 'member' }), 403, 'FORBIDDEN')
    await error(f.call(`/orders/${first.id}`, { now }), 404, 'NOT_FOUND')
    assert.equal(
      (await data(f.call('/subscriptions', { now: startFuture, tenant }))).currentPeriod.orderId,
      won.id,
    )
    assert.equal(
      (await data(f.call('/subscriptions', { now: startFuture, tenant }))).futurePeriod,
      null,
    )
    const balance = () => data(f.call('/credits/balance', { now: startFuture, tenant }))
    const balances = await Promise.all([balance(), balance()])
    assert.deepEqual(
      balances.map(({ available }) => available),
      [500, 500],
    )
    assert.deepEqual(
      (
        await f.db
          .prepare(
            'SELECT order_id, status, available FROM billing_credit_period ORDER BY starts_at',
          )
          .all()
      ).results,
      [
        { order_id: first.id, status: 'EXPIRED', available: 0 },
        { order_id: won.id, status: 'ACTIVE', available: 500 },
      ],
    )
  })

  test('A06 interval changes, leap day, January anchor, year rollover and expiry reset', async (t) => {
    const f = await setup(t)
    const start = at('2028-01-31T02:05:04.123Z')
    const feb = at('2028-02-29T02:05:04.123Z')
    const febNextYear = at('2029-02-28T02:05:04.123Z')
    const marNextYear = at('2029-03-31T02:05:04.123Z')
    await error(f.create({ now: start }, 'MONTH', 'Somewhere/Invalid'), 400, 'INVALID_TIMEZONE')
    const first = await data(f.create({ now: start }), 201)
    assert.equal((await data(f.pay(first, { now: start }))).servicePeriod.endsAt, feb)

    const year = await data(f.create({ now: feb - 1 }, 'YEAR', 'America/New_York'), 201)
    const annual = await data(f.pay(year, { now: feb - 1 }))
    assert.equal(annual.servicePeriod.startsAt, feb)
    assert.equal(annual.servicePeriod.endsAt, febNextYear)
    const month = await data(f.create({ now: feb }, 'MONTH', 'America/New_York'), 201)
    const monthly = await data(f.pay(month, { now: feb }))
    assert.equal(monthly.servicePeriod.startsAt, febNextYear)
    assert.equal(monthly.servicePeriod.endsAt, marNextYear)
    assert.equal(
      (await data(f.call('/subscriptions', { now: feb }))).billingTimeZone,
      'Asia/Shanghai',
    )
    assert.equal(await f.count('billing_subscription'), 1)
    const reset = await data(f.create({ now: marNextYear - 1 }, 'MONTH', 'America/New_York'), 201)
    const repaid = await data(f.pay(reset, { now: marNextYear }))
    assert.equal(repaid.servicePeriod.startsAt, marNextYear)
    assert.equal(repaid.servicePeriod.endsAt, at('2029-05-01T02:05:04.123Z'))
    assert.equal(
      (await data(f.call('/subscriptions', { now: marNextYear }))).billingTimeZone,
      'America/New_York',
    )
    assert.equal(await f.count('billing_subscription'), 2)
  })

  test('A06 DST gaps and folds are resolved in the original time zone', async (t) => {
    const f = await setup(t)
    for (const [tenant, start, end] of [
      ['gap', at('2027-02-14T07:30:00.125Z'), at('2027-03-14T07:30:00.125Z')],
      ['fold', at('2027-10-07T05:30:00.125Z'), at('2027-11-07T05:30:00.125Z')],
      ['year-rollover', at('2027-12-31T17:15:00.125Z'), at('2028-01-31T17:15:00.125Z')],
    ] as const) {
      const first = await data(f.create({ now: start, tenant }, 'MONTH', 'America/New_York'), 201)
      assert.equal(
        (await data(f.pay(first, { now: start, tenant }))).servicePeriod.endsAt,
        end,
        tenant,
      )
      assert.equal(
        (await data(f.call('/subscriptions', { now: end - 1, tenant }))).paidThroughAt,
        end,
      )
      assert.equal((await data(f.call('/subscriptions', { now: end, tenant }))).currentPeriod, null)
    }
  })

  test('A07 whole future refund shifts paid-through back, with strict start cutoff and stable event replay', async (t) => {
    const f = await setup(t)
    const tenant = 'team-fixture'
    const start = at('2027-06-12T07:30:00Z')
    const now = at('2027-07-05T07:30:00Z')
    const cutoff = at('2027-07-12T07:30:00Z')
    const first = await data(f.create({ now: start, tenant }), 201)
    await data(f.pay(first, { now: start, tenant }))
    const next = await data(f.create({ now, tenant }, 'YEAR', 'America/New_York'), 201)
    await data(f.pay(next, { now, tenant }))
    const sh = await data(
      f.call(`/orders/${next.id}`, { tenant, now: cutoff - 1, actor: 'admin-sh' }),
    )
    const ny = await data(
      f.call(`/orders/${next.id}`, { tenant, now: cutoff - 1, actor: 'admin-ny' }),
    )
    assert.equal(sh.servicePeriod.startsAt, cutoff)
    assert.equal(ny.servicePeriod.startsAt, cutoff)
    assert.equal(sh.refundable, true)
    const refund = () =>
      data(f.refund(next, { tenant, now: cutoff - 1, key: 'auto-refund', actor: 'admin-2' }))
    const [refunded, duplicate] = await Promise.all([refund(), refund()])
    assert.deepEqual(duplicate, refunded)
    assert.equal(refunded.status, 'REFUNDED')
    assert.equal(refunded.amountMinor, 30000)
    assert.equal(refunded.refundedAt, cutoff - 1)
    assert.equal(refunded.servicePeriod.revokedAt, cutoff - 1)
    assert.deepEqual(
      await data(f.refund(next, { tenant, now: cutoff, key: 'auto-refund', actor: 'admin-2' })),
      refunded,
    )
    await error(f.refund(next, { tenant, now: cutoff - 1 }), 409, 'ORDER_NOT_REFUNDABLE')
    assert.deepEqual(
      await data(f.notify(f.event(next, { eventId: `refund.${next.id}`, kind: 'REFUND' }), cutoff)),
      refunded,
    )
    assert.equal(
      (await data(f.call('/subscriptions', { tenant, now: cutoff - 1 }))).paidThroughAt,
      cutoff,
    )
    assert.equal(
      (await data(f.call('/subscriptions', { tenant, now: cutoff - 1 }))).futurePeriod,
      null,
    )
    assert.equal(
      (await data(f.call(`/orders/${first.id}`, { tenant, now: cutoff - 1 }))).status,
      'PAID',
    )
    assert.equal(
      await f.db
        .prepare('SELECT available FROM billing_credit_period WHERE order_id=?')
        .bind(first.id)
        .first('available'),
      500,
    )
    assert.equal(
      await f.db
        .prepare('SELECT COUNT(*) n FROM billing_credit_period WHERE order_id=?')
        .bind(next.id)
        .first('n'),
      0,
    )
    const replacement = await data(f.create({ now: cutoff - 1, tenant }), 201)
    assert.equal(
      (await data(f.pay(replacement, { now: cutoff - 1, tenant }))).servicePeriod.startsAt,
      cutoff,
    )
    await error(
      f.refund(replacement, { tenant, now: cutoff - 1, commitTime: cutoff }),
      409,
      'ORDER_NOT_REFUNDABLE',
    )
    await error(f.refund(replacement, { now: cutoff, tenant }), 409, 'ORDER_NOT_REFUNDABLE')
    assert.equal(
      (await data(f.call('/subscriptions', { now: cutoff, tenant }))).currentPeriod.orderId,
      replacement.id,
    )
    const unused = await data(f.create({ now: start }, 'YEAR'), 201)
    await data(f.pay(unused, { now: start }))
    await error(f.refund(unused, { now: start + 1 }), 409, 'ORDER_NOT_REFUNDABLE')
  })

  test('A08 latest checkout quote and frozen pending/paid versions survive plan retirement', async (t) => {
    const f = await setup(t)
    const start = at('2027-06-12T07:30:00Z')
    const pending = await data(f.create({ now: start }), 201)
    await f.db.batch([
      f.db
        .prepare(
          `INSERT INTO billing_plan_version VALUES
        (?, 2, 'WORK', 'USER', '新版', 'CNY', 1200, 12000, 125, ?)`,
        )
        .bind(personal, start + 1),
      f.db.prepare('UPDATE billing_plan SET current_version=2 WHERE plan_id=?').bind(personal),
    ])
    const now = start + minute
    const next = await data(f.create({ now }, 'YEAR'), 201)
    assert.deepEqual([next.planVersion, next.amountMinor, next.monthlyCredits], [2, 12000, 125])
    assert.deepEqual(
      [(await data(f.call('/catalog/plans', { now }))).items[0].planVersion, pending.planVersion],
      [2, 1],
    )
    const oldPaid = await data(f.pay(pending, { now }))
    assert.deepEqual(
      [oldPaid.planVersion, oldPaid.amountMinor, oldPaid.monthlyCredits],
      [1, 1000, 100],
    )
    const newestRenewal = await data(f.create({ now: now + 1 }), 201)
    assert.deepEqual(
      [newestRenewal.planVersion, newestRenewal.amountMinor, newestRenewal.monthlyCredits],
      [2, 1200, 125],
    )
    await f.db
      .prepare("UPDATE billing_plan SET sale_status='DISCONTINUED' WHERE plan_id=?")
      .bind(personal)
      .run()
    await error(f.create({ now }), 409, 'PLAN_DISCONTINUED')
    const newPaid = await data(f.pay(next, { now: now + 1 }))
    assert.deepEqual(
      [newPaid.planVersion, newPaid.amountMinor, newPaid.monthlyCredits],
      [2, 12000, 125],
    )
    const subscription = await data(f.call('/subscriptions', { now: now + 1 }))
    assert.equal(subscription.currentPeriod.monthlyCredits, 100)
    assert.equal(subscription.futurePeriod.monthlyCredits, 125)
    assert.equal(subscription.futurePeriod.planName, '新版')
    await error(f.create({ now: newPaid.servicePeriod.endsAt }), 409, 'PLAN_DISCONTINUED')
    assert.equal(
      (await data(f.call('/subscriptions', { now: newPaid.servicePeriod.startsAt }))).currentPeriod
        .planVersion,
      2,
    )
    assert.equal(
      (await data(f.call('/credits/balance', { now: newPaid.servicePeriod.startsAt }))).available,
      125,
    )
  })

  test('live service clock reaches D1, and unsigned or misconfigured providers fail closed', async (t) => {
    const f = await setup(t)
    const pending = await data(f.create({}), 201)
    const paid = await data(f.pay(pending, {}))
    assert.ok(paid.paidAt >= pending.createdAt)
    const unconfigured = await setup(t, 'test', false)
    const other = await data(unconfigured.create({}), 201)
    await error(
      unconfigured.notify(unconfigured.event(other), Date.now()),
      503,
      'BILLING_UNAVAILABLE',
    )
    const prod = await setup(t, 'prod')
    const prodOrder = await data(prod.create({}), 201)
    await error(prod.pay(prodOrder, {}), 404, 'NOT_FOUND')
    await error(prod.notify(prod.event(prodOrder), Date.now()), 404, 'NOT_FOUND')
  })
}
