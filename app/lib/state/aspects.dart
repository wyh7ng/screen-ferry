/// 组件只订阅需要的状态域；高频 RTT、传输进度与日志不触发无关页面重建。
enum AppAspect {
  navigation,
  clock,
  local,
  connection,
  devices,
  settings,
  rtt,
  clipboard,
  activity,
  update,
  updateReminder,
  pairing,
  dial,
  log,
  runLog,
  diagnostics,
  performance,
}

/// 值快照只含不可变标量及新建的列表/映射，避免新解码对象的身份变化触发重建。
bool sameValue(Object? a, Object? b) {
  if (identical(a, b)) return true;
  if (a is List && b is List) {
    if (a.length != b.length) return false;
    for (var i = 0; i < a.length; i++) {
      if (!sameValue(a[i], b[i])) return false;
    }
    return true;
  }
  if (a is Map && b is Map) {
    if (a.length != b.length) return false;
    return a.keys.every((key) => b.containsKey(key) && sameValue(a[key], b[key]));
  }
  return a == b;
}
