import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:screenferry_settings/app.dart';
import 'package:screenferry_settings/state/app_state.dart';

import '../support.dart';

class MetricsBackend extends DemoBackend {
  MetricsBackend() : super(animate: false);
  @override
  void emit(Notice notice) {
    if (notice case StateNotice(:final state)) {
      super.emit(
        StateNotice(
          DaemonState(
            session: state.session,
            local: state.local,
            sharing: state.sharing,
            problem: state.problem,
            link: state.link,
            clipboard: state.clipboard,
            peers: state.peers,
            found: state.found,
            settings: state.settings,
            activity: state.activity,
            update: state.update,
            performance: PerformanceInfo.fromJson({
              'peers': [
                {
                  'fingerprint': null,
                  'measured_at': 1000,
                  'metrics': {
                    'rtt': {
                      'count': 5,
                      'p50_us': 1500,
                      'p95_us': 3000,
                      'p99_us': 6000,
                      'max_us': 5555,
                      'over_100ms': 0,
                    },
                    'display_query': {'count': 5, 'max_us': 120123, 'over_100ms': 1},
                    'cursor_switch': {'count': 2, 'max_us': 864},
                    'heartbeat_send': {'count': 8, 'max_us': 287},
                    'capture': {'depth': 0, 'high_water': 12, 'oldest_us': null, 'coalesced': 50},
                  },
                },
              ],
            }),
          ),
        ),
      );
    } else {
      super.emit(notice);
    }
  }
}

void main() {
  for (final width in [375.0, 1160.0]) {
    testWidgets('性能统计在${width.toInt()}宽度无溢出并区分无样本', (tester) async {
      useLocale(tester);
      tester.view.physicalSize = Size(width, 900);
      tester.view.devicePixelRatio = 1;
      addTearDown(tester.view.reset);
      final state = AppState(backend: MetricsBackend())..start();
      addTearDown(state.dispose);
      await tester.pumpWidget(ScreenFerryApp(state: state));
      await tester.pump();
      await tester.pump(const Duration(seconds: 1));
      await tester.ensureVisible(find.text('性能统计'));
      await tester.pump();
      expect(find.text('1.50'), findsOneWidget);
      expect(find.text('5.55'), findsOneWidget);
      expect(find.text('—'), findsWidgets, reason: '无注入等样本不显示0ms');
      expect(find.text('最老等待 —'), findsNWidgets(2));
      expect(find.textContaining('不是端到端输入延迟'), findsOneWidget);
      expect(find.text('显示器查询'), findsOneWidget);
      expect(find.text('光标切换'), findsOneWidget);
      expect(find.text('心跳排队与写入'), findsOneWidget);
      expect(find.text('120.12'), findsOneWidget);
      expect(find.text('0.86'), findsOneWidget);
      expect(find.text('0.29'), findsOneWidget);
      expect(find.textContaining('不代表输入线程被阻塞'), findsOneWidget);
      expect(tester.takeException(), isNull);
    });
  }
}
