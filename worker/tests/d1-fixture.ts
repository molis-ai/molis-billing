import { readFile } from 'node:fs/promises'

export async function sqlFile(file: string) {
  const sql = await readFile(new URL(`../${file}`, import.meta.url), 'utf8')
  const statements = []
  let current = ''
  let trigger = false
  for (const line of sql.split('\n').map((value) => value.trim())) {
    if (!line || line.startsWith('--')) continue
    current += `${line}\n`
    if (line.startsWith('CREATE TRIGGER ')) trigger = true
    if ((trigger && line === 'END;') || (!trigger && line.endsWith(';'))) {
      statements.push(current)
      current = ''
      trigger = false
    }
  }
  if (current) throw new Error(`Incomplete statement in ${file}`)
  return statements
}

type Database = Awaited<ReturnType<import('miniflare').Miniflare['getD1Database']>>

export async function applySchema(db: Database) {
  const statements = await sqlFile('migrations/0001_billing_baseline.sql')
  await db.batch(statements.map((sql) => db.prepare(sql)))
}

export async function seedPlans(db: Database) {
  const statements = await sqlFile('seeds/local-test-plans.sql')
  await db.batch(statements.map((sql) => db.prepare(sql)))
}
