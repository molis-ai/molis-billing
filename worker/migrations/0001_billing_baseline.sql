CREATE TABLE billing_plan_version (
  plan_id TEXT NOT NULL,
  plan_version INTEGER NOT NULL CHECK (plan_version > 0),
  product TEXT NOT NULL,
  owner_kind TEXT NOT NULL CHECK (owner_kind IN ('USER', 'TENANT')),
  name TEXT NOT NULL,
  currency TEXT NOT NULL,
  monthly_price_minor INTEGER NOT NULL CHECK (monthly_price_minor >= 0),
  yearly_price_minor INTEGER NOT NULL CHECK (yearly_price_minor >= 0),
  monthly_credits INTEGER NOT NULL CHECK (monthly_credits >= 0),
  created_at INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (plan_id, plan_version)
) STRICT;

CREATE UNIQUE INDEX billing_plan_version_scope
  ON billing_plan_version (plan_id, plan_version, product, owner_kind);

CREATE TABLE billing_plan (
  plan_id TEXT PRIMARY KEY NOT NULL,
  product TEXT NOT NULL,
  owner_kind TEXT NOT NULL CHECK (owner_kind IN ('USER', 'TENANT')),
  sale_status TEXT NOT NULL CHECK (sale_status IN ('ON_SALE', 'DISCONTINUED')),
  current_version INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE (plan_id, product, owner_kind),
  FOREIGN KEY (plan_id, current_version, product, owner_kind)
    REFERENCES billing_plan_version (plan_id, plan_version, product, owner_kind)
) STRICT;

CREATE INDEX billing_plan_catalog ON billing_plan (product, owner_kind, sale_status, plan_id);

CREATE TRIGGER billing_plan_version_no_update
BEFORE UPDATE ON billing_plan_version BEGIN
  SELECT RAISE(ABORT, 'billing plan versions are immutable');
END;

CREATE TRIGGER billing_plan_version_no_delete
BEFORE DELETE ON billing_plan_version BEGIN
  SELECT RAISE(ABORT, 'billing plan versions are immutable');
END;

-- REPLACE can bypass DELETE triggers when recursive triggers are off.
CREATE TRIGGER billing_plan_version_no_replace
BEFORE INSERT ON billing_plan_version
WHEN EXISTS (
  SELECT 1 FROM billing_plan_version
  WHERE plan_id = NEW.plan_id AND plan_version = NEW.plan_version
) BEGIN
  SELECT RAISE(ABORT, 'billing plan versions are immutable');
END;

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
  credits_complete INTEGER NOT NULL DEFAULT 0 CHECK (credits_complete IN (0, 1)),
  version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0),
  UNIQUE (order_id, product, owner_kind, owner_id),
  FOREIGN KEY (order_id, product, owner_kind, owner_id, plan_id)
    REFERENCES billing_order (id, product, owner_kind, owner_id, plan_id),
  FOREIGN KEY (subscription_id, product, owner_kind, owner_id, plan_id)
    REFERENCES billing_subscription (id, product, owner_kind, owner_id, plan_id)
) STRICT;

CREATE INDEX billing_service_period_active
  ON billing_order_service_period (product, owner_kind, owner_id, starts_at, ends_at) WHERE revoked_at IS NULL;
CREATE INDEX billing_service_period_pending_credits
  ON billing_order_service_period (starts_at, product, owner_kind, owner_id, order_id)
  WHERE revoked_at IS NULL AND credits_complete = 0;

CREATE INDEX billing_service_period_subject_pending_credits
  ON billing_order_service_period (product, owner_kind, owner_id, starts_at, order_id)
  WHERE revoked_at IS NULL AND credits_complete = 0;

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

CREATE TABLE billing_credit_period (
  id TEXT PRIMARY KEY NOT NULL,
  order_id TEXT NOT NULL,
  month_index INTEGER NOT NULL CHECK (month_index BETWEEN 0 AND 11),
  product TEXT NOT NULL,
  owner_kind TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  starts_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL CHECK (expires_at > starts_at),
  status TEXT NOT NULL CHECK (status IN ('ACTIVE', 'EXPIRED', 'REVOKED')),
  granted INTEGER NOT NULL CHECK (granted >= 0),
  available INTEGER NOT NULL CHECK (available >= 0),
  reserved INTEGER NOT NULL CHECK (reserved >= 0),
  consumed INTEGER NOT NULL CHECK (consumed >= 0),
  expired INTEGER NOT NULL CHECK (expired >= 0),
  granted_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0),
  CHECK (granted = available + reserved + consumed + expired),
  UNIQUE (order_id, month_index),
  UNIQUE (id, product, owner_kind, owner_id),
  FOREIGN KEY (order_id, product, owner_kind, owner_id)
    REFERENCES billing_order_service_period (order_id, product, owner_kind, owner_id)
) STRICT;

CREATE INDEX billing_credit_period_subject
  ON billing_credit_period (product, owner_kind, owner_id, starts_at DESC, id DESC);
CREATE INDEX billing_credit_period_subject_active
  ON billing_credit_period (product, owner_kind, owner_id, expires_at, id) WHERE status = 'ACTIVE';
CREATE INDEX billing_credit_period_expiry ON billing_credit_period (expires_at) WHERE status = 'ACTIVE';

CREATE TABLE billing_credit_hold (
  id TEXT PRIMARY KEY NOT NULL,
  credit_period_id TEXT NOT NULL,
  product TEXT NOT NULL,
  owner_kind TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  reference_id TEXT NOT NULL,
  actor_user_id TEXT NOT NULL,
  calling_service TEXT NOT NULL,
  request_id TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('OPEN', 'CLOSED', 'EXPIRED')),
  amount INTEGER NOT NULL CHECK (amount > 0),
  reserved INTEGER NOT NULL CHECK (reserved >= 0),
  settled INTEGER NOT NULL CHECK (settled >= 0),
  released INTEGER NOT NULL CHECK (released >= 0),
  expired INTEGER NOT NULL CHECK (expired >= 0),
  occurred_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL CHECK (expires_at > created_at),
  updated_at INTEGER NOT NULL,
  version INTEGER NOT NULL DEFAULT 1 CHECK (version > 0),
  CHECK (amount = reserved + settled + released + expired),
  CHECK ((status = 'OPEN' AND reserved > 0) OR (status != 'OPEN' AND reserved = 0)),
  UNIQUE (id, credit_period_id),
  FOREIGN KEY (credit_period_id, product, owner_kind, owner_id)
    REFERENCES billing_credit_period (id, product, owner_kind, owner_id)
) STRICT;

CREATE INDEX billing_credit_hold_reference
  ON billing_credit_hold (calling_service, actor_user_id, reference_id, credit_period_id);
CREATE INDEX billing_credit_hold_period
  ON billing_credit_hold (credit_period_id, status, expires_at);
CREATE INDEX billing_credit_hold_subject_open
  ON billing_credit_hold (product, owner_kind, owner_id, expires_at, id) WHERE status = 'OPEN';
CREATE INDEX billing_credit_hold_expiry ON billing_credit_hold (expires_at) WHERE status = 'OPEN';

CREATE TABLE billing_credit_ledger (
  id TEXT PRIMARY KEY NOT NULL,
  credit_period_id TEXT NOT NULL,
  hold_id TEXT,
  product TEXT NOT NULL,
  owner_kind TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK (kind IN ('GRANT', 'HOLD', 'SETTLE', 'RELEASE', 'CONSUME', 'EXPIRE', 'REVOKE')),
  delta_granted INTEGER NOT NULL DEFAULT 0,
  delta_available INTEGER NOT NULL DEFAULT 0,
  delta_reserved INTEGER NOT NULL DEFAULT 0,
  delta_consumed INTEGER NOT NULL DEFAULT 0,
  delta_expired INTEGER NOT NULL DEFAULT 0,
  reference_id TEXT,
  actor_user_id TEXT,
  calling_service TEXT NOT NULL,
  request_id TEXT NOT NULL,
  audit_id TEXT NOT NULL REFERENCES billing_audit (id),
  occurred_at INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  CHECK (delta_granted = delta_available + delta_reserved + delta_consumed + delta_expired),
  CHECK (kind NOT IN ('HOLD', 'SETTLE', 'RELEASE', 'CONSUME')
    OR (actor_user_id IS NOT NULL AND reference_id IS NOT NULL)),
  FOREIGN KEY (credit_period_id, product, owner_kind, owner_id)
    REFERENCES billing_credit_period (id, product, owner_kind, owner_id),
  FOREIGN KEY (hold_id, credit_period_id) REFERENCES billing_credit_hold (id, credit_period_id)
) STRICT;

CREATE INDEX billing_credit_ledger_page
  ON billing_credit_ledger (product, owner_kind, owner_id, created_at DESC, id DESC);
CREATE INDEX billing_credit_ledger_actor_page
  ON billing_credit_ledger (product, owner_kind, owner_id, actor_user_id, created_at DESC, id DESC)
  WHERE kind IN ('HOLD', 'SETTLE', 'RELEASE', 'CONSUME');
CREATE UNIQUE INDEX billing_credit_ledger_single_period_event
  ON billing_credit_ledger (credit_period_id, kind) WHERE kind IN ('GRANT', 'EXPIRE', 'REVOKE');

CREATE TRIGGER billing_credit_ledger_no_update
BEFORE UPDATE ON billing_credit_ledger BEGIN
  SELECT RAISE(ABORT, 'billing credit ledger is append-only');
END;

CREATE TRIGGER billing_credit_ledger_no_delete
BEFORE DELETE ON billing_credit_ledger BEGIN
  SELECT RAISE(ABORT, 'billing credit ledger is append-only');
END;

CREATE TRIGGER billing_credit_ledger_no_replace
BEFORE INSERT ON billing_credit_ledger
WHEN EXISTS (
  SELECT 1 FROM billing_credit_ledger l
  WHERE l.id = NEW.id OR (
    NEW.kind IN ('GRANT', 'EXPIRE', 'REVOKE')
    AND l.credit_period_id = NEW.credit_period_id AND l.kind = NEW.kind
  )
) BEGIN
  SELECT RAISE(ABORT, 'billing credit ledger is append-only');
END;
