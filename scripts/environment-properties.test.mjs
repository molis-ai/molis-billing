import assert from 'node:assert/strict'
import { createServer } from 'node:net'
import { test } from 'node:test'
import {
  assertPortAvailable,
  deploymentDatabaseId,
  localDatabaseId,
  parseProperties,
  selectEnvironment,
} from './environment-properties.mjs'

test('parses comments and preserves equals signs in property values', () => {
  assert.deepEqual(
    parseProperties(' # comment\r\n! comment\r\n key = a=b \r\nempty=\r\n', 'fixture'),
    {
      key: 'a=b',
      empty: '',
    },
  )
  assert.throws(() => parseProperties('key=a\nkey=b', 'fixture'), /fixture at line 2/)
  assert.throws(() => parseProperties('invalid line', 'fixture'), /fixture at line 1/)
})

test('selects local start by default and validates deployment flags', () => {
  assert.deepEqual(selectEnvironment('start', []), { name: 'dev', dryRun: false })
  assert.deepEqual(selectEnvironment('start', ['--env', 'dev']), { name: 'dev', dryRun: false })
  assert.throws(() => selectEnvironment('start', ['--env', 'prod']), /Local start/)
  assert.throws(() => selectEnvironment('deploy', []), /Choose one environment/)
  assert.throws(() => selectEnvironment('deploy', ['--env', 'dev']), /requires --dry-run/)
  assert.throws(
    () => selectEnvironment('deploy', ['--env', 'test', '--env', 'prod']),
    /Choose one environment/,
  )
  for (const name of ['dev', 'test', 'prod']) {
    assert.deepEqual(selectEnvironment('deploy', ['--env', name, '--dry-run']), {
      name,
      dryRun: true,
    })
  }
})

test('only dry runs can substitute remote D1 placeholders', () => {
  const databaseId = 'bfeea980-f1de-43f5-a320-6813b2afacb2'
  assert.equal(deploymentDatabaseId('dev', localDatabaseId, false), localDatabaseId)
  for (const name of ['test', 'prod']) {
    const placeholder = `REPLACE_WITH_${name.toUpperCase()}_D1_DATABASE_ID`
    assert.equal(deploymentDatabaseId(name, placeholder, true), localDatabaseId)
    assert.equal(deploymentDatabaseId(name, databaseId, false), databaseId)
    assert.throws(() => deploymentDatabaseId(name, placeholder, false), /valid D1 databaseId/)
    assert.throws(() => deploymentDatabaseId(name, localDatabaseId, false), /valid D1 databaseId/)
  }
})

test('local port check rejects an occupied port', async (context) => {
  const server = createServer()
  await new Promise((resolve, reject) => {
    server.once('error', reject)
    server.listen(0, '127.0.0.1', resolve)
  })
  context.after(() => new Promise((resolve) => server.close(resolve)))

  await assert.rejects(assertPortAvailable(server.address().port), /EADDRINUSE/)
  await assertPortAvailable(0)
})
