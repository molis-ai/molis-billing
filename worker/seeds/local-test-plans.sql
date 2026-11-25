-- Local development catalog. Prices are in fen, credits are granted monthly.
INSERT INTO billing_plan_version (
  plan_id, plan_version, product, owner_kind, name, currency,
  monthly_price_minor, yearly_price_minor, monthly_credits
) VALUES
  ('work-personal-test', 1, 'WORK', 'USER', '个人测试套餐', 'CNY', 1000, 10000, 100),
  ('work-team-test', 1, 'WORK', 'TENANT', '团队测试套餐', 'CNY', 3000, 30000, 500)
ON CONFLICT (plan_id, plan_version) DO NOTHING;
