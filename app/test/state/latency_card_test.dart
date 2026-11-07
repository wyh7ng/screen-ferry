import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:screenferry_settings/app.dart';
import 'package:screenferry_settings/state/app_state.dart';

import '../support.dart';

void main() {
  testWidgets('历史尖峰与最近窗口分开显示，采样时间独立更新且兼容旧后台', (tester) async {
    useLocale(tester);
    tester.view.physicalSize = const Size(375, 900);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.reset);
    final backend = DemoBackend(animate: false);
    final state = AppState(backend: backend)..start();
    addTearDown(state.dispose);
    await tester.pumpWidget(ScreenFerryApp(state: state));
    await tester.pump();

    final fixture = jsonDecode(File('test/fixtures/ipc_samples.json').readAsStringSync()) as Map;
    final json = (((fixture['notices'] as List).cast<Map>().firstWhere((n) => n['type'] == 'state'))['state'] as Map)
        .cast<String, Object?>();
    json['session'] = backend.session;
    (json['settings'] as Map)['language'] = 'zh';
    final link = json['link'] as Map;
    link['rtt_ms'] = [120.0, 3.0];
    link.remove('rtt_measured_at');
    backend.emit(StateNotice(DaemonState.fromJson(json)));
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 400));
    await tester.ensureVisible(find.text('应用往返'));
    await tester.pump();

    expect(find.text('历史峰值 120.00 ms'), findsOneWidget);
    expect(find.text('最近 2 个采样窗口均值 61.50 ms'), findsOneWidget);
    expect(find.text('最近窗口均值 · 采样时间未知'), findsOneWidget);
    expect(state.latestRtt, 3.0);
    expect(state.rttMeasuredAt, isNull);

    // 只有时间变化、RTT 数组不变时，卡片也应更新，不能沿用旧样本年龄。
    link['rtt_measured_at'] = DateTime.now().subtract(const Duration(seconds: 15)).millisecondsSinceEpoch;
    backend.emit(StateNotice(DaemonState.fromJson(json)));
    await tester.pump();
    expect(find.text('最近窗口均值 · 15 秒前'), findsOneWidget);
    expect(state.rttMeasuredAt, isNotNull);

    link['rtt_measured_at'] = DateTime.now().millisecondsSinceEpoch;
    backend.emit(StateNotice(DaemonState.fromJson(json)));
    await tester.pump();
    expect(find.text('最近窗口均值 · 刚刚'), findsOneWidget);
    expect(find.text('历史峰值 120.00 ms'), findsOneWidget);
    expect(tester.takeException(), isNull);
  });
}
