CREATE TABLE billing_audit (
  id TEXT PRIMARY KEY NOT NULL,
  calling_service TEXT NOT NULL,
  actor_user_id TEXT,
  operation TEXT NOT NULL,
  target_id TEXT NOT NULL,
  product TEXT NOT NULL,
  owner_kind TEXT NOT NULL CHECK (owner_kind IN ('USER', 'TENANT')),
  owner_id TEXT NOT NULL,
  request_id TEXT NOT NULL,
  details_json TEXT NOT NULL CHECK (json_valid(details_json)),
  created_at INTEGER NOT NULL
) STRICT;

CREATE INDEX billing_audit_page ON billing_audit (created_at DESC, id DESC);
CREATE INDEX billing_audit_subject ON billing_audit (product, owner_kind, owner_id, created_at DESC, id DESC);

CREATE TABLE billing_idempotency_receipt (
  calling_service TEXT NOT NULL,
  actor_user_id TEXT NOT NULL,
  operation TEXT NOT NULL,
  product TEXT NOT NULL,
  owner_kind TEXT NOT NULL CHECK (owner_kind IN ('USER', 'TENANT')),
  owner_id TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  request_hash TEXT NOT NULL CHECK (length(request_hash) = 64),
  response_status INTEGER NOT NULL CHECK (response_status BETWEEN 200 AND 299),
  response_json TEXT NOT NULL CHECK (json_valid(response_json)),
  audit_id TEXT NOT NULL REFERENCES billing_audit (id),
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL CHECK (expires_at > created_at),
  PRIMARY KEY (calling_service, actor_user_id, operation, product, owner_kind, owner_id, idempotency_key)
) STRICT;

CREATE INDEX billing_idempotency_expiry ON billing_idempotency_receipt (expires_at);

-- The batch helper inserts a failing CHECK only when a write precondition misses.
CREATE TABLE billing_write_guard (
  failure INTEGER NOT NULL CONSTRAINT billing_write_guard_satisfied CHECK (failure = 0)
) STRICT;

CREATE TABLE billing_order (
  id TEXT PRIMARY KEY NOT NULL,
  product TEXT NOT NULL,
  owner_kind TEXT NOT NULL CHECK (owner_kind IN ('USER', 'TENANT')),
  owner_id TEXT NOT NULL,
  actor_user_id TEXT NOT NULL,
  plan_id TEXT NOT NULL,
  plan_version INTEGER NOT NULL,
  interval TEXT NOT NULL CHECK (interval IN ('MONTH', 'YEAR')),
  currency TEXT NOT NULL,
  amount_minor INTEGER NOT NULL CHECK (amount_minor >= 0),
  monthly_credits INTEGER NOT NULL CHECK (monthly_credits >= 0),
  billing_time_zone TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('PENDING_PAYMENT', 'EXPIRED', 'PAID', 'REFUNDED')),
  created_at INTEGER NOT NULL,
  payment_expires_at INTEGER NOT NULL CHECK (payment_expires_at = created_at + 1800000),
  paid_at INTEGER CHECK (paid_at >= created_at AND paid_at < payment_expires_at),
  refunded_at INTEGER CHECK (refunded_at >= paid_at),
  payment_provider TEXT,
  provider_payment_id TEXT,
  version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0),
  CHECK (
    (status IN ('PENDING_PAYMENT', 'EXPIRED') AND paid_at IS NULL AND refunded_at IS NULL
      AND payment_provider IS NULL AND provider_payment_id IS NULL)
    OR (status = 'PAID' AND paid_at IS NOT NULL AND refunded_at IS NULL
      AND payment_provider IS NOT NULL AND provider_payment_id IS NOT NULL)
    OR (status = 'REFUNDED' AND paid_at IS NOT NULL AND refunded_at IS NOT NULL
      AND payment_provider IS NOT NULL AND provider_payment_id IS NOT NULL)
  ),
  UNIQUE (payment_provider, provider_payment_id),
  UNIQUE (id, product, owner_kind, owner_id, plan_id),
  FOREIGN KEY (plan_id, product, owner_kind) REFERENCES billing_plan (plan_id, product, owner_kind),
  FOREIGN KEY (plan_id, plan_version, product, owner_kind)
    REFERENCES billing_plan_version (plan_id, plan_version, product, owner_kind)
) STRICT;

CREATE INDEX billing_order_page ON billing_order (product, owner_kind, owner_id, created_at DESC, id DESC);
CREATE INDEX billing_order_pending ON billing_order (payment_expires_at) WHERE status = 'PENDING_PAYMENT';

CREATE TRIGGER billing_order_quote_insert
BEFORE INSERT ON billing_order
WHEN NOT EXISTS (
  SELECT 1 FROM billing_plan_version v
  WHERE v.plan_id = NEW.plan_id AND v.plan_version = NEW.plan_version
    AND v.currency = NEW.currency AND v.monthly_credits = NEW.monthly_credits
    AND NEW.amount_minor = CASE NEW.interval
      WHEN 'MONTH' THEN v.monthly_price_minor WHEN 'YEAR' THEN v.yearly_price_minor END
) BEGIN
  SELECT RAISE(ABORT, 'billing order quote differs from its plan version');
END;

CREATE TRIGGER billing_order_quote_immutable
BEFORE UPDATE OF plan_version, plan_id, product, owner_kind, owner_id, actor_user_id, interval,
  currency, amount_minor, monthly_credits, billing_time_zone, created_at, payment_expires_at
ON billing_order BEGIN
  SELECT RAISE(ABORT, 'billing order quote is immutable');
END;

CREATE TABLE billing_subscription (
  id TEXT PRIMARY KEY NOT NULL,
  product TEXT NOT NULL,
  owner_kind TEXT NOT NULL CHECK (owner_kind IN ('USER', 'TENANT')),
  owner_id TEXT NOT NULL,
  plan_id TEXT NOT NULL,
  billing_time_zone TEXT NOT NULL,
  anchor_at INTEGER NOT NULL,
  version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0),
  UNIQUE (id, product, owner_kind, owner_id, plan_id),
  FOREIGN KEY (plan_id, product, owner_kind) REFERENCES billing_plan (plan_id, product, owner_kind)
) STRICT;

CREATE INDEX billing_subscription_subject ON billing_subscription (product, owner_kind, owner_id, anchor_at DESC);

CREATE TABLE billing_order_service_period (
  order_id TEXT PRIMARY KEY NOT NULL,
  subscription_id TEXT NOT NULL,
  product TEXT NOT NULL,
  owner_kind TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  plan_id TEXT NOT NULL,
  month_offset INTEGER NOT NULL CHECK (month_offset >= 0),
  month_count INTEGER NOT NULL CHECK (month_count IN (1, 12)),
  starts_at INTEGER NOT NULL,
  ends_at INTEGER NOT NULL CHECK (ends_at > starts_at),
  revoked_at INTEGER CHECK (revoked_at < starts_at),
  version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0),
  UNIQUE (order_id, product, owner_kind, owner_id),
  FOREIGN KEY (order_id, product, owner_kind, owner_id, plan_id)
    REFERENCES billing_order (id, product, owner_kind, owner_id, plan_id),
  FOREIGN KEY (subscription_id, product, owner_kind, owner_id, plan_id)
    REFERENCES billing_subscription (id, product, owner_kind, owner_id, plan_id)
) STRICT;

CREATE INDEX billing_service_period_active
  ON billing_order_service_period (product, owner_kind, owner_id, starts_at, ends_at) WHERE revoked_at IS NULL;
CREATE UNIQUE INDEX billing_service_period_offset
  ON billing_order_service_period (subscription_id, month_offset) WHERE revoked_at IS NULL;

CREATE TABLE billing_payment_event (
  provider TEXT NOT NULL,
  external_event_id TEXT NOT NULL,
  event_kind TEXT NOT NULL CHECK (event_kind IN ('PAYMENT', 'REFUND')),
  provider_payment_id TEXT NOT NULL,
  order_id TEXT NOT NULL REFERENCES billing_order (id),
  amount_minor INTEGER NOT NULL CHECK (amount_minor >= 0),
  currency TEXT NOT NULL,
  request_hash TEXT NOT NULL CHECK (length(request_hash) = 64),
  response_json TEXT NOT NULL CHECK (json_valid(response_json)),
  audit_id TEXT NOT NULL REFERENCES billing_audit (id),
  occurred_at INTEGER NOT NULL,
  received_at INTEGER NOT NULL,
  PRIMARY KEY (provider, external_event_id)
) STRICT;

CREATE INDEX billing_payment_event_order ON billing_payment_event (order_id, received_at);
