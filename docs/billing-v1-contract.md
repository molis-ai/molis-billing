# Billing v1 接口与代码交接

本文件约定 Auth、Billing 和本地调用方连接时使用的字段及模块。

## 用户侧转发与鉴权

Auth 的浏览器路由为 `/api/account/billing` 加下表路径。Billing 的入站路由为 `/api/v1` 加同一路径。Auth 从 HttpOnly Cookie 取得当前网页会话，写请求沿用现有 CSRF 校验。Auth 按原 HTTP 动词转发 JSON 和查询串，并透传 `Idempotency-Key` 与 `X-Request-Id`。Billing 负责匹配具体路由、验证查询和请求字段、校验必填幂等键以及返回业务错误。Auth 将服务端取得的中央会话访问凭证作为 `Authorization: Bearer <access token>`，和 `X-Molis-Internal-Secret` 一起发送给 Billing。

| 方法 | 路径 | 访问 |
| --- | --- | --- |
| GET | `/catalog/plans` | 本人、有效团队成员 |
| GET | `/subscriptions` | 本人、有效团队成员 |
| GET | `/orders`、`/orders/:id` | 本人、当前团队 OWNER 或 ADMIN |
| POST | `/orders`、`/orders/:id/mock-pay`、`/orders/:id/refund` | 本人、当前团队 OWNER 或 ADMIN |
| GET | `/credits/balance` | 本人、有效团队成员 |
| GET | `/credits/ledger` | 本人、当前团队 OWNER 或 ADMIN，MEMBER 仅见本人发起的 HOLD、SETTLE、RELEASE、CONSUME |
| POST | `/entitlements/check`（可选） | 本人、有效团队成员 |

用户侧请求的查询参数为 `product=WORK`（缺省 WORK）以及`ownerKind=USER|TENANT`（缺省 USER）。TENANT 必须提供所选 `ownerId`。USER 从鉴权得出本人 ID，客户端若提供 ID 则必须与本人一致。Billing 每次重新校验目标主体及当前团队角色，列表查询先按 product、ownerKind、ownerId 和可见角色过滤。团队管理员可以处理该主体内其他创建者的订单。范围外订单按 404 处理。客户端提交的操作者或角色不构成授权证据。

### Auth 桥接

Billing 使用 `AUTH` 绑定分别连接 `molis-auth-test`、`molis-auth-production` 的命名入口 `AuthInternal`。Auth 的 `BILLING` 绑定连接各自环境 Worker 的命名入口 `BillingInternal`。两个方向的 HTTP 请求使用 `X-Molis-Internal-Secret`，值来自同环境的 `MOLIS_INTERNAL_SECRET`，为 32 字节随机值的无 padding base64url 编码。在本地 dev 允许仅从回环访问 Auth `http://127.0.0.1:8787` 和 Billing `http://127.0.0.1:8788`。绑定使用 `https://auth.internal` 和 `https://billing.internal` 作为请求 URL。Worker 通过命名入口隔离用户账务及内部账务路由，并校验共享密钥。公开域名继续承载探针和独立验签的支付回调。

| Auth 路由 | 请求 | 成功响应 |
| --- | --- | --- |
| `POST /internal/v1/subjects/verify` | 服务端 Bearer 和共享密钥，JSON `{"product":"WORK","owner":{"kind":"TENANT","id":"tenant-id"}}`，USER 的 id 可省略 | `{"data":{"userId":"user-id","product":"WORK","owner":{"kind":"TENANT","id":"tenant-id"},"role":"ADMIN"}}` |

`subjects/verify` 使用同一次 Auth 一致性读取验证会话与该主体，角色为 `SELF`（USER）或 `OWNER` / `ADMIN` / `MEMBER`（TENANT）。成功响应只用于本次操作。桥接不续活。Billing 在敏感写入和成功收据回放前都使用刚取得的验证结果。Auth 与 Billing 使用各自 D1，权限更新发生在验证返回与账务提交之间时，跨库当前请求无法原子撤回。下一次请求和回放重新验证。Billing 批次独立校验账务并发条件。

### 数据与分页

成功 envelope 为 `{"data":...}`，错误 envelope 为 `{"error":{"code":"...","message":"..."},"requestId":"..."}`，响应头 `X-Request-Id` 与错误正文中的 ID 一致。时间字段是 UTC Unix 毫秒整数，金额是币种的最小货币单位整数，Credit 是整数。标识符是字符串。JSON DTO 字段采用 camelCase。下列列表对象为 `data: {items, nextCursor}`，其中 `nextCursor` 可为 null。

- 订单、流水和授权的审计列表使用 `limit` 和 `cursor`。默认 20，允许 1–100，按 `created_at DESC, id DESC` 稳定排序。读取 `limit+1` 条以生成下一页位置，下一页在已过滤范围内使用 `(created_at,id) < (?,?)`。客户端原样带回服务端返回的 cursor。
- 目录返回 `data: {items}`，每项为 `{planId, planVersion, product, ownerKind, name, currency, monthlyPriceMinor, yearlyPriceMinor, monthlyCredits}`。只返回相应主体类型在售的当前版本，按 planId 排序。
- `POST /orders` 请求为 `{planId, interval, billingTimeZone?}`。interval 为 `MONTH` 或 `YEAR`，时区省略时用 Asia/Shanghai。成功是 HTTP 201，响应 data 为 Order。订单详情和续付均使用原订单 ID。支付和退款请求体可省略或为空对象，成功是 HTTP 200，data 为 Order。三个订单写入路由都要求 `Idempotency-Key`。由服务端确定金额、版本、操作者和操作时刻。
- Order 字段为 `{id, product, ownerKind, ownerId, actorUserId, planId, planVersion, interval, currency, amountMinor, monthlyCredits, billingTimeZone, status, createdAt, paymentExpiresAt, paidAt, refundedAt, servicePeriod, refundable}`。状态为 `PENDING_PAYMENT` / `EXPIRED` / `PAID` / `REFUNDED`。servicePeriod 在支付前为 null，否则为 `{orderId, startsAt, endsAt, revokedAt}`。paidAt、refundedAt、revokedAt 可为 null。退款额为整单 amountMinor。refundable 由服务端时间和状态判定。
- 订阅读取为 `{product, ownerKind, ownerId, billingTimeZone, currentPeriod, futurePeriod, paidThroughAt}`。无有效数据时后四项各按实际情况为 null。两个 Period 字段使用 `{startsAt, endsAt, planId, planVersion, planName, interval, monthlyCredits}`；Order 权限用户另见对应 orderId。
- 余额为 `{product, ownerKind, ownerId, available, granted, reserved, consumed, currentCreditPeriod}`。currentCreditPeriod 为 `{startsAt, expiresAt}` 或 null，查询错误走错误 envelope。流水项为 `{id, kind, creditPeriodId, holdId, deltaGranted, deltaAvailable, deltaReserved, deltaConsumed, deltaExpired, referenceId, actorUserId, callingService, requestId, occurredAt, createdAt}`，可空字段按存储结果返回。可选权益检查使用相同查询参数和空请求体，响应为 `{active, planId, endsAt}`，后两项无权益时为 null。

### 平台审计

| 方向 | 方法与路径 | 请求 | 成功响应 |
| --- | --- | --- | --- |
| 平台服务端 → `BillingInternal` | `GET /internal/v1/audit-events` | `Authorization: Bearer <access token>`、`X-Molis-Internal-Secret`，可选 `X-Request-Id`、`limit`、`cursor` | `200 {"data":{"items":[...],"nextCursor":null}}`，有后续记录时 `nextCursor` 为字符串 |
| Billing → `AuthInternal` | `POST /internal/v1/platform/audit/authorize` | 同一服务端 Bearer、共享密钥、请求 ID，JSON `{}` | `200 {"data":{"userId":"user-id","permission":"AUDIT_READ"}}` |

Billing 每页都向 Auth 复核会话和平台 `AUDIT_READ`。Auth 授权端须在同一次 D1 一致性读取中检查会话、客户端和该用户的有效平台权限，沿用桥接的验票及客户端规则且不更新 `last_active_at`。调用者提供的角色、权限和主体字段不参与授权。团队角色本身不授予平台审计访问。线上走同环境私有 Service Binding，本地走配置的回环通道。用户 BFF `/api/account/billing/*` 无此转发。Auth `GET /api/v1/platform/audit-events` 继续查询 `auth_management_audit` 并使用原合同。

审计列表从 Billing D1 的 `billing_audit` 跨主体分页，查询参数、顺序和游标沿用上节。每项为 `{id, callingService, actorUserId, operation, targetId, product, ownerKind, ownerId, requestId, details, createdAt}`。`details` 返回已写入的 JSON 值，系统操作的 `actorUserId` 为 null。

分页参数无效返回 `400 INVALID_REQUEST`。缺少会话凭证返回 `401 UNAUTHENTICATED`，会话失效返回 `401 SESSION_EXPIRED`，缺平台审计权限返回 `403 FORBIDDEN`。公网入口或错误内部密钥返回 `404 NOT_FOUND`。Auth 的绑定、授权路由或其响应不可用返回 `503 AUTH_UNAVAILABLE`，Billing 的内部密钥配置或 D1 不可用返回 `503 BILLING_UNAVAILABLE`。

### 后端账务与支付回调

- 可信调用方经 Service Binding + 共享密钥调用 `POST /internal/v1/credits/holds`、`/internal/v1/credits/holds/:id/settle`、`/internal/v1/credits/holds/:id/release`、`/internal/v1/credits/consume`。Auth BFF 路由表不包含这些路径。每个请求带 `callingService`、`actorUserId`、`product`、`owner: {kind,id}`、`referenceId`、`occurredAt` 和幂等请求头。调用者对其产品操作及金额负责，账务模块复核主体、任务引用和余额。预扣/直扣携带 `amount`，预扣携带 `expiresAt`。内部写入要求 application/json，正文上限 16 KiB。每次结算最多 16 段。结算提交 `segments: [{occurredAt, amount}]`，释放定位原 hold 并释放剩余预留。预扣成功为 201，返回 data.hold。结算、释放、直扣成功为 200，返回 `data: {hold, settledSegments}`，直扣的 hold 为 null。Hold 的公共字段为 `{id, creditPeriodId, referenceId, status, amount, reserved, settled, released, expired, expiresAt}`。settledSegments 记录实际用量落账段，释放时为空。
- 跨期结算第二段余额不足时，402 的 data 包含 `settledSegments` 和 `failedSegment`，各段至少包含 `occurredAt`、`amount`，成功段包含 ledgerId。旧段先独立提交，重试按已落账的段复用结果。
- `POST /api/v1/webhooks/:provider` 由支付适配器校验原始请求签名，持久事件键为 `(provider, external_event_id)`，规范化摘要包含类型、提供方支付 ID、订单、金额和币种。确认与回调复用相同的履约结果。持久事件冲突返回 409。
- 配置或响应异常统一映射为 `503 AUTH_UNAVAILABLE` 或 `503 BILLING_UNAVAILABLE`。非私有入口或错误内部密钥在 Billing 返回 404。账务响应由 `http::json_response` 统一添加 no-store 和安全响应头。运行探针保留既有响应。

## Billing 模块

- `worker/rust/src/app.rs` 集中匹配公开探针、用户账务和内部账务路由，并在分发前校验相应通道。请求上下文统一传递请求 ID 和处理时间。`core/` 提供 HTTP、身份、D1、幂等、分页和日历实现。`orders/payments/` 管理支付和退款，`credit/` 管理额度，`maintenance.rs` 执行定时清理。
- `identity::authorize_user` 对目标调用 subjects/verify，并执行 `UserAccess::Read` / `Manage`。`VerifiedUser::ledger_actor_filter` 提供成员可见性过滤所需的 actor。在服务绑定缺失或密钥缺失时封闭失败。
- `audit::handle` 在私有入口校验共享密钥，经 `identity::authorize_platform_audit` 检查平台权限后使用 `billing_audit_page` 索引读取分页。
- `catalog::find_quote` 供订单节点查当前在售版本。创建订单在同一批次复核 current_version/sale_status，按该版本冻结 amount_minor、currency、monthly_credits、interval。支付后继续按已冻结版本履约。
- `calendar::purchase_period` 共用首购/过期重买、月转年、年转月安排。订阅保存 anchor_at 和 billing_time_zone，每单服务期保存 month_offset、month_count 与起止 UTC 毫秒。Credit 用 `calendar::boundary_at` 从原锚点推算月边界，账务读取用 `covers`、退款用 `refund_allowed`。
- `storage::guard` 将 SQL 布尔条件作为同一原子 batch 内的断言。用 `storage::commit` 提交 guard、余额或状态更新、审计、流水和收据。Guard 失败会回滚整批，业务模块回读最新状态区分过期、已购未来一期和版本冲突。禁止用批次外预读代替提交点条件。
- `idempotency::ReceiptKey` 按调用服务、操作者、操作和主体隔离键。调用 `request_hash` 传入该键的作用域、明确目标 ID 和规范化业务参数。先鉴权，再查收据，摘要相同回放原状态和响应，摘要不同冲突。并发提交时将 guard、业务结果、审计和收据插入置于同一批次，竞争失败后回读收据。若匹配则用存储的状态和 JSON envelope 回放，若无收据再按提交点业务条件处理。清理按 expires_at 进行，事件去重行独立保留。
- `pagination::Page` 解析分页参数，`pagination::Cursor` 提供绑定顺序所需的创建时间和 ID。
- SQL 表结构及索引在 `worker/migrations/0001_billing_baseline.sql`。Credit 周期行与 hold 行的计数约束求和守恒。流水 `delta_*` 以周期为键求和可重建余额和消费。GRANT、EXPIRE、REVOKE 各周期最多一条。服务期的撤销时间保存为 revoked_at，后续重新购买可复用该月偏移。未来一期库存由下单支付的提交点 guard 检查，未支付订单不占库存。

### 审计操作

| 动作 | operation |
| --- | --- |
| 创建、支付、退款、超时关闭订单 | `billing.order.create`、`billing.order.pay`、`billing.order.refund`、`billing.order.expire` |
| 发放、归档额度 | `billing.credit.grant`、`billing.credit.expire` |
| 预扣、预扣超时 | `billing.credit.hold`、`billing.credit.hold.timeout` |
| 结算、主动释放、直扣 | `billing.credit.settle`、`billing.credit.release`、`billing.credit.consume` |
