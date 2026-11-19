# Worker 接入 API v1

本文为待实现的接口约定，可独立提供给 worker 实现方。

## 基本约定

- 地址：默认 `http://127.0.0.1:17653`，由部署方提供；不使用鉴权或 TLS，HTTP 客户端应绕过代理。
- 部署方提供固定 `run_id`（UUID）。worker 为每次进程执行生成一个 `instance_id`（UUID v4）；请求重试复用该 ID，进程重启生成新 ID。
- 下文 `{R}`、`{I}` 分别替换为这两个 UUID。worker 不得自动切换到其他 run_id。
- 请求体使用 UTF-8 JSON，设置 `Content-Type: application/json`，最大 16 KiB。可选请求字段不用时省略，不传 `null`。客户端忽略未知响应字段。
- worker 必须先确认注册成功且状态为 `active`，才能开始任务。注册关闭由部署方负责，worker 不调用关闭接口。

## 1. 注册

`PUT /v1/runs/{R}/workers/{I}`

```json
{"name":"export-worker","task_timeout_ms":3600000}
```

| 字段 | 约束 |
| --- | --- |
| name | 必填字符串，1～128 个 UTF-8 字节，允许重名 |
| task_timeout_ms | 可选正整数，单位毫秒；省略使用服务端默认值（默认 3600000）；超出服务端计时范围返回 400 |

首次成功返回 `201`；相同 ID、相同有效参数的重复注册返回 `200`。响应格式相同：

```json
{
  "run_id":"11111111-1111-4111-8111-111111111111",
  "instance_id":"22222222-2222-4222-8222-222222222222",
  "state":"active",
  "heartbeat_interval_ms":5000,
  "heartbeat_timeout_ms":30000,
  "task_timeout_ms":3600000
}
```

示例 UUID 分别代表请求路径中的 R、I；时间参数以实际响应为准。

重复注册不刷新心跳、不重置任务时限；省略超时与显式传入服务端默认值视为相同。注册关闭后，已有实例仍可重放注册，新实例被拒绝。重放响应可能已经是终态，此时不得启动任务。

## 2. 心跳

`POST /v1/runs/{R}/workers/{I}/heartbeat`

请求体：`{}`。成功返回 `200`：

```json
{"instance_id":"22222222-2222-4222-8222-222222222222","state":"active"}
```

注册成功后立即启动独立心跳任务，按返回的 `heartbeat_interval_ms` 周期发送，避免被业务阻塞。服务端收到有效心跳后刷新租约。

## 3. 完成

`POST /v1/runs/{R}/workers/{I}/complete`

```json
{"outcome":"succeeded","message":"export finished"}
```

| 字段 | 约束 |
| --- | --- |
| outcome | 必填，只能为 `succeeded` 或 `failed`；两者都解除等待 |
| message | 可选字符串，最多 4096 个 UTF-8 字节，仅作诊断 |

成功返回 `200`：

```json
{"instance_id":"22222222-2222-4222-8222-222222222222","state":"succeeded","duplicate":false}
```

重复上报相同 outcome 返回 `200`、`duplicate:true`，保留首次消息和完成时间；不同 outcome 返回 `409 result_conflict`。已经失联或执行超时的实例返回 `409 worker_terminal`，不能补报覆盖。

**上报前必须完成保存结果、提交事务、等待必要子任务和清理。** 上报后系统可能立即开始关机，不等待 worker 进程退出。上报期间保持心跳，直到完成得到确认或获知实例已进入终态。

## 4. 查询状态（可选）

`GET /v1/runs/{R}`，无请求体，成功返回 `200`。worker 只需读取以下响应字段：

| 字段 | 含义 |
| --- | --- |
| run_id | 当前批次 UUID |
| workers | 全部已注册实例数组 |
| workers[].instance_id | 实例 UUID，用于找到自身记录 |
| workers[].state | 下表中的状态 |
| workers[].message | 完成消息，无消息时为 null |

完整响应含其他批次字段，worker 可忽略。未找到自己的 ID 表示尚无该实例记录；终态记录不会被删除。

| 状态 | 含义 |
| --- | --- |
| active | 仍在等待 |
| succeeded | 已记录业务成功 |
| failed | 已记录业务失败 |
| lost | 心跳超时，已解除等待 |
| timed_out | 任务总时限到期，已解除等待 |

除 active 外均为不可逆终态。服务端从首次注册成功开始计时：首次心跳前以注册时刻计算租约，之后以最后一次有效心跳计算。**心跳不延长任务总时限。** 服务端处理请求时若当前时刻已达到任一期限，先判超时，再处理请求；迟到的心跳或完成不能恢复实例。

## 5. 错误与处理

错误响应统一为：

```json
{"error":{"code":"worker_terminal","message":"worker already reached terminal state","current_state":"lost"}}
```

`code`、`message` 必有；`current_state` 在 worker_terminal 时提供；run_mismatch 提供 `error.current_run_id`。程序依据 code 判断，不解析 message。

| HTTP | error.code | 处理 |
| --- | --- | --- |
| 400 | invalid_request | 修正 JSON、字段类型、UUID 或参数范围 |
| 404 | worker_not_found | 心跳/完成的实例未注册；检查 ID 与注册结果 |
| 409 | run_mismatch | 批次不符，停止本次接入并报告；不自动换批次 |
| 409 | registration_closed | 新实例被拒绝，不启动任务 |
| 409 | registration_conflict | 同一 ID 的注册参数不同，修正调用逻辑 |
| 409 | result_conflict | 已记录另一 outcome，不覆盖；查询并报告冲突 |
| 409 | worker_terminal | 停止心跳；若仍有业务，尽力取消和清理 |
| 413 | payload_too_large | 缩小请求体 |
| 415 | unsupported_media_type | 使用 application/json |
| 500 | internal_error | 按暂时故障重试 |

网络错误、超时或 5xx 可重试；4xx 不自动重试。注册/完成重试必须保持原 ID 和请求参数，不能因响应丢失生成新 ID 或改变 outcome。

建议单次 HTTP 超时 2 秒，失败退避从 200 毫秒倍增至最多 2 秒；注册、完成各自重试总预算 30 秒，心跳失败则在下一周期继续发送。预算均不能突破服务端期限。

- 注册预算耗尽：不启动任务，报告“注册未确认”并退出。
- 完成预算耗尽：查询自身状态；succeeded/failed 表示已记录结果，lost/timed_out 表示异常解除等待。若仍为 active、记录不存在或无法查询，报告“完成未确认”，停止心跳并退出。已注册且仍 active 的记录之后由服务端超时处理。
- 完成响应可能因系统关机而丢失，连接失败不代表完成未被记录。心跳与完成并发时，心跳收到 succeeded/failed 终态可作为完成结果确认；结果与预期不同则报告冲突。

## 最小接入流程

```text
固定 run_id → 生成 instance_id → 注册并确认 active
→ 启动独立心跳 → 执行业务及必要清理
→ 上报最终 succeeded/failed（保持心跳）
→ 确认终态 → 停止心跳并退出
```

业务内部重试应在同一次注册和固定任务时限内完成，只上报最终结果。dry-run 与真实关机模式使用相同协议，worker 无需区分。
