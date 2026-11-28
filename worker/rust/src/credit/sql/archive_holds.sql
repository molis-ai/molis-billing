UPDATE billing_credit_hold
SET expired=expired+reserved, reserved=0, status='EXPIRED', updated_at=?, version=version+1
WHERE credit_period_id=? AND status='OPEN'
