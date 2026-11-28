import assert from 'node:assert/strict'
import { randomBytes } from 'node:crypto'
import { test } from 'node:test'
import { startLocalRuntime } from '../scripts/local-runtime.mjs'
import { authFetch } from '../rust/src/runtime/foundation.js'
import { applySchema, seedPlans } from './d1-fixture.ts'

const origin = 'https://billing.internal'
const secret = randomBytes(32).toString('base64url')
const header = 'X-Molis-Internal-Secret'
const personal = 'work-personal-test'
const team = 'work-team-test'
const now = Date.UTC(2027, 0, 31, 14, 15, 16, 123)

async function assertError(response: Response, status: number, code: string) {
  assert.equal(response.status, status)
  assert.equal(response.headers.get('Cache-Control'), 'no-store')
  const text = await response.text()
  assert.equal(JSON.parse(text).error.code, code)
  assert.equal(text.includes(secret), false)
}

function authFixture() {
  const calls: { path: string; requestId: string | null }[] = []
  const fetch = async (request: Request) => {
    const url = new URL(request.url)
    const authorization = request.headers.get('Authorization')
    calls.push({ path: url.pathname, requestId: request.headers.get('X-Request-Id') })
    if (
      url.origin !== 'https://auth.internal' ||
      request.method !== 'POST' ||
      request.headers.get(header) !== secret ||
      request.headers.has('Cookie')
    )
      return Response.json({ error: { code: 'FORBIDDEN' } }, { status: 403 })
    if (authorization === 'Bearer expired')
      return Response.json({ error: { code: 'SESSION_REVOKED', message: secret } }, { status: 401 })
    if (authorization === 'Bearer disabled')
      return Response.json({ error: { code: 'TENANT_DISABLED' } }, { status: 403 })
    if (authorization === 'Bearer redirect') return Response.redirect('https://invalid.example/')
    if (authorization === 'Bearer broken') return new Response('invalid')
    if (authorization === 'Bearer unauthorized')
      return Response.json({ error: { code: 'UNAUTHENTICATED' } }, { status: 401 })
    if (authorization === 'Bearer left')
      return Response.json({ error: { code: 'FORBIDDEN' } }, { status: 403 })
    const { product, owner } = await request.json()
    if (product !== 'WORK' || !owner)
      return Response.json({ error: { code: 'INVALID_REQUEST' } }, { status: 400 })
    if (owner.kind === 'USER' && owner.id !== undefined && owner.id !== 'user-1')
      return Response.json({ error: { code: 'FORBIDDEN' } }, { status: 403 })
    return Response.json({
      data: {
        userId: 'user-1',
        product,
        owner: {
          kind: owner.kind,
          id: authorization === 'Bearer wrong-subject' ? 'team-other' : (owner.id ?? 'user-1'),
        },
        role:
          owner.kind === 'USER' ? 'SELF' : authorization === 'Bearer member' ? 'MEMBER' : 'ADMIN',
      },
    })
  }
  return { fetch, calls }
}

function billingHeaders(credential = 'valid') {
  return { [header]: secret, Authorization: `Bearer ${credential}`, 'X-Request-Id': 'fixture-1' }
}

const catalogPath = '/api/v1/catalog/plans'
const teamQuery = '?ownerKind=TENANT&ownerId=team-1'

test('A01 single baseline cold start, repeat initialization and exact order snapshots', async (t) => {
  for (const existing of [false, true]) {
    await t.test(existing ? 'pre-seeded baseline' : 'fresh D1', async (context) => {
      const fixture = authFixture()
      const runtime = startLocalRuntime({
        bindings: { MOLIS_ENVIRONMENT: 'test', MOLIS_INTERNAL_SECRET: secret },
        auth: fixture.fetch,
      })
      context.after(() => runtime.dispose())
      const caller = await runtime.getWorker('caller')
      const db = await runtime.getD1Database('DB', 'billing')
      await applySchema(db)
      if (existing) await seedPlans(db)
      await seedPlans(db)
      await seedPlans(db)
      assert.equal(await db.prepare('SELECT COUNT(*) AS n FROM billing_plan').first('n'), 2)
      assert.equal(await db.prepare('SELECT COUNT(*) AS n FROM billing_plan_version').first('n'), 2)

      for (const [kind, planId, scope, prices, credits] of [
        ['USER', personal, '', [1000, 10000], 100],
        ['TENANT', team, teamQuery, [3000, 30000], 500],
      ] as const) {
        const response = await caller.fetch(origin + catalogPath + scope, {
          headers: billingHeaders(),
        })
        assert.equal(response.status, 200)
        assert.equal(response.headers.get('Cache-Control'), 'no-store')
        const { data } = await response.json()
        assert.equal(data.items.length, 1)
        const [quote] = data.items
        assert.equal(quote.planId, planId)
        assert.equal(quote.planVersion, 1)
        assert.equal(quote.ownerKind, kind)
        assert.equal(quote.currency, 'CNY')
        assert.equal(quote.monthlyCredits, credits)
        assert.deepEqual([quote.monthlyPriceMinor, quote.yearlyPriceMinor], prices)

        for (const [interval, amount] of [
          ['MONTH', quote.monthlyPriceMinor],
          ['YEAR', quote.yearlyPriceMinor],
        ]) {
          const id = `${kind}-${interval}`
          await db
            .prepare(
              `INSERT INTO billing_order
                 (id, product, owner_kind, owner_id, actor_user_id, plan_id, plan_version,
                  interval, currency, amount_minor, monthly_credits, billing_time_zone,
                  status, created_at, payment_expires_at)
               VALUES (?, 'WORK', ?, ?, 'user-1', ?, ?, ?, ?, ?, ?, 'Asia/Shanghai',
                       'PENDING_PAYMENT', ?, ?)`,
            )
            .bind(
              id,
              kind,
              kind === 'USER' ? 'user-1' : 'team-1',
              planId,
              quote.planVersion,
              interval,
              quote.currency,
              amount,
              credits,
              now,
              now + 1_800_000,
            )
            .run()
        }
      }

      assert.deepEqual(
        (
          await db.prepare('SELECT amount_minor FROM billing_order ORDER BY amount_minor').all()
        ).results.map((row) => row.amount_minor),
        [1000, 3000, 10000, 30000],
      )
      await assert.rejects(
        db.prepare("UPDATE billing_order SET amount_minor = 999 WHERE id = 'USER-MONTH'").run(),
        /quote is immutable/u,
      )
      await assert.rejects(
        db
          .prepare('UPDATE billing_plan_version SET monthly_credits = 1 WHERE plan_id = ?')
          .bind(personal)
          .run(),
        /versions are immutable/u,
      )
      await db.batch([
        db
          .prepare(
            `INSERT INTO billing_plan_version
          VALUES (?, 2, 'WORK', 'USER', '新版', 'CNY', 1100, 11000, 110, ?)`,
          )
          .bind(personal, now),
        db
          .prepare(
            "UPDATE billing_plan SET current_version=2, sale_status='DISCONTINUED' WHERE plan_id=?",
          )
          .bind(personal),
      ])
      await seedPlans(db)
      assert.deepEqual(
        await db
          .prepare('SELECT current_version,sale_status FROM billing_plan WHERE plan_id=?')
          .bind(personal)
          .first(),
        { current_version: 2, sale_status: 'DISCONTINUED' },
      )
      const retired = await caller.fetch(origin + catalogPath, { headers: billingHeaders() })
      assert.deepEqual(await retired.json(), { data: { items: [] } })
      assert.equal(
        await db
          .prepare("SELECT amount_minor FROM billing_order WHERE id='USER-MONTH'")
          .first('amount_minor'),
        1000,
      )
      assert.ok(
        fixture.calls.every(
          ({ path, requestId }) =>
            path === '/internal/v1/subjects/verify' && requestId === 'fixture-1',
        ),
      )
    })
  }
})

test('Billing rejects public ingress and rechecks current Auth authority on private routes', async (t) => {
  const auth = authFixture()
  const runtime = startLocalRuntime({
    bindings: { MOLIS_ENVIRONMENT: 'test', MOLIS_INTERNAL_SECRET: secret },
    auth: auth.fetch,
  })
  t.after(() => runtime.dispose())
  const db = await runtime.getD1Database('DB', 'billing')
  await applySchema(db)
  await seedPlans(db)
  const caller = await runtime.getWorker('caller')
  for (const path of [catalogPath, '/internal/v1', '/internal/v1/credits/holds']) {
    await assertError(
      await runtime.dispatchFetch(origin + path, { headers: billingHeaders() }),
      404,
      'NOT_FOUND',
    )
  }
  await assertError(await caller.fetch(origin + catalogPath), 404, 'NOT_FOUND')
  await assertError(
    await caller.fetch(origin + '/internal/v1/credits/holds', {
      method: 'POST',
      headers: { Authorization: 'Bearer valid' },
    }),
    404,
    'NOT_FOUND',
  )
  await assertError(
    await caller.fetch(origin + catalogPath, {
      headers: { ...billingHeaders(), [header]: randomBytes(32).toString('base64url') },
    }),
    404,
    'NOT_FOUND',
  )
  await assertError(
    await caller.fetch(origin + catalogPath, { headers: { [header]: secret } }),
    401,
    'UNAUTHENTICATED',
  )
  assert.equal(auth.calls.length, 0)
  const positives = await Promise.all([
    caller.fetch(origin + catalogPath, { headers: billingHeaders() }),
    caller.fetch(origin + catalogPath + teamQuery, { headers: billingHeaders('member') }),
    caller.fetch(origin + catalogPath + teamQuery, { headers: billingHeaders() }),
  ])
  for (const response of positives) {
    assert.equal(response.status, 200)
    assert.equal(response.headers.get('access-control-allow-origin'), null)
    await response.body?.cancel()
  }
  for (const [credential, status, code] of [
    ['expired', 401, 'SESSION_EXPIRED'],
    ['unauthorized', 401, 'UNAUTHENTICATED'],
    ['left', 403, 'FORBIDDEN'],
    ['disabled', 403, 'TENANT_DISABLED'],
    ['wrong-subject', 503, 'AUTH_UNAVAILABLE'],
    ['redirect', 503, 'AUTH_UNAVAILABLE'],
    ['broken', 503, 'AUTH_UNAVAILABLE'],
  ] as const) {
    await assertError(
      await caller.fetch(origin + catalogPath + teamQuery, { headers: billingHeaders(credential) }),
      status,
      code,
    )
  }
  await assertError(
    await caller.fetch(origin + catalogPath + '?ownerKind=USER&ownerId=another-user', {
      headers: billingHeaders(),
    }),
    403,
    'FORBIDDEN',
  )
  await assertError(
    await caller.fetch(origin + '/api/v1/orders' + teamQuery, {
      headers: billingHeaders('member'),
    }),
    403,
    'FORBIDDEN',
  )
  await assertError(
    await caller.fetch(origin + catalogPath + '?ownerKind=TENANT', { headers: billingHeaders() }),
    400,
    'INVALID_REQUEST',
  )
})

test('test and prod fail closed when the binding or internal secret is missing', async (t) => {
  for (const environment of ['test', 'prod']) {
    const noBinding = startLocalRuntime({
      bindings: {
        MOLIS_ENVIRONMENT: environment,
        MOLIS_INTERNAL_SECRET: secret,
        AUTH_ORIGIN: 'http://127.0.0.1:8787',
      },
    })
    t.after(() => noBinding.dispose())
    const caller = await noBinding.getWorker('caller')
    await assertError(
      await caller.fetch(origin + catalogPath, { headers: billingHeaders() }),
      503,
      'AUTH_UNAVAILABLE',
    )
  }
  const noSecret = startLocalRuntime({ bindings: { MOLIS_ENVIRONMENT: 'prod' } })
  t.after(() => noSecret.dispose())
  const caller = await noSecret.getWorker('caller')
  await assertError(
    await caller.fetch(origin + catalogPath, { headers: billingHeaders() }),
    503,
    'BILLING_UNAVAILABLE',
  )
})

test('a D1 guard miss rolls back preceding audit writes, while one concurrent write wins', async (t) => {
  const runtime = startLocalRuntime()
  t.after(() => runtime.dispose())
  const db = await runtime.getD1Database('DB', 'billing')
  await applySchema(db)
  await seedPlans(db)
  const audit = (id: string) =>
    db
      .prepare(
        `INSERT INTO billing_audit
    VALUES (?, 'billing', NULL, 'plan.update', ?, 'WORK', 'USER', 'user-1', ?, '{}', ?)`,
      )
      .bind(id, personal, id, now)
  const guard = () =>
    db
      .prepare(
        `INSERT INTO billing_write_guard (failure)
    SELECT 1 WHERE CASE WHEN (
      EXISTS (SELECT 1 FROM billing_plan WHERE plan_id = ? AND sale_status = 'ON_SALE')
    ) THEN 0 ELSE 1 END = 1`,
      )
      .bind(personal)
  const change = () =>
    db.prepare("UPDATE billing_plan SET sale_status='DISCONTINUED' WHERE plan_id=?").bind(personal)
  const receipt = (auditId: string) =>
    db
      .prepare(
        `INSERT INTO billing_idempotency_receipt
      VALUES ('billing', 'user-1', 'plan.update', 'WORK', 'USER', 'user-1', 'request-1',
              ?, 200, '{"data":{"updated":true}}', ?, ?, ?)`,
      )
      .bind('a'.repeat(64), auditId, now, now + 60_000)
  const writes = await Promise.allSettled([
    db.batch([audit('a1'), guard(), change(), receipt('a1')]),
    db.batch([audit('a2'), guard(), change(), receipt('a2')]),
  ])
  assert.equal(writes.filter((result) => result.status === 'fulfilled').length, 1)
  const rejected = writes.find((result) => result.status === 'rejected')
  assert.match(String(rejected?.reason), /CHECK constraint failed: billing_write_guard_satisfied/u)
  await assert.rejects(db.batch([audit('replay'), receipt('replay')]), /UNIQUE constraint failed/u)
  assert.equal(await db.prepare('SELECT COUNT(*) n FROM billing_audit').first('n'), 1)
  assert.equal(await db.prepare('SELECT COUNT(*) n FROM billing_write_guard').first('n'), 0)
  assert.deepEqual(
    await db
      .prepare(
        'SELECT r.response_status, r.response_json FROM billing_idempotency_receipt r JOIN billing_audit a ON a.id=r.audit_id',
      )
      .first(),
    { response_status: 200, response_json: '{"data":{"updated":true}}' },
  )
})

test('the Auth client sends the subject verification request over the exact dev loopback only', async (t) => {
  const network = t.mock.method(globalThis, 'fetch', async (request: Request) => {
    assert.equal(request.url, 'http://127.0.0.1:8787/internal/v1/subjects/verify')
    assert.equal(request.redirect, 'manual')
    assert.equal(request.headers.get(header), secret)
    assert.equal(request.headers.get('Authorization'), 'Bearer fixture')
    assert.equal(request.headers.get('X-Request-Id'), 'fixture-verify')
    assert.deepEqual(await request.json(), {})
    return Response.json({ data: { userId: 'user-1' } })
  })
  const vars = { AUTH_ORIGIN: 'http://127.0.0.1:8787', MOLIS_INTERNAL_SECRET: secret }
  const verify = (env: typeof vars & { MOLIS_ENVIRONMENT: string }) =>
    authFetch(env, '/internal/v1/subjects/verify', 'Bearer fixture', '{}', 'fixture-verify')
  assert.deepEqual(JSON.parse(await verify({ ...vars, MOLIS_ENVIRONMENT: 'dev' })), {
    status: 200,
    result: { data: { userId: 'user-1' } },
  })
  for (const env of ['test', 'prod']) {
    await assert.rejects(verify({ ...vars, MOLIS_ENVIRONMENT: env }), /transport unavailable/u)
  }
  await assert.rejects(
    verify({ ...vars, MOLIS_ENVIRONMENT: 'dev', AUTH_ORIGIN: 'http://127.0.0.1:8789' }),
    /transport unavailable/u,
  )
  assert.equal(network.mock.callCount(), 1)
})

test('credit hot paths and maintenance use bounded index searches on the baseline', async (t) => {
  const runtime = startLocalRuntime()
  t.after(() => runtime.dispose())
  const db = await runtime.getD1Database('DB', 'billing')
  await applySchema(db)
  const { readFile } = await import('node:fs/promises')
  for (const [query, bindings, indexes] of [
    ['consume_expired', 2, ['billing_credit_hold_period']],
    ['archive_holds', 2, ['billing_credit_hold_period']],
    ['release_due_holds', 4, ['billing_credit_hold_subject_open']],
    ['period_at', 5, ['billing_credit_period_subject']],
    ['archive_due', 4, ['billing_credit_period_subject_active']],
    [
      'cleanup_due',
      2,
      [
        'billing_credit_period_expiry',
        'billing_credit_hold_expiry',
        'billing_service_period_pending_credits',
      ],
    ],
  ] as const) {
    const sql = await readFile(
      new URL('../rust/src/credit/sql/' + query + '.sql', import.meta.url),
      'utf8',
    )
    const result = await db
      .prepare('EXPLAIN QUERY PLAN ' + sql)
      .bind(...Array(bindings).fill(1))
      .all()
    const plan = result.results.map((row) => row.detail).join('\n')
    assert.doesNotMatch(plan, /\bSCAN\b|billing_service_period_active\b/)
    if (query !== 'cleanup_due') assert.doesNotMatch(plan, /TEMP B-TREE/)
    for (const index of indexes) assert.ok(plan.includes(index), plan)
    console.log('QUERY_PLAN', query, plan)
  }
})
