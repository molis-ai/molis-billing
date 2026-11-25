import assert from 'node:assert/strict'
import { randomBytes } from 'node:crypto'
import { test, type TestContext } from 'node:test'
import { startLocalRuntime } from '../scripts/local-runtime.mjs'
import { applySchema, seedPlans } from './d1-fixture.ts'

const origin = 'https://billing.internal'
const internal = '/internal/v1/credits'
const user = '/api/v1/credits'
const checkout = '/fixture/credits/pay'
const cleanup = '/fixture/credits/cleanup'
const secret = randomBytes(32).toString('base64url')
const first = Date.UTC(2027, 0, 31, 14)
const next = Date.UTC(2027, 1, 28, 14)
const end = Date.UTC(2028, 0, 31, 14)
const team = { kind: 'TENANT', id: 'team-1' }
const teammate = 'user-2'
const self = { kind: 'USER', id: 'user-1' }
const teamOther = { kind: 'TENANT', id: 'team-2' }

const credentials = { 'X-Molis-Internal-Secret': secret }
const query = (owner = team) => '?ownerKind=' + owner.kind + '&ownerId=' + owner.id

function auth(request: Request) {
  const token = request.headers.get('Authorization')
  assert.equal(new URL(request.url).pathname, '/internal/v1/subjects/verify')
  assert.equal(request.headers.get('X-Molis-Internal-Secret'), secret)
  if (token === 'Bearer left')
    return Response.json({ error: { code: 'FORBIDDEN' } }, { status: 403 })
  return request.json().then(({ product, owner }) =>
    Response.json({
      data: {
        userId: 'user-1',
        product,
        owner: { kind: owner.kind, id: owner.id ?? 'user-1' },
        role: owner.kind === 'USER' ? 'SELF' : token === 'Bearer member' ? 'MEMBER' : 'ADMIN',
      },
    }),
  )
}

function usage(referenceId: string, occurredAt: number, overrides = {}) {
  return {
    callingService: 'work-fixture',
    actorUserId: 'user-1',
    product: 'WORK',
    owner: team,
    referenceId,
    occurredAt,
    ...overrides,
  }
}

async function testFixture(t: TestContext) {
  const runtime = startLocalRuntime({
    triggerHandlers: true,
    bindings: {
      MOLIS_ENVIRONMENT: 'test',
      MOLIS_INTERNAL_SECRET: secret,
      BILLING_CREDIT_FIXTURE: '1',
    },
    auth,
  })
  t.after(() => runtime.dispose())
  const db = await runtime.getD1Database('DB', 'billing')
  const caller = await runtime.getWorker('caller')
  await applySchema(db)
  await seedPlans(db)
  async function pending(id: string, owner = team, paidAt = first) {
    const personal = owner.kind === 'USER'
    const planId = personal ? 'work-personal-test' : 'work-team-test'
    const monthlyCredits = personal ? 100 : 500
    await db
      .prepare(
        `INSERT INTO billing_order
         (id, product, owner_kind, owner_id, actor_user_id, plan_id, plan_version,
          interval, currency, amount_minor, monthly_credits, billing_time_zone, status, created_at, payment_expires_at)
       VALUES (?, 'WORK', ?, ?, 'user-1', ?, 1, 'YEAR', 'CNY', ?, ?, 'Etc/UTC', 'PENDING_PAYMENT', ?, ?)`,
      )
      .bind(
        id,
        owner.kind,
        owner.id,
        planId,
        personal ? 10000 : 30000,
        monthlyCredits,
        paidAt - 1,
        paidAt + 1_799_999,
      )
      .run()
    return { orderId: id, owner, planId, monthlyCredits, interval: 'YEAR', rejectAfterGrant: false }
  }
  const call = async (
    path: string,
    at: number,
    body?: object,
    key = 'fixture-key',
    actor?: string,
  ) => {
    const headers: Record<string, string> = {
      ...credentials,
      'X-Fixture-Now': String(at),
      'X-Request-Id': key,
    }
    if (actor) headers.Authorization = 'Bearer ' + actor
    if (body) {
      headers['Idempotency-Key'] = key
      headers['Content-Type'] = 'application/json'
    }
    const response = await caller.fetch(origin + path, {
      method: body ? 'POST' : 'GET',
      headers,
      body: body && JSON.stringify(body),
    })
    assert.equal(response.headers.get('Cache-Control'), 'no-store')
    const json = await response.json()
    assert.equal(JSON.stringify(json).includes(secret), false)
    return { status: response.status, ...json }
  }
  const buy = async (id: string, owner = team) => {
    const payment = await pending(id, owner)
    assert.equal((await call(checkout, first, payment)).status, 200)
    return payment
  }
  const balance = (at: number, actor = 'admin', owner = team) =>
    call(user + '/balance' + query(owner), at, undefined, 'balance', actor)
  const ledger = (at: number, actor = 'admin', owner = team, page = '') =>
    call(user + '/ledger' + query(owner) + page, at, undefined, 'ledger', actor)
  const rows = async (sql: string) => (await db.prepare(sql).all()).results
  return { runtime, caller, db, call, pending, buy, balance, ledger, rows }
}

test('A03 first month is atomic with payment and concurrent turnover has one grant and expiry', async (t) => {
  const { call, pending, balance, rows } = await testFixture(t)
  const payment = await pending('year-team')
  assert.equal((await balance(first - 1)).data.available, 0)
  const rejected = await call(checkout, first, { ...payment, rejectAfterGrant: true })
  assert.equal(rejected.error.code, 'VERSION_CONFLICT')
  assert.deepEqual(await rows('SELECT id FROM billing_credit_period'), [])
  assert.deepEqual(await rows('SELECT order_id FROM billing_order_service_period'), [])
  assert.deepEqual(await rows('SELECT id FROM billing_audit'), [])
  assert.deepEqual(await rows('SELECT status, amount_minor FROM billing_order'), [
    { status: 'PENDING_PAYMENT', amount_minor: 30000 },
  ])
  assert.equal((await call(checkout, first, payment)).data.endsAt, end)
  assert.equal((await call(checkout, first, payment)).status, 200)
  assert.deepEqual((await balance(first)).data.currentCreditPeriod, {
    startsAt: first,
    expiresAt: next,
  })
  assert.equal((await balance(first)).data.available, 500)
  assert.equal(
    (
      await call(
        internal + '/holds',
        first + 1,
        { ...usage('job', first + 1), amount: 20, expiresAt: next + 100 },
        'hold',
      )
    ).status,
    201,
  )
  const concurrent = await Promise.all([
    ...Array.from({ length: 8 }, () => balance(next)),
    ...Array.from({ length: 2 }, () => call(cleanup, next)),
  ])
  for (const result of concurrent) {
    assert.equal(result.status, 200, JSON.stringify(result))
    if (result.data.available !== undefined) assert.equal(result.data.available, 500)
  }
  assert.equal((await call(cleanup, next)).status, 200)
  assert.deepEqual(
    await rows(
      'SELECT month_index,status,available,reserved,expired FROM billing_credit_period ORDER BY month_index',
    ),
    [
      { month_index: 0, status: 'EXPIRED', available: 0, reserved: 0, expired: 500 },
      { month_index: 1, status: 'ACTIVE', available: 500, reserved: 0, expired: 0 },
    ],
  )
  assert.deepEqual(
    await rows('SELECT kind, count(*) AS n FROM billing_credit_ledger GROUP BY kind ORDER BY kind'),
    [
      { kind: 'EXPIRE', n: 1 },
      { kind: 'GRANT', n: 2 },
      { kind: 'HOLD', n: 1 },
    ],
  )
  const fourth = Date.UTC(2027, 3, 30, 14)
  assert.equal((await call(cleanup, fourth)).status, 200)
  assert.equal((await balance(fourth)).data.available, 500)
  assert.deepEqual(
    await rows('SELECT month_index,starts_at FROM billing_credit_period ORDER BY month_index'),
    [
      { month_index: 0, starts_at: first },
      { month_index: 1, starts_at: next },
      { month_index: 2, starts_at: Date.UTC(2027, 2, 31, 14) },
      { month_index: 3, starts_at: fourth },
    ],
  )
})

test('A09 private credits reject user credentials and member ledger scope precedes pagination', async (t) => {
  const { runtime, caller, call, buy, balance, ledger } = await testFixture(t)
  await buy('team')
  await buy('personal', self)
  await buy('other-team', teamOther)
  const at = first + 31
  const consume = { ...usage('mine', at), amount: 4 }
  for (const [fetch, headers] of [
    [runtime.dispatchFetch.bind(runtime), { ...credentials, Authorization: 'Bearer member' }],
    [runtime.dispatchFetch.bind(runtime), credentials],
    [caller.fetch.bind(caller), { Authorization: 'Bearer member' }],
    [caller.fetch.bind(caller), { ...credentials, Authorization: 'Bearer member' }],
    [caller.fetch.bind(caller), { ...credentials, Cookie: 'session=fixture' }],
  ] as const) {
    const response = await fetch(origin + internal + '/consume', {
      method: 'POST',
      headers,
      body: JSON.stringify(consume),
    })
    assert.equal(response.status, 404)
    assert.equal((await response.json()).error.code, 'NOT_FOUND')
  }
  const own = await call(internal + '/consume', at, consume, 'mine')
  const mate = await call(
    internal + '/consume',
    at,
    { ...usage('theirs', at, { actorUserId: teammate }), amount: 3 },
    'theirs',
  )
  const another = await call(
    internal + '/consume',
    at,
    { ...usage('another', at, { owner: teamOther }), amount: 8 },
    'another',
  )
  const personal = await call(
    internal + '/consume',
    at,
    { ...usage('personal', at, { owner: self }), amount: 2 },
    'personal',
  )
  for (const result of [own, mate, another, personal])
    assert.equal(result.status, 200, JSON.stringify(result))
  const hold = await call(
    internal + '/holds',
    at,
    { ...usage('my-hold', at), amount: 5, expiresAt: at + 100 },
    'my-hold',
  )
  assert.equal(hold.status, 201)
  for (const change of [
    { actorUserId: teammate },
    { callingService: 'other-service' },
    { referenceId: 'other-job' },
    { owner: teamOther },
  ]) {
    const bad = await call(
      internal + '/holds/' + hold.data.hold.id + '/release',
      at,
      { ...usage('my-hold', at), ...change },
      'other-task',
    )
    assert.equal(bad.error.code, 'NOT_FOUND')
  }
  assert.equal(
    (
      await call(
        internal + '/consume',
        at,
        { ...usage('impersonate', at, { owner: self, actorUserId: teammate }), amount: 1 },
        'impersonate',
      )
    ).error.code,
    'FORBIDDEN',
  )
  assert.equal((await balance(at, 'member')).data.available, 488)
  assert.equal((await balance(at, 'admin', teamOther)).data.available, 492)
  assert.equal((await balance(at, 'admin', self)).data.available, 98)
  assert.equal((await balance(at, 'left')).error.code, 'FORBIDDEN')
  for (const [path, body] of [
    ['/api/v1/orders' + query(), undefined],
    ['/api/v1/orders/team/refund' + query(), {}],
  ] as const) {
    assert.equal((await call(path, at, body, 'orders', 'member')).error.code, 'FORBIDDEN')
  }
  const page = await ledger(at, 'member', team, '&limit=1')
  assert.equal(page.status, 200)
  assert.equal(page.data.items.length, 1)
  assert.ok(page.data.nextCursor)
  const final = await ledger(
    at,
    'member',
    team,
    '&limit=1&cursor=' + encodeURIComponent(page.data.nextCursor),
  )
  const items = [...page.data.items, ...final.data.items]
  assert.deepEqual(items.map((item) => item.kind).sort(), ['CONSUME', 'HOLD'])
  assert.ok(
    items.every((item) => item.actorUserId === 'user-1' && item.callingService === 'work-fixture'),
  )
  assert.equal(final.data.nextCursor, null)
  assert.deepEqual((await ledger(at)).data.items.map((item) => item.kind).sort(), [
    'CONSUME',
    'CONSUME',
    'GRANT',
    'HOLD',
  ])
  assert.equal((await balance(next, 'member')).status, 200)
  assert.ok((await ledger(next)).data.items.some(({ kind }) => kind === 'EXPIRE'))
  const afterTurnover = (await ledger(next, 'member')).data.items
  assert.deepEqual(afterTurnover.map(({ kind }) => kind).sort(), ['CONSUME', 'HOLD', 'RELEASE'])
  assert.ok(afterTurnover.every(({ actorUserId }) => actorUserId === 'user-1'))
})

test('A10 archived old segment survives new-period 402, retries and receipt cleanup', async (t) => {
  const { call, buy, rows } = await testFixture(t)
  await buy('year-team')
  const before = next - 1
  const after = next + 1
  const oldHold = await call(
    internal + '/holds',
    before,
    {
      ...usage('cross', before),
      amount: 20,
      expiresAt: next + 10_000,
    },
    'hold-cross',
  )
  assert.equal(oldHold.status, 201)
  const blocker = await call(
    internal + '/holds',
    after,
    {
      ...usage('blocker', after),
      amount: 495,
      expiresAt: next + 10_000,
    },
    'hold-blocker',
  )
  assert.equal(blocker.status, 201)
  const url = internal + '/holds/' + oldHold.data.hold.id + '/settle'
  const segments = [
    { occurredAt: before, amount: 4 },
    { occurredAt: after, amount: 6 },
  ]
  const request = { ...usage('cross', after), segments }
  const failed = await call(url, after, request, 'cross')
  assert.equal(failed.status, 402, JSON.stringify(failed))
  assert.equal(failed.error.code, 'INSUFFICIENT_CREDITS')
  assert.equal(failed.data.settledSegments.length, 1)
  const [oldSegment] = failed.data.settledSegments
  assert.deepEqual(oldSegment, { ...segments[0], ledgerId: oldSegment.ledgerId })
  assert.ok(oldSegment.ledgerId)
  assert.deepEqual(failed.data.failedSegment, segments[1])
  const audit = await rows(
    `SELECT kind,delta_consumed,delta_expired FROM billing_credit_ledger WHERE id='${oldSegment.ledgerId}'`,
  )
  assert.deepEqual(audit, [{ kind: 'SETTLE', delta_consumed: 4, delta_expired: -4 }])
  assert.deepEqual(await call(url, after, request, 'cross'), failed)
  const different = await call(
    url,
    after,
    { ...request, segments: [segments[0], { ...segments[1], amount: 5 }] },
    'cross',
  )
  assert.equal(different.error.code, 'IDEMPOTENCY_CONFLICT')
  assert.equal(
    (
      await call(
        internal + '/holds/' + blocker.data.hold.id + '/release',
        after,
        usage('blocker', after),
        'release-blocker',
      )
    ).status,
    200,
  )
  const [complete, concurrent] = await Promise.all([
    call(url, after, request, 'cross'),
    call(url, after, request, 'cross'),
  ])
  assert.equal(complete.status, 200, JSON.stringify(complete))
  assert.deepEqual(complete, concurrent)
  assert.deepEqual(complete.data.settledSegments[0], oldSegment)
  const later = after + 172_800_000
  assert.equal((await call(cleanup, later)).status, 200)
  assert.deepEqual(await rows('SELECT idempotency_key FROM billing_idempotency_receipt'), [])
  assert.deepEqual(await call(url, later, request, 'cross'), complete)
  assert.deepEqual(
    await rows(
      'SELECT month_index,available,reserved,consumed,expired FROM billing_credit_period ORDER BY month_index',
    ),
    [
      { month_index: 0, available: 0, reserved: 0, consumed: 4, expired: 496 },
      { month_index: 1, available: 494, reserved: 0, consumed: 6, expired: 0 },
    ],
  )
  assert.deepEqual(
    await rows(
      "SELECT occurred_at,delta_consumed FROM billing_credit_ledger WHERE kind IN ('SETTLE','CONSUME') ORDER BY occurred_at",
    ),
    [
      { occurred_at: before, delta_consumed: 4 },
      { occurred_at: after, delta_consumed: 6 },
    ],
  )
})

test('A10 commits before archive, then handles partial release, timeouts and racing direct usage', async (t) => {
  const { call, buy, pending, balance, rows } = await testFixture(t)
  await buy('year-team')
  const before = next - 1
  const after = next + 1
  const endpoint = internal + '/holds'
  const held = await call(
    endpoint,
    before,
    { ...usage('order-cross', before), amount: 20, expiresAt: next + 100 },
    'order-cross',
  )
  const cross = await call(
    endpoint + '/' + held.data.hold.id + '/settle',
    after,
    {
      ...usage('order-cross', after),
      segments: [
        { occurredAt: before, amount: 4 },
        { occurredAt: after, amount: 6 },
      ],
    },
    'settle-cross',
  )
  assert.equal(cross.status, 200, JSON.stringify(cross))
  assert.deepEqual(
    await rows(
      "SELECT kind,delta_reserved FROM billing_credit_ledger WHERE kind IN ('SETTLE','EXPIRE') ORDER BY occurred_at",
    ),
    [
      { kind: 'SETTLE', delta_reserved: -4 },
      { kind: 'EXPIRE', delta_reserved: -16 },
    ],
  )
  const partial = await call(
    endpoint,
    after,
    { ...usage('partial', after), amount: 15, expiresAt: after + 100 },
    'hold-partial',
  )
  const path = endpoint + '/' + partial.data.hold.id
  const settled = await call(
    path + '/settle',
    after,
    { ...usage('partial', after), segments: [{ occurredAt: after, amount: 4 }] },
    'partial',
  )
  assert.equal(settled.data.hold.reserved, 11)
  const release = await call(path + '/release', after, usage('partial', after), 'release')
  assert.equal(release.data.hold.released, 11)
  assert.deepEqual(
    await call(path + '/release', after, usage('partial', after), 'release'),
    release,
  )
  const deadline = after + 1_800_000
  const timeoutPayload = { ...usage('timeout', after), amount: 9, expiresAt: deadline }
  const timeout = await call(endpoint, after, timeoutPayload, 'hold-timeout')
  assert.equal(timeout.status, 201)
  await pending('pending-order', team, after)
  for (let index = 0; index < 2; index++) assert.equal((await call(cleanup, deadline)).status, 200)
  assert.deepEqual(await call(endpoint, deadline, timeoutPayload, 'hold-timeout'), timeout)
  assert.equal((await balance(deadline)).data.available, 490)
  assert.deepEqual(
    await rows(
      "SELECT status,reserved,released FROM billing_credit_hold WHERE reference_id='timeout'",
    ),
    [{ status: 'EXPIRED', reserved: 0, released: 9 }],
  )
  assert.deepEqual(
    await rows(
      "SELECT operation,count(*) AS n FROM billing_audit WHERE operation IN ('ORDER_EXPIRE','CREDIT_HOLD_TIMEOUT') GROUP BY operation ORDER BY operation",
    ),
    [
      { operation: 'CREDIT_HOLD_TIMEOUT', n: 1 },
      { operation: 'ORDER_EXPIRE', n: 1 },
    ],
  )
  const consume = internal + '/consume'
  const result = await Promise.all([
    call(consume, deadline, { ...usage('a', deadline), amount: 490 }, 'race-a'),
    call(consume, deadline, { ...usage('b', deadline), amount: 490 }, 'race-b'),
  ])
  assert.deepEqual(result.map(({ status }) => status).sort(), [200, 402])
  assert.equal((await balance(deadline)).data.available, 0)
  const difference = await rows(
    `SELECT p.id FROM billing_credit_period p JOIN billing_credit_ledger l ON l.credit_period_id=p.id
     GROUP BY p.id HAVING p.granted<>SUM(l.delta_granted) OR p.available<>SUM(l.delta_available)
        OR p.reserved<>SUM(l.delta_reserved) OR p.consumed<>SUM(l.delta_consumed) OR p.expired<>SUM(l.delta_expired)`,
  )
  assert.deepEqual(difference, [])
})

test('scheduled Worker entrypoint processes due orders without the credit fixture route', async (t) => {
  const { runtime, pending, rows } = await testFixture(t)
  await pending('cron-order', team, Date.now() - 1_800_000)
  const url = new URL('/cdn-cgi/local/scheduled', origin)
  url.searchParams.set('cron', '*/5 * * * *')
  for (let index = 0; index < 2; index++) {
    const response = await runtime.dispatchFetch(url)
    assert.equal(response.status, 200, await response.text())
  }
  assert.deepEqual(await rows("SELECT status FROM billing_order WHERE id='cron-order'"), [
    { status: 'EXPIRED' },
  ])
  assert.deepEqual(await rows("SELECT operation FROM billing_audit WHERE target_id='cron-order'"), [
    { operation: 'ORDER_EXPIRE' },
  ])
})
