# Auth 与 Billing 日志看板

## 配置导出并导入看板

1. 在 Cloudflare 的 **Observability → Destinations** 确认已有名为 `molis-grafana-logs` 的 Logs 目标，指向 Grafana Cloud OTLP endpoint 的 `/v1/logs`。将 Grafana 的 `Authorization` 头保存在 Cloudflare 目标中。[Cloudflare 配置说明](https://developers.cloudflare.com/observability/export/opentelemetry/grafana-cloud/)
2. Auth 和 Billing 的 `config/test.properties` 与 `config/prod.properties` 均已登记该目标，部署对应环境 Worker 后两项服务会导出到同一个 Loki 数据源。本地 `dev` 不支持配置导出。
3. 在 Grafana Cloud 打开 **Dashboards → New → Import dashboard**，上传 [grafana-auth-billing-dashboard.json](grafana-auth-billing-dashboard.json) 后点击 **Import**。在看板顶部选择接收两项服务日志的 Loki 数据源。[Grafana 导入说明](https://grafana.com/docs/grafana/latest/visualizations/dashboards/build-dashboards/import-dashboards/)

| 变量 | 用法 |
| --- | --- |
| `datasource` | 选择接收 Cloudflare OTLP 日志的 Loki 数据源。 |
| `env` | 默认为 `prod`，可切换到 `test`。测试环境启用导出后才有数据。 |
| `request_id` | 从响应头复制完整的 `X-Request-Id`，并选中请求发生的时间范围。清空时串联日志面板为空。 |

## 查询口径

- 核心 API 按服务分组，覆盖已识别的 `/api/v1/`、`/api/account/` 和 `/internal/v1/` 路由，排除健康及就绪检查。可用率为 `(2xx + 3xx) / (2xx + 3xx + 5xx)`，无符合条件的请求时显示无数据。QPS 和 P50/P95 包含客户端错误及服务端错误，窗口为 5 分钟。
- 4xx 和 5xx 各取所选时间范围内请求数最高的 10 组，按服务、HTTP 状态和 `error_code` 分组。错误统计也包含探活失败和未匹配路由。
- 定时任务耗时显示近 10 分钟内每项服务的最长已完成运行。失败按服务和 `request_id` 去重，统计近 15 分钟的失败运行。预算面板统计 Auth 的 `time_budget` 或 `round_limit`，以及 Billing 的 `budget_exhausted=true`。没有失败或耗尽事件时，相应面板可能显示无数据。
- Billing 的维护摘要中，`expire.processed` 是已过期订单数，`sync.processed` 是同步调用成功返回的账本主体数，`purge.processed` 是已删除的幂等回执数。每个步骤记录 `complete`、`failed` 或 `skipped`，单条处理失败计入该步骤的 `failed` 数。失败明细与摘要共用运行 ID。
- 串联查询只会显示实际记录了同一 ID 的应用事件。Auth 的部分成功内部调用没有独立请求日志。可先在 Explore 用下面的查询确认导出，并把时间范围缩小到请求发生前后。查询兼容原始应用 JSON 与应用 JSON 位于 `message` 的 Cloudflare 日志行。[Loki 日志查询语法](https://grafana.com/docs/loki/latest/query/log_queries/)

```logql
{service_name=~".+"} |= "REPLACE_WITH_REQUEST_ID"
| cloudflare_script_name=~"molis-(auth|billing)-production"
| json cf_message="message"
| line_format `{{ .cf_message | default (__line__) }}`
| drop cf_message
| json app_env="env", app_request_id="request_id"
| __error__="" | app_env="prod" | app_request_id="REPLACE_WITH_REQUEST_ID"
```

## 建议告警阈值

| 指标 | 建议起点 |
| --- | --- |
| 核心 API 5xx 率 | 近 5 分钟超过 1% 且窗口内至少有 20 条符合可用率口径的请求，持续 10 分钟。5xx 率为 1 减去可用率。 |
| 路由 P95 | 近 5 分钟超过 750 ms，持续 10 分钟。按高流量路由的基线调整，并针对外部供应商路径单独设阈值。 |
| 定时任务失败 | 近 15 分钟内任一服务有至少 1 次失败运行。对仅统计失败事件的查询，将无数据视为正常，并单独监控摘要缺失。 |
| 定时任务超时 | 近 15 分钟内至少 1 次预算耗尽，或近 10 分钟最长耗时达到 8,000 ms。对每 5 分钟运行一次的任务，15 分钟没有摘要也应告警，以覆盖硬超时和导出中断。 |

创建 Grafana 告警规则时，将看板变量替换为目标环境和数据源，按服务分别评估。用于检查摘要缺失的 `absent_over_time` 查询应分别匹配 `auth.cleanup` 和 `billing.maintenance`。[LogQL 指标查询语法](https://grafana.com/docs/loki/latest/query/metric_queries/)
