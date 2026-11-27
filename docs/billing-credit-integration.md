# Billing Credit 接线

在支付模块的 `AppendFirstCredit` 回调中，把 `credit::prepare_paid_first_month` 返回的语句追加到支付批次。回调在批次内标记订单为 `PAID` 并写入服务期之后执行，发放、流水、审计与支付结果一起提交。提前续费的额度在服务期开始时由 `credit::sync_credit_periods` 发放。退款只适用于尚未生效的服务期，此时对应额度尚未发放。

```rust
fn append_first_credit(
    db: &D1Database,
    payment: &orders::FirstCredit<'_>,
    batch: &mut Vec<D1PreparedStatement>,
) -> Result<(), ApiError> {
    let scope = credit::CreditScope { owner: payment.owner, product: payment.product };
    batch.extend(credit::prepare_paid_first_month(
        db,
        &scope,
        credit::FirstMonthGrant {
            order_id: payment.order_id,
            anchor: payment.anchor,
            period: &payment.period,
            monthly_credits: payment.monthly_credits,
            paid_at: payment.now,
            request_id: payment.request_id,
        },
    )?);
    Ok(())
}
```

集成分发在 `authorize_user` 返回后，将 `CreditBalance` 和 `CreditLedger` 交给 `credit::handle_user(&route, &request, &env, &user, &request_id)`。内部 Credit 路由交给 `credit::handle_internal(&route, request, &env)`，由该模块复核共享密钥并拒绝会话凭证。公开入口沿用前置 Worker 的路由隔离。

集成构建把 `credit-fixture` 与订单特性保留在 `Cargo.toml` 的同一 `[features]` 节，并在 `rust-build.mjs` 保留两种验收参数。Credit fixture 仅用于显式启用的本地编译，运行时要求 Miniflare 的环境标记和 fixture 开关，以注入受控时钟并验证支付批次回滚。

```sh
npm --prefix worker run build:rust -- --credit-fixture
node --test worker/tests/credit.acceptance.ts
```
