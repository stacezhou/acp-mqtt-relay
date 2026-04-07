import time
import datetime
import threading
import signal
import sys
import argparse
from collections import defaultdict
from paho.mqtt import client as mqtt_client
from rich.console import Console
from rich.table import Table
from rich.live import Live

# --- 参数解析 ---
def parse_args():
    parser = argparse.ArgumentParser(description="ACP Topic 活跃度监控工具")
    parser.add_argument("--broker", type=str, default="127.0.0.1", help="MQTT Broker 地址 (默认: 127.0.0.1)")
    parser.add_argument("--port", type=int, default=1883, help="MQTT Broker 端口 (默认: 1883)")
    parser.add_argument("--topic", type=str, default="acp/+/+/out", help="订阅的主题过滤器 (默认: acp/+/+/out)")
    parser.add_argument("--user", type=str, default=None, help="MQTT 用户名 (可选)")
    parser.add_argument("--pw", type=str, default=None, help="MQTT 密码 (可选)")
    return parser.parse_args()

args = parse_args()

# --- 配置常量 ---
INACTIVE_THRESHOLD = 60  # 60秒无消息视为不活跃
CLEANUP_INTERVAL = 3600  # 每小时清理一次已彻底离线的 Topic

# --- 数据中心 ---
class StatsManager:
    def __init__(self):
        self.data = defaultdict(lambda: {
            "last_ts": 0, 
            "today_sec": 0, 
            "week_sec": 0, 
            "last_update_day": datetime.date.today()
        })
        self.lock = threading.Lock()

    def update(self, topic):
        now = time.time()
        today = datetime.date.today()
        
        with self.lock:
            record = self.data[topic]
            
            # 跨天/跨周重置检测
            if record["last_update_day"] != today:
                if today.weekday() == 0: # 周一重置周统计
                    record["week_sec"] = 0
                record["today_sec"] = 0
                record["last_update_day"] = today

            # 计算活跃时长
            if record["last_ts"] > 0:
                diff = now - record["last_ts"]
                if diff <= INACTIVE_THRESHOLD:
                    record["today_sec"] += diff
                    record["week_sec"] += diff
            
            record["last_ts"] = now

    def cleanup(self):
        now = time.time()
        with self.lock:
            keys_to_del = [t for t, d in self.data.items() if (now - d["last_ts"]) > 86400]
            for t in keys_to_del:
                del self.data[t]

stats_manager = StatsManager()

# --- MQTT 回调 ---
def on_message(client, userdata, msg):
    stats_manager.update(msg.topic)

def on_connect(client, userdata, flags, rc):
    if rc == 0:
        client.subscribe(args.topic)
    else:
        print(f"连接失败，错误码: {rc}")

# --- UI 渲染 ---
def generate_table():
    table = Table(title=f"ACP 监控 [bold blue]{args.broker}:{args.port}[/] - [yellow]{datetime.datetime.now().strftime('%H:%M:%S')}[/]", 
                  caption=f"订阅范围: {args.topic} | 阈值: {INACTIVE_THRESHOLD}s")
    
    table.add_column("Topic (Node/Client)", style="cyan", no_wrap=True)
    table.add_column("状态", justify="center")
    table.add_column("今日活跃(m)", justify="right", style="green")
    table.add_column("本周活跃(m)", justify="right", style="magenta")
    table.add_column("最后活动", justify="center")

    now = time.time()
    with stats_manager.lock:
        for topic in sorted(stats_manager.data.keys()):
            d = stats_manager.data[topic]
            is_active = (now - d["last_ts"]) < INACTIVE_THRESHOLD
            status = "[bold green]● 活跃[/]" if is_active else "[dim]○ 离线[/]"
            
            table.add_row(
                topic,
                status,
                f"{d['today_sec']/60:.1f}",
                f"{d['week_sec']/60:.1f}",
                datetime.datetime.fromtimestamp(d["last_ts"]).strftime('%H:%M:%S')
            )
    return table

# --- 主程序 ---
def main():
    client = mqtt_client.Client()
    
    if args.user and args.pw:
        client.username_pw_set(args.user, args.pw)
        
    client.on_connect = on_connect
    client.on_message = on_message

    try:
        # 增加超时处理，防止地址填错时程序卡死
        client.connect(args.broker, args.port, keepalive=60)
    except Exception as e:
        print(f"[ERROR] 无法连接到 Broker {args.broker}: {e}")
        return

    client.loop_start()

    # 定时清理逻辑
    def cleanup_loop():
        while True:
            time.sleep(CLEANUP_INTERVAL)
            stats_manager.cleanup()
    
    threading.Thread(target=cleanup_loop, daemon=True).start()

    console = Console()
    try:
        # 使用 Live 模式实时更新表格
        with Live(generate_table(), refresh_per_second=2, console=console) as live:
            while True:
                time.sleep(0.5)
                live.update(generate_table())
    except KeyboardInterrupt:
        client.loop_stop()
        print("\n已安全退出监控。")
        sys.exit(0)

if __name__ == "__main__":
    main()