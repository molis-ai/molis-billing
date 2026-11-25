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
  ON billing_credit_period (product, owner_kind, owner_id, starts_at, expires_at);
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
