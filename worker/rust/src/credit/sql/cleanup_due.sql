SELECT product, owner_kind, owner_id FROM billing_credit_period INDEXED BY billing_credit_period_expiry
  WHERE status='ACTIVE' AND expires_at<=?1
UNION
SELECT product, owner_kind, owner_id FROM billing_credit_hold INDEXED BY billing_credit_hold_expiry
  WHERE status='OPEN' AND expires_at<=?1
UNION
SELECT p.product, p.owner_kind, p.owner_id FROM billing_order_service_period p INDEXED BY billing_service_period_pending_credits
  JOIN billing_order o ON o.id=p.order_id
  WHERE p.revoked_at IS NULL AND p.credits_complete=0 AND o.status='PAID' AND p.starts_at<=?1 AND NOT EXISTS (
    SELECT 1 FROM billing_credit_period c
    WHERE c.order_id=p.order_id AND c.starts_at<=?1 AND c.expires_at>?1)
ORDER BY product, owner_kind, owner_id LIMIT ?2
