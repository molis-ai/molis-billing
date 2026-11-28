import BillingWorker from '../../build/index.js'

function isPublic(path) {
  return (
    path === '/api/v1/health' ||
    path === '/api/v1/readiness' ||
    path.startsWith('/api/v1/webhooks/')
  )
}

function notFound(request, env, started) {
  const incomingId = request.headers.get('X-Request-Id') ?? ''
  const requestId = /^[A-Za-z0-9_.-]{1,128}$/u.test(incomingId) ? incomingId : crypto.randomUUID()
  console.log(
    JSON.stringify({
      schema: 1,
      service: 'molis-billing',
      env: ['prod', 'test', 'dev'].includes(env.MOLIS_ENVIRONMENT)
        ? env.MOLIS_ENVIRONMENT
        : 'unknown',
      event: 'billing.http.request',
      request_id: requestId,
      route: 'not_found',
      method: request.method,
      status: 404,
      error_code: 'NOT_FOUND',
      duration_ms: Math.max(0, Date.now() - started),
    }),
  )
  return Response.json(
    { error: { code: 'NOT_FOUND', message: 'Route not found' }, requestId },
    {
      status: 404,
      headers: {
        'Content-Type': 'application/json; charset=utf-8',
        'Cache-Control': 'no-store',
        'X-Content-Type-Options': 'nosniff',
        'X-Request-Id': requestId,
        'X-Frame-Options': 'DENY',
        'Referrer-Policy': 'no-referrer',
      },
    },
  )
}

export default class extends BillingWorker {
  fetch(request) {
    const started = Date.now()
    const env = this.env
    const url = new URL(request.url)
    const local =
      env.MOLIS_ENVIRONMENT === 'dev' &&
      env.BILLING_ORIGIN === 'http://127.0.0.1:8788' &&
      url.origin === env.BILLING_ORIGIN
    if (isPublic(url.pathname)) {
      // Auth's browser proxy uses this server-only header in local development, too.
      if (request.headers.has('X-Molis-Internal-Secret')) return notFound(request, env, started)
    } else if (!local) {
      return notFound(request, env, started)
    }
    return super.fetch(request)
  }
}

export class BillingInternal extends BillingWorker {
  fetch(request) {
    const started = Date.now()
    if (isPublic(new URL(request.url).pathname)) return notFound(request, this.env, started)
    return super.fetch(request)
  }
}
