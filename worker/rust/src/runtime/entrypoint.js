import BillingWorker from '../../build/index.js'

function isPublic(path) {
  return (
    path === '/api/v1/health' ||
    path === '/api/v1/readiness' ||
    path.startsWith('/api/v1/webhooks/')
  )
}

function notFound() {
  return Response.json(
    { error: { code: 'NOT_FOUND', message: 'Route not found' } },
    {
      status: 404,
      headers: {
        'Content-Type': 'application/json; charset=utf-8',
        'Cache-Control': 'no-store',
        'X-Content-Type-Options': 'nosniff',
        'X-Frame-Options': 'DENY',
        'Referrer-Policy': 'no-referrer',
      },
    },
  )
}

export default class extends BillingWorker {
  fetch(request) {
    const env = this.env
    const url = new URL(request.url)
    const local =
      env.MOLIS_ENVIRONMENT === 'dev' &&
      env.BILLING_ORIGIN === 'http://127.0.0.1:8788' &&
      url.origin === env.BILLING_ORIGIN
    if (!local && !isPublic(url.pathname)) return notFound()
    return super.fetch(request)
  }
}

export class BillingInternal extends BillingWorker {
  fetch(request) {
    if (isPublic(new URL(request.url).pathname)) return notFound()
    return super.fetch(request)
  }
}
