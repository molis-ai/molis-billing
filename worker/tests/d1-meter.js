// Miniflare-only entrypoint. Count calls where the WASM Worker crosses into the D1 binding.
import PublicWorker, { BillingInternal as InternalWorker } from './src/runtime/entrypoint.js'

function metered(Worker) {
  return class extends Worker {
    async fetch(request) {
      const counts = { reads: 0, writes: 0 }
      const originals = new WeakMap()
      function statement(target, sql) {
        const proxy = new Proxy(target, {
          get(target, key) {
            if (key === 'bind') return (...args) => statement(target.bind(...args), sql)
            if (['first', 'all', 'raw', 'run'].includes(key))
              return (...args) => {
                counts[/^\s*SELECT\b/i.test(sql) ? 'reads' : 'writes']++
                return target[key](...args)
              }
            return target[key]
          },
        })
        originals.set(proxy, { target, sql })
        return proxy
      }
      const db = new Proxy(this.env.DB, {
        get(target, key) {
          if (key === 'prepare') return (sql) => statement(target.prepare(sql), sql)
          if (key === 'batch')
            return (statements) => {
              const entries = statements.map((s) => originals.get(s))
              counts[entries.every(({ sql }) => /^\s*SELECT\b/i.test(sql)) ? 'reads' : 'writes']++
              return target.batch(entries.map(({ target }) => target))
            }
          return target[key]
        },
      })
      const result = await Worker.prototype.fetch.call(
        { env: { ...this.env, DB: db }, ctx: this.ctx },
        request,
      )
      const response = new Response(result.body, result)
      response.headers.set('X-Test-D1-Counts', JSON.stringify(counts))
      return response
    }
  }
}

export default metered(PublicWorker)
export const BillingInternal = metered(InternalWorker)
