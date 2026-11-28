import assert from 'node:assert/strict'
import { createServer } from 'node:net'
import { test } from 'node:test'
import { billingEnvironment } from '../worker/scripts/environment.mjs'
import {
  assertPortAvailable,
  deploymentDatabaseId,
  localDatabaseId,
  parseProperties,
  readProperties,
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

test('Grafana Logs export is opt-in remotely and forbidden in dev', () => {
  const destination = 'molis-grafana-logs'
  const prod = billingEnvironment('prod', true).platform
  assert.deepEqual(prod.observability, {
    enabled: true,
    redact_query_string: true,
    logs: {
      enabled: true,
      invocation_logs: false,
      head_sampling_rate: 1,
      persist: false,
      destinations: [destination],
    },
    traces: { enabled: false },
  })
  assert.deepEqual(billingEnvironment('test', true).platform.observability, prod.observability)
  assert.equal(billingEnvironment('dev', true).platform.observability, undefined)
  const settings = (name) =>
    readProperties(new URL(`../config/${name}.properties`, import.meta.url))
  const withExport = (name, grafanaLogsDestination) =>
    billingEnvironment(name, true, { ...settings(name), grafanaLogsDestination }).platform
  assert.equal(withExport('test', undefined).observability, undefined)
  assert.throws(() => withExport('dev', destination), /config\/dev.properties/)
  assert.throws(() => withExport('test', ''), /config\/test.properties/)
  assert.throws(() => withExport('prod', 'invalid/name'), /config\/prod.properties/)
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
