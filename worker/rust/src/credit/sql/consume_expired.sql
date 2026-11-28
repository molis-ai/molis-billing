SELECT p.expired - COALESCE((SELECT SUM(h.expired) FROM billing_credit_hold h
                           WHERE h.credit_period_id=p.id), 0) AS available
FROM billing_credit_period p WHERE p.id=? AND p.version=?
