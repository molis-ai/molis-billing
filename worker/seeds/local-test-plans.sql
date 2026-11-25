-- Local development catalog. Prices are in fen, credits are granted monthly.
WITH seed(plan_id, owner_kind, name, monthly_price_minor, yearly_price_minor, monthly_credits) AS (
  VALUES
    ('work-personal-test', 'USER', '个人测试套餐', 1000, 10000, 100),
    ('work-team-test', 'TENANT', '团队测试套餐', 3000, 30000, 500)
)
INSERT INTO billing_plan_version (
  plan_id, plan_version, product, owner_kind, name, currency,
  monthly_price_minor, yearly_price_minor, monthly_credits
)
SELECT seed.plan_id, 1, 'WORK', seed.owner_kind, seed.name, 'CNY',
       seed.monthly_price_minor, seed.yearly_price_minor, seed.monthly_credits
FROM seed
WHERE NOT EXISTS (
  SELECT 1 FROM billing_plan_version v WHERE v.plan_id = seed.plan_id AND v.plan_version = 1
);

INSERT INTO billing_plan (plan_id, product, owner_kind, sale_status, current_version, updated_at)
SELECT v.plan_id, v.product, v.owner_kind, 'ON_SALE', v.plan_version, v.created_at
FROM billing_plan_version v
WHERE v.plan_id IN ('work-personal-test', 'work-team-test')
  AND v.plan_version = (SELECT MAX(other.plan_version) FROM billing_plan_version other WHERE other.plan_id = v.plan_id)
ON CONFLICT (plan_id) DO NOTHING;
