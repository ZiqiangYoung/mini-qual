# mini-qual

Linux NVMe SSD 高温顺序磨损、容量预测和断电 retention 准备 CLI。试验采用 JESD218C 企业级高温磨损基准 76.5°C、1200 小时，I/O 使用顺序负载。两项活化能采用颗粒厂商给出的磨损 1.1 eV 和 retention 0.8 eV。参见 [JESD218C](JESD218C.pdf) 表 4、§6.1.4 和附录 A。

## 构建与设备授权

```sh
cargo build --release --locked
target/release/mini-qual --help
fio --enghelp=libaio
```

需要 Linux、支持 libaio 的 Flexible I/O Tester 和能够执行 discard 的 NVMe namespace。可以用 `--fio /path/to/fio` 选择随包分发的 fio。nvme-cli 可供管理员核对设备，CLI 本身不依赖它。日志目录应放在系统盘或独立日志盘。

设备操作会 TRIM 和覆盖目标盘。管理员先核实序列号和节点，再只为试验账户授权对应的 namespace 块设备。例如节点为 `/dev/nvme2n1`，账户为 `qual` 时，由管理员执行以下命令。拔插或重启后重新核对节点和 ACL。

```sh
cat /sys/class/nvme/nvme2/serial
setfacl -m u:qual:rw /dev/nvme2n1
```

CLI 以普通用户执行。它按 SN 查找唯一控制器，要求唯一主机可见 namespace 为 NSID 1，读取主机可见容量和逻辑块大小，并检查当前 UID 的进程句柄和命令行。设备锁防止本工具的其他实例同时使用该盘，Linux 独占打开探测会发现内核已占用的设备，例如挂载中的分区。若试验还要求核查控制器已分配但未附加的 namespace，由管理员执行 `nvme list-ns /dev/nvme2 --all`，该列表的查询权限取决于内核和控制器。

Linux 可能拒绝读取持有额外 capability 的同 UID 进程的占用信息。CLI 继续检查可见的句柄和命令行，并在事件日志里列出检查范围受限的 PID。

## 预测试

在保持 76.5°C 的同型号专用盘上运行。将示例 SN、PE 和容量网格换成实际值。

```sh
mini-qual pretest --sn PRETEST_SSD_SN --pe 3.25 \
  --pretest-nand-capacity 4TiB --nand-capacity-step 128GiB \
  --minimum-nand-capacity 3TiB --maximum-nand-capacity 8TiB
```

每轮完成全盘 TRIM、顺序写、flush、顺序读和错误定位。终端即时显示每次 GET 的查询时间和壳温，查询失败时显示原因。至少取得两轮后按 Ctrl+C 停止。CLI 等待当时的 I/O 子进程退出，把已完成的轮次用于估算，并保留未完成轮次的日志。

容量推荐固定主机可见容量，只考虑逻辑块对齐、严格大于主机可见容量且位于指定范围内的候选值。候选 NAND 容量等于预测试盘当前 NAND 容量加上容量步长乘以档位偏移。例如当前容量为 4 TiB，步长为 128 GiB，档位偏移为 −2 时，候选容量为 3.75 TiB。

计算先从总主机写入预算中预留一个主机可见容量用于 retention，再按整轮和尾轮预测磨损时长。整轮使用各阶段和衔接的实测平均耗时，尾轮保留整盘 TRIM、flush 和阶段衔接耗时，按字节比例估算写、读和定位耗时。推荐值与 1200 小时的差值最小，等距时取较小值。报告另列实测温度下的预测等效磨损时长。用户依照推荐修改并烧录固件。

## 正式运行与断电交接

启动协调器并从健康接口取得本次 run ID。首次联调使用 dry-run。

```sh
./coordinator --dry-run
```

在另一终端运行以下命令，并将三个温度、时长字段里的示例值替换为试验方案值。

```sh
./barrierctl health
mini-qual run --sn FORMAL_SSD_SN --pe 3.25 --nand-capacity 4TiB \
  --run-id "$RUN_ID" \
  --retention-baseline-temperature-c 40 --retention-baseline-duration-hours 2160 \
  --retention-test-temperature-c 80
```

`RUN_ID` 为部署者从 health 输出取得的 UUID。先让本批次的所有 CLI 实例注册，然后由部署者执行 `./barrierctl close --run-id "$RUN_ID"`。使用 `./barrierctl status --run-id "$RUN_ID"` 查看进度。每个进程只操作一张盘，可对不同 SN 手动启动多个进程。长时间试验可通过 `--coordinator-task-timeout-ms` 指定可覆盖整场运行的任务时限，也可使用随附协调器支持的 `--task-timeout 0`。心跳只续租，任务总时限从注册起计。

PE 以 K 为单位，`3.25` 与 `3.25K` 均展开为 3250。容量可用字节、二进制单位（如 KiB、GiB、PiB）或十进制单位（如 GB、TB），上限 2 PiB。PE 必须展开为整数，已烧录的实际 NAND 容量必须按逻辑块对齐。主机可见容量以当前设备为准。

总主机写入预算等于 `PE × NAND 容量`。磨损写入预算等于 `总主机写入预算 − 主机可见容量`。每轮全盘 TRIM，顺序写和读回同一范围，余量不足一盘时只写读对应的起始 LBA 范围。读写用直接 libaio、128 KiB、QD32，末段按逻辑块对齐。普通读取记录失败请求的范围和 fio 状态，并在该轮普通读取结束后逐 LBA 细读。细读优先取 NVMe 命令状态，内核权限受限时记录直接读取的 Linux errno。普通读取中取得的错误记录一直保留。写入或 flush 发生错误时，CLI 终止该盘试验并保存日志。

磨损完成后紧接全盘 TRIM、全盘顺序写和一次 flush。此时累计主机写入正好达到总主机写入预算。CLI 保存计算结果和试验日志后按协议上报。成功与失败上报都解除该实例对断电协调器的等待。断电和烘烤由外部流程执行。参见 [Worker 接入 API](retention-poweroff-worker-api.md)。

## 壳温、时间和日志

默认 GET 地址为 `http://127.0.0.1:30030/devices/036JNGZES9000051/temperature`，采样间隔默认 60 秒，可通过 `--temperature-sample-interval-seconds` 调整。多盘各自使用对应传感器时传入 `--temperature-url`。支持 JSON 数值、常见 `temperature` / `case_temperature_c` 壳温字段，以及 `--temperature-pointer /data/value` 指定的摄氏数值。日志保留每次查询时间、响应完成时间、状态和原始响应正文。

按实际磨损窗口积分相对于 76.5°C 的 Arrhenius 速率，包含轮次中的 TRIM、写、flush、读、定位和轮次间隙。两次有效采样之间对速率做线性插值，端点用最近样本延展。磨损窗口内的有效采样间隔或端点延展超过三个采样周期时，CLI 记录原因。准备 retention 的 I/O 在磨损窗口之后。对同一基准温度，时长调整因子为 `max(0.5, 等效磨损小时数 / 1200)`。再用 retention 的 0.8 eV Arrhenius 因子，将用户提供的基准温度和时长换算到实际断电温度。

```sh
mini-qual retention --log-dir ./qual-logs/本次运行目录 \
  --retention-baseline-temperature-c 40 --retention-baseline-duration-hours 2160 \
  --retention-test-temperature-c 80
```

每次调用创建独立日志目录。主要输出如下。

| 文件 | 内容 |
| --- | --- |
| `events.jsonl` | 参数、阶段顺序、字节数、带宽、累计写入和终止原因 |
| `temperature.jsonl` | 逐次 REST 查询、原始响应和实测壳温 `measured_case_temperature_c` |
| `*.fio.stdout`、`*.fio.stderr` | fio 原始输出 |
| `*_bw.1.log` | fio 在运行时按秒采样、阶段退出时导出的带宽序列 |
| `*errors.jsonl` | 普通读取的失败范围、细读错误、可取得的返回状态 |
| `capacity-recommendation.json` | 预测试模型、建议 NAND 容量 `nand_capacity_bytes` 和档位偏移 `nand_capacity_step_offset` |
| `wear-window.json`、`retention.json` | 正式磨损窗口与 retention 建议的计算依据 |

## 验证

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

测试覆盖预算、容量网格、温度模型及 REST 记录、设备筛选、fio 尾段和读取错误。默认测试无需 NVMe。协调器 dry-run 自验另见 `tests/worker.rs` 中的调用方式。
