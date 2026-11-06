import 'package:flutter_test/flutter_test.dart';
import 'package:screenferry_settings/ipc/messages.dart';

void main() {
  test('性能计数、微秒与未知队列年龄按协议解析', () {
    final info = PerformanceInfo.fromJson({
      'peers': [
        {
          'fingerprint': null,
          'measured_at': 1000,
          'metrics': {
            'window_ms': 5000,
            'rtt': {'count': 100, 'p50_us': 800, 'p95_us': 1600, 'p99_us': 3200, 'max_us': 2777, 'over_100ms': 0},
            'capture': {
              'depth': 2,
              'high_water': 9,
              'oldest_us': null,
              'max_wait_us': 345,
              'coalesced': 50,
              'overflows': 1,
            },
          },
        },
      ],
    });
    final peer = info.peers.single;
    expect(peer.fingerprint, isNull);
    expect(peer.measuredAt!.millisecondsSinceEpoch, 1000);
    expect(peer.metrics.windowMs, 5000);
    expect(peer.metrics.latencies['rtt']!.p95Us, 1600);
    expect(peer.metrics.latencies['rtt']!.maxUs, 2777);
    expect(peer.metrics.capture.oldestUs, isNull);
    expect(peer.metrics.capture.maxWaitUs, 345);
    expect(peer.metrics.capture.coalesced, 50);
    expect(peer.metrics.latencies['inject']!.count, 0);
    expect(() => peer.metrics.latencies.clear(), throwsUnsupportedError);
    expect(() => info.peers.clear(), throwsUnsupportedError);
  });

  test('新增设置缺省安全且补丁保留零值和关闭状态', () {
    final settings = DaemonSettings.fromJson({});
    expect(settings.fileRateLimitMbps, 0);
    expect(settings.coalesceMotion, isFalse);
    const patch = SettingsPatch(fileRateLimitMbps: 0, coalesceMotion: false);
    expect(patch.toJson(), {'file_rate_limit_mbps': 0, 'coalesce_motion': false});
    expect(settings.apply(const SettingsPatch(fileRateLimitMbps: 12, coalesceMotion: true)).fileRateLimitMbps, 12);
    expect(settings.apply(const SettingsPatch(fileRateLimitMbps: 12, coalesceMotion: true)).coalesceMotion, isTrue);
  });
}
