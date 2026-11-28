SELECT id, starts_at, expires_at, granted, available, reserved, consumed, version, status
FROM billing_credit_period WHERE product=? AND owner_kind=? AND owner_id=?
  AND starts_at<=? AND expires_at>? AND status IN ('ACTIVE','EXPIRED')
ORDER BY starts_at DESC, id DESC LIMIT 1
