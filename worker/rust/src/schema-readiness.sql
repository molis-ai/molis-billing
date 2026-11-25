SELECT
  EXISTS (SELECT current_version FROM billing_plan LIMIT 1) AS plans,
  EXISTS (SELECT created_at FROM billing_plan_version LIMIT 1) AS plan_versions,
  EXISTS (SELECT version FROM billing_order LIMIT 1) AS orders,
  EXISTS (SELECT anchor_at FROM billing_subscription LIMIT 1) AS subscriptions,
  EXISTS (SELECT revoked_at FROM billing_order_service_period LIMIT 1) AS service_periods,
  EXISTS (SELECT external_event_id FROM billing_payment_event LIMIT 1) AS payment_events,
  EXISTS (SELECT version FROM billing_credit_period LIMIT 1) AS credit_periods,
  EXISTS (SELECT version FROM billing_credit_hold LIMIT 1) AS credit_holds,
  EXISTS (SELECT delta_available FROM billing_credit_ledger LIMIT 1) AS credit_ledger,
  EXISTS (SELECT request_id FROM billing_audit LIMIT 1) AS audit,
  EXISTS (SELECT expires_at FROM billing_idempotency_receipt LIMIT 1) AS receipts,
  EXISTS (SELECT failure FROM billing_write_guard LIMIT 1) AS write_guard,
  1 AS ok
