# `amr`

A lightweight `stdio over MQTT` relay for ACP-style agent workflows, written in Rust.

`amr` 当前的 MVP 本质上是一个双端对称的 `stdio` 桥接器：

- `user` 端读取本地 `stdin`，封装后发布到 MQTT
- `work` 端从 MQTT 订阅消息，写入远端子进程的 `stdin`
- `work` 端读取子进程的 `stdout/stderr`，再经 MQTT 发回
- `user` 端收到后写回本地 `stdout/stderr`

它本身不解析 ACP 协议，而是透明搬运 ACP 所依赖的标准输入输出通道。因此它同样可以用于任意基于 `stdio` 交互的命令行程序。

## Current Status

当前已实现：

- Rust 单二进制程序
- 默认 user 端 + `--serve` work 端单入口 CLI
- MQTT Broker 连接、订阅、发布和重连后重订阅
- **同一 `node_id` 下多 user session 并发，每个 session 对应独立 work 子进程**
- `stdin` / `stdout` / `stderr` 双向桥接
- JSON 分帧消息格式，内容使用 base64 编码
- `spawn` / `heartbeat` / `shutdown` 控制消息
- **QoS 1 + 应用层序号的消息顺序保证和乱序处理**
- `amr` 默认静默日志和 `--log` 文件日志

当前未实现：

- QoS 分级策略
- 端到端加密
- 多节点发现和管理
- ACP 协议级优化

## Build

要求：

- Rust toolchain
- 可选：Docker，用于本地 Mosquitto 测试

构建：

```bash
cargo build --release
```

可执行文件位于：

```bash
./target/release/amr
```

## CLI

查看帮助：

```bash
cargo run -- --help
cargo run -- --serve --help
```

默认模式（user 端）：

```bash
amr my-agent \
  --broker localhost \
  [--log /tmp/amr-user.log] \
  [--username USERNAME] \
  [--password PASSWORD]
```

serve 模式（work 端）：

```bash
amr --serve my-agent \
  --broker localhost \
  --command "cat" \
  [--log /tmp/amr-work.log] \
  [--username USERNAME] \
  [--password PASSWORD]
```

说明：

- `--broker` 支持 `localhost` 或 `mqtt://host:port` 形式
- `<node-id>` 是必填位置参数，用于推导 topic
- `--serve --command` 通过系统 shell 启动子进程
- `--log` 仅记录 `amr` 自身日志到文件；默认不向终端输出日志，避免污染 ACP 使用的 `stdout` / `stderr`
- CLI 参数优先于配置文件

## Config

支持的配置文件路径：

- 首选：`~/.config/amr/config.yaml`
- 兼容旧路径：`~/.config/acp-mqtt-relay/config.yaml`

当前可从配置文件读取的字段：

- `broker`
- `username`
- `password`

`node_id` 不再从配置文件读取，必须通过命令行位置参数显式提供。

示例：

```yaml
broker: mqtt://localhost:1883
username: my-user
password: my-password
```

当前 topic 规则：

```text
acp/{node-id}/control
acp/{node-id}/{client-id}/in
acp/{node-id}/{client-id}/out
```

流向：

- `user` 每次启动都会生成一个随机 `client_id`
- `user` 发布控制消息到 `acp/{node-id}/control`
- `user` 发布 stdin 到 `acp/{node-id}/{client-id}/in`，订阅 `acp/{node-id}/{client-id}/out`
- `work` 订阅 `acp/{node-id}/control` 和 `acp/{node-id}/+/in`
- `work` 为每个 `client_id` 维护独立子进程，并发布输出到 `acp/{node-id}/{client-id}/out`

生命周期：

- user 启动后发送 `spawn`
- user 运行中定期发送 `heartbeat`
- user 正常退出时 best effort 发送 `shutdown`
- work 端如果连续 48 小时未收到某个 session 的控制或输入消息，会自动回收该子进程

## Message Format

MVP 使用按消息逐条发布的 JSON 结构：

```json
{
  "seq": 1,
  "type": "stdin",
  "content": "SGVsbG8gQUNQIQo=",
  "encoding": "base64"
}
```

其中：

- `seq` 为递增的应用层消息序号，用于保证顺序和去重
- `type` 为 `stdin`、`stdout` 或 `stderr`
- `content` 为原始字节的 base64 编码
- `encoding` 当前固定为 `base64`

控制 topic 使用单独的 JSON 消息：

```json
{
  "client_id": "18e3f7f4f7d-1a2b-1",
  "action": "heartbeat",
  "sent_at_ms": 1775550000000
}
```

其中：

- `client_id` 为 user 进程启动时生成的唯一会话 ID
- `action` 为 `spawn`、`heartbeat` 或 `shutdown`
- `sent_at_ms` 为发送时间戳，便于排障和后续扩展

## Quick Start

### 1. 启动本地 Broker

仓库里已经提供了一个最小 Mosquitto 配置：

- [docker/mosquitto.conf](/Users/zhouh/Repos/acp-mqtt-relay/docker/mosquitto.conf)

启动容器：

```bash
docker run -d \
  --name acp-mqtt-test \
  -p 1883:1883 \
  -v "$(pwd)/docker/mosquitto.conf:/mosquitto/config/mosquitto.conf:ro" \
  eclipse-mosquitto:2.0
```

### 2. 启动 work 端

```bash
./target/release/amr \
  --serve \
  e2e-local \
  --broker localhost \
  --log /tmp/amr-work.log \
  --command "cat"
```

### 3. 启动 user 端

```bash
./target/release/amr \
  e2e-local \
  --broker localhost \
  --log /tmp/amr-user.log
```

### 4. 验证回环

在 `user` 端输入：

```text
Hello ACP!
```

如果链路正常：

- `user` 端会看到经由 MQTT 回来的 `Hello ACP!`
- `work` 端会为该 user 的 `client_id` 创建一个独立 `cat` 子进程
- `/tmp/amr-work.log` 和 `/tmp/amr-user.log` 中会出现 `amr` 自身日志，终端不会被内部日志污染

### 5. 验证多 session 并发

在另一个终端再启动一个 user：

```bash
./target/release/amr \
  e2e-local \
  --broker localhost
```

此时两个 user 会拿到不同的 `client_id`，分别连接到各自的 work 子进程，输入输出互不串线。

### 6. 清理测试容器

```bash
docker rm -f acp-mqtt-test
```

## Verification

当前已完成的本地验证：

```bash
cargo fmt
cargo check
cargo test
cargo run -- --help
cargo run -- --serve --help
```

建议的端到端验证：

- `amr --serve e2e-local --command cat`
- 同一 `node_id` 下启动两个 `amr e2e-local`
- 验证两个 user 的输出互不串线
- 关闭某个 user，确认对应 work 子进程被清理
- 验证默认终端无 `amr` 自身日志，传入 `--log` 时日志写入指定文件

## Architecture

逻辑结构如下：

```text
[ user relay: client-a ] --\
                            \
                             -> MQTT Broker <-> [ work relay ] <-> [ child: client-a ]
                            /
[ user relay: client-b ] --/                               \-> [ child: client-b ]
```

如果上层程序是 ACP agent，那么整体效果就是 ACP 所需的 `stdio` 被搬运到了 MQTT 之上。

## Roadmap

- 区分控制流和大流量输出的 QoS 策略
- 增加 TLS / MQTT 鉴权配置
- 增加端到端加密
- 增加会话管理和更稳健的断线恢复
