ALTER TABLE billing_plan_version ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0;

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

INSERT INTO billing_plan (plan_id, product, owner_kind, sale_status, current_version, updated_at)
SELECT v.plan_id, v.product, v.owner_kind, 'ON_SALE', v.plan_version, v.created_at
FROM billing_plan_version v
WHERE v.plan_version = (
  SELECT MAX(prior.plan_version) FROM billing_plan_version prior WHERE prior.plan_id = v.plan_id
);

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
