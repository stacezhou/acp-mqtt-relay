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
- `stdin` / `stdout` / `stderr` 双向桥接
- JSON 分帧消息格式，内容使用 base64 编码
- **QoS 1 + 应用层序号的消息顺序保证和乱序处理**
- 本地 Mosquitto 容器端到端验证

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
  [--username USERNAME] \
  [--password PASSWORD]
```

serve 模式（work 端）：

```bash
amr --serve my-agent \
  --broker localhost \
  --command "cat" \
  [--username USERNAME] \
  [--password PASSWORD]
```

说明：

- `--broker` 支持 `localhost` 或 `mqtt://host:port` 形式
- `<node-id>` 是必填位置参数，用于推导 topic
- `--serve --command` 通过系统 shell 启动子进程
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
acp/{node-id}/in
acp/{node-id}/out
```

流向：

- `user` 发布到 `acp/{node-id}/in`，订阅 `acp/{node-id}/out`
- `work` 发布到 `acp/{node-id}/out`，订阅 `acp/{node-id}/in`

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
  --command "cat"
```

### 3. 启动 user 端

```bash
./target/release/amr \
  e2e-local \
  --broker localhost
```

### 4. 验证回环

在 `user` 端输入：

```text
Hello ACP!
```

如果链路正常：

- `work` 端会记录收到 MQTT 消息并写入 `cat`
- `user` 端会看到经由 MQTT 回来的 `Hello ACP!`

### 5. 清理测试容器

```bash
docker rm -f acp-mqtt-test
```

## Verification

当前已完成的本地验证：

```bash
cargo check
cargo test
cargo run -- --help
cargo run -- --serve --help
```

另外已经用本地 Mosquitto 容器完成过一轮真实端到端验证：

- `amr --serve e2e-local --command cat`
- `amr e2e-local` 发送 `Hello ACP!\n`
- user 端成功收到原样回显

## Architecture

逻辑结构如下：

```text
local stdin/stdout
    |
    v
[ user relay ] <-> MQTT Broker <-> [ work relay ] <-> child process stdio
```

如果上层程序是 ACP agent，那么整体效果就是 ACP 所需的 `stdio` 被搬运到了 MQTT 之上。

## Roadmap

- 区分控制流和大流量输出的 QoS 策略
- 增加 TLS / MQTT 鉴权配置
- 增加端到端加密
- 增加会话管理和更稳健的断线恢复
