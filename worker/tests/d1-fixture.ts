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

export async function applySchema(db: Database, from = 1, to = 5) {
  const files = [
    '0001_init.sql',
    '0002_plan_catalog.sql',
    '0003_billing_plan.sql',
    '0004_billing_orders.sql',
    '0005_billing_credits.sql',
  ]
  for (const file of files.slice(from - 1, to)) {
    const statements = await sqlFile(`migrations/${file}`)
    await db.batch(statements.map((sql) => db.prepare(sql)))
  }
}

export async function seedPlans(db: Database) {
  const statements = await sqlFile('seeds/local-test-plans.sql')
  await db.batch(statements.map((sql) => db.prepare(sql)))
}
