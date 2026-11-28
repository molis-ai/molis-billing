SELECT h.id, h.occurred_at FROM billing_credit_hold h
JOIN billing_credit_period p ON p.id=h.credit_period_id
WHERE h.product=? AND h.owner_kind=? AND h.owner_id=?
  AND h.status='OPEN' AND h.expires_at<=? AND h.expires_at<p.expires_at
ORDER BY h.expires_at, h.id
