const dayMs = 86_400_000

function formatter(zone) {
  return new Intl.DateTimeFormat('en', {
    calendar: 'gregory',
    numberingSystem: 'latn',
    timeZone: zone,
    year: 'numeric',
    month: 'numeric',
    day: 'numeric',
    hour: 'numeric',
    minute: 'numeric',
    second: 'numeric',
    fractionalSecondDigits: 3,
    hourCycle: 'h23',
  })
}

export function normalizeZone(zone) {
  // Intl also accepts numeric offsets in newer runtimes. Billing requires an IANA identifier.
  if (!/^[a-z][a-z0-9_+/-]*$/iu.test(zone)) throw new RangeError('Invalid time zone')
  return formatter(zone).resolvedOptions().timeZone
}

function partsAt(format, instant) {
  const fields = Object.fromEntries(
    format.formatToParts(instant).map(({ type, value }) => [type, value]),
  )
  return [
    Number(fields.year),
    Number(fields.month),
    Number(fields.day),
    Number(fields.hour),
    Number(fields.minute),
    Number(fields.second),
    Number(fields.fractionalSecond),
  ]
}

function wallTime(year, month, day, hour = 0, minute = 0, second = 0, ms = 0) {
  const time = new Date(0)
  time.setUTCFullYear(year, month - 1, day)
  time.setUTCHours(hour, minute, second, ms)
  return time.getTime()
}

function instantAt(format, wall) {
  const offsets = new Set(
    [-dayMs, 0, dayMs].map((delta) => {
      const instant = wall + delta
      return wallTime(...partsAt(format, instant)) - instant
    }),
  )
  const candidates = [...offsets].map((offset) => wall - offset).sort((a, b) => a - b)
  // First occurrence at a fold. In a gap, use the earlier offset to move forward by the gap.
  return (
    candidates.find((instant) => wallTime(...partsAt(format, instant)) === wall) ??
    candidates.at(-1)
  )
}

export function monthBoundary(anchorAt, zone, monthOffset) {
  if (monthOffset === 0) return anchorAt

  const format = formatter(zone)
  const [year, month, day, ...clock] = partsAt(format, anchorAt)
  const monthIndex = year * 12 + month - 1 + monthOffset
  const targetYear = Math.floor(monthIndex / 12)
  const targetMonth = (monthIndex % 12) + 1
  const monthEnd = new Date(wallTime(targetYear, targetMonth + 1, 0)).getUTCDate()
  return instantAt(format, wallTime(targetYear, targetMonth, Math.min(day, monthEnd), ...clock))
}

export async function sha256(input) {
  return new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(input)))
}

export async function matchSecret(expected, provided) {
  const [left, right] = await Promise.all([sha256(expected), sha256(provided)])
  return crypto.subtle.timingSafeEqual(left, right)
}

export function requestId() {
  return crypto.randomUUID()
}

export async function authFetch(env, path, authorization, body, requestId) {
  const secret = env.MOLIS_INTERNAL_SECRET
  if (typeof secret !== 'string' || !/^[A-Za-z0-9_-]{43}$/u.test(secret))
    throw new Error('Auth transport unavailable')
  const local = env.MOLIS_ENVIRONMENT === 'dev' && env.AUTH_ORIGIN === 'http://127.0.0.1:8787'
  const binding =
    env.MOLIS_ENVIRONMENT === 'test' || env.MOLIS_ENVIRONMENT === 'prod' ? env.AUTH : undefined
  if (!local && typeof binding?.fetch !== 'function') throw new Error('Auth transport unavailable')

  const url = new URL(path, local ? 'http://127.0.0.1:8787' : 'https://auth.internal')
  const request = new Request(url, {
    method: 'POST',
    headers: {
      Accept: 'application/json',
      Authorization: authorization,
      'Content-Type': 'application/json',
      'X-Molis-Internal-Secret': secret,
      'X-Request-Id': requestId,
    },
    body,
    redirect: 'manual',
    signal: AbortSignal.timeout(10_000),
  })
  const response = await (local ? fetch(request) : binding.fetch(request))
  const contentType = response.headers.get('Content-Type')?.split(';', 1)[0].trim().toLowerCase()
  if (contentType !== 'application/json' || (response.status >= 300 && response.status < 400))
    throw new Error('Invalid Auth response')
  return JSON.stringify({ status: response.status, result: await response.json() })
}
