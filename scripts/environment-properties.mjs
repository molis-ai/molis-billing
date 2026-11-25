import { readFileSync } from 'node:fs'
import { createServer } from 'node:net'
import { parseArgs } from 'node:util'

export const localDatabaseId = '00000000-0000-0000-0000-000000000000'
const environments = ['dev', 'test', 'prod']
const uuid = /^[a-f\d]{8}(?:-[a-f\d]{4}){3}-[a-f\d]{12}$/iu

export function selectEnvironment(operation, args) {
  if (!['start', 'deploy'].includes(operation)) throw new Error('Choose start or deploy.')
  const { values, tokens } = parseArgs({
    args,
    options: { env: { type: 'string' }, 'dry-run': { type: 'boolean' } },
    tokens: true,
  })
  const dryRun = values['dry-run'] ?? false
  const name = values.env ?? (operation === 'start' ? 'dev' : undefined)
  if (
    new Set(tokens.map((token) => token.name)).size !== tokens.length ||
    !environments.includes(name)
  ) {
    throw new Error('Choose one environment with --env dev|test|prod.')
  }
  if (operation === 'start' && (name !== 'dev' || dryRun)) {
    throw new Error('Local start uses --env dev.')
  }
  if (operation === 'deploy' && name === 'dev' && !dryRun) {
    throw new Error('Deploy for dev requires --dry-run.')
  }
  return { name, dryRun }
}

export function parseProperties(input, source) {
  const entries = new Map()
  for (const [index, line] of input.split(/\r?\n/u).entries()) {
    const text = line.trim()
    if (!text || /^[#!]/u.test(text)) continue
    const match = /^([A-Za-z][A-Za-z0-9_.-]*)\s*=\s*(.*)$/u.exec(text)
    if (!match || entries.has(match[1])) {
      throw new Error(`Invalid or duplicate property in ${source} at line ${index + 1}.`)
    }
    const [, key, value] = match
    entries.set(key, value)
  }
  return Object.fromEntries(entries)
}

export function readProperties(url, { optional = false } = {}) {
  try {
    return parseProperties(
      new TextDecoder('utf-8', { fatal: true }).decode(readFileSync(url)),
      url.pathname,
    )
  } catch (error) {
    if (optional && error.code === 'ENOENT') return {}
    throw error
  }
}

export function deploymentDatabaseId(name, databaseId, dryRun) {
  if (name === 'dev' && databaseId === localDatabaseId) return databaseId
  const placeholder = `REPLACE_WITH_${name.toUpperCase()}_D1_DATABASE_ID`
  if (name !== 'dev' && databaseId === placeholder && dryRun) return localDatabaseId
  if (name !== 'dev' && uuid.test(databaseId) && databaseId !== localDatabaseId) return databaseId
  throw new Error(`Configure a valid D1 databaseId in config/${name}.properties.`)
}

export async function assertPortAvailable(port) {
  const server = createServer()
  await new Promise((resolve, reject) => {
    server.once('error', (error) => {
      reject(new Error(`Cannot bind 127.0.0.1:${port} (${error.code}).`))
    })
    server.listen({ host: '127.0.0.1', port, exclusive: true }, () => server.close(resolve))
  })
}
