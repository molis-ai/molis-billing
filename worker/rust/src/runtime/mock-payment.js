export async function verifyMockSignature(secret, body, signature) {
  if (!/^[0-9a-f]{64}$/u.test(signature)) return false
  const key = await crypto.subtle.importKey(
    'raw',
    new TextEncoder().encode(secret),
    { name: 'HMAC', hash: 'SHA-256' },
    false,
    ['verify'],
  )
  const provided = Uint8Array.from(signature.match(/../gu), (hex) => Number.parseInt(hex, 16))
  return crypto.subtle.verify('HMAC', key, provided, body)
}
