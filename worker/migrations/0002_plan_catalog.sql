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
  PRIMARY KEY (plan_id, plan_version)
) STRICT;
