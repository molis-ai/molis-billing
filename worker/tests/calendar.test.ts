import assert from 'node:assert/strict'
import { test } from 'node:test'
import { monthBoundary, normalizeZone } from '../rust/src/runtime/foundation.js'

const ms = (iso: string) => Date.parse(iso)

test('A06 clamped month ends recover the original day and preserve time through leap and new years', () => {
  const at = ms('2028-01-31T03:17:29.271Z')
  const zone = 'Asia/Shanghai'
  assert.equal(monthBoundary(at, zone, 0), at)
  assert.equal(monthBoundary(at, zone, 1), ms('2028-02-29T03:17:29.271Z'))
  assert.equal(monthBoundary(at, zone, 2), ms('2028-03-31T03:17:29.271Z'))
  assert.equal(monthBoundary(at, zone, 12), ms('2029-01-31T03:17:29.271Z'))
  assert.equal(monthBoundary(at, zone, 13), ms('2029-02-28T03:17:29.271Z'))
  assert.equal(monthBoundary(ms('2027-12-31T17:00:00Z'), zone, 1), ms('2028-01-31T17:00:00Z'))
})

test('A06 DST gap moves forward, fold uses first occurrence, and later months recover wall time', () => {
  const zone = 'America/New_York'
  const spring = ms('2028-02-12T07:30:55.123Z')
  assert.equal(monthBoundary(spring, zone, 1), ms('2028-03-12T07:30:55.123Z'))
  assert.equal(monthBoundary(spring, zone, 2), ms('2028-04-12T06:30:55.123Z'))
  const autumn = ms('2028-10-05T05:30:55.123Z')
  assert.equal(monthBoundary(autumn, zone, 1), ms('2028-11-05T05:30:55.123Z'))
  assert.equal(monthBoundary(autumn, zone, 2), ms('2028-12-05T06:30:55.123Z'))
  const halfHour = ms('2028-09-01T15:45:00Z')
  assert.equal(monthBoundary(halfHour, 'Australia/Lord_Howe', 1), ms('2028-10-01T15:15:00Z'))
})

test('A06 rejects invalid named zones and two viewer zones preserve the same UTC cutoff', () => {
  for (const invalid of ['Not/A_Zone', '', '+08:00', '-02', ' UTC']) {
    assert.throws(() => normalizeZone(invalid), RangeError)
  }
  assert.equal(normalizeZone('Asia/Shanghai'), 'Asia/Shanghai')
  const cutoff = monthBoundary(ms('2027-05-12T07:30:00Z'), 'Asia/Shanghai', 1)
  assert.equal(cutoff, ms('2027-06-12T07:30:00Z'))
  const display = (timeZone: string) =>
    new Intl.DateTimeFormat('en-GB', { timeZone, dateStyle: 'short', timeStyle: 'short' }).format(
      cutoff,
    )
  assert.notEqual(display('Asia/Shanghai'), display('America/Los_Angeles'))
  assert.equal(cutoff - 1 < cutoff, true)
})
