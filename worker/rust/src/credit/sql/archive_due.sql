SELECT id, starts_at, expires_at, granted, available, reserved, consumed, version, status
FROM billing_credit_period
WHERE product=? AND owner_kind=? AND owner_id=? AND status='ACTIVE' AND expires_at<=?
ORDER BY expires_at, id
