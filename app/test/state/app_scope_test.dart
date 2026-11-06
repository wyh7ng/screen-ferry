import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:screenferry_settings/app.dart';
import 'package:screenferry_settings/state/app_state.dart';

import '../support.dart';

Map<String, Object?> snapshot() {
  final fixture = jsonDecode(File('test/fixtures/ipc_samples.json').readAsStringSync()) as Map;
  final notice = (fixture['notices'] as List).cast<Map>().firstWhere((n) => n['type'] == 'state');
  return (notice['state'] as Map).cast<String, Object?>();
}

Future<(AppState, DemoBackend)> mount(WidgetTester tester) async {
  useLocale(tester);
  tester.view.physicalSize = const Size(1160, 780);
  tester.view.devicePixelRatio = 1;
  addTearDown(tester.view.reset);
  final backend = DemoBackend(animate: false);
  final state = AppState(backend: backend)..start();
  addTearDown(state.dispose);
  await tester.pumpWidget(ScreenFerryApp(state: state));
  backend.emit(StateNotice(DaemonState.fromJson(snapshot())));
  await tester.pump();
  await tester.pump(const Duration(seconds: 2));
  return (state, backend);
}

void main() {
  testWidgets('20次RTT和下载进度快照不重建设置、侧栏或页面骨架', (tester) async {
    final (state, backend) = await mount(tester);
    state.go(SfPage.settings);
    await tester.pump();
    await tester.pump(const Duration(seconds: 1));
    final builds = <String, int>{};
    debugOnRebuildDirtyWidget = (element, _) {
      final type = element.widget.runtimeType.toString();
      builds[type] = (builds[type] ?? 0) + 1;
    };
    addTearDown(() => debugOnRebuildDirtyWidget = null);
    for (var i = 0; i < 20; i++) {
      final json = snapshot();
      (json['link'] as Map)['rtt_ms'] = [1.0 + i];
      (json['update'] as Map)['done'] = i * 100;
      backend.emit(StateNotice(DaemonState.fromJson(json)));
      await tester.pump(const Duration(milliseconds: 250));
      await tester.pump();
    }
    for (final name in ['SettingsPage', 'Sidebar', 'HomeShell', 'PageScaffold']) {
      expect(builds[name] ?? 0, 0, reason: '$name不订阅RTT/下载进度');
    }
    final changed = snapshot();
    (changed['settings'] as Map)['clipboard'] = !state.settings.clipboard;
    backend.emit(StateNotice(DaemonState.fromJson(changed)));
    await tester.pump();
    await tester.pump();
    expect(builds['SettingsPage'], 1, reason: '真实设置变化仍然更新');
  });

  testWidgets('RTT只重建延迟卡片，等值的新对象不重建设备和剪贴板', (tester) async {
    final (_, backend) = await mount(tester);
    final builds = <String, int>{};
    debugOnRebuildDirtyWidget = (element, _) {
      final type = element.widget.runtimeType.toString();
      builds[type] = (builds[type] ?? 0) + 1;
    };
    addTearDown(() => debugOnRebuildDirtyWidget = null);
    for (var i = 0; i < 12; i++) {
      final json = snapshot();
      (json['link'] as Map)['rtt_ms'] = [20.0 + i];
      backend.emit(StateNotice(DaemonState.fromJson(json)));
      await tester.pump(const Duration(milliseconds: 250));
      await tester.pump();
    }
    expect(builds['_LatencyCard'], 12);
    for (final name in ['_HeroCard', '_DevicesCard', '_ClipboardCard', '_TodayCard', 'PerformanceCard', 'HomeShell']) {
      expect(builds[name] ?? 0, 0, reason: '相同内容的独立JSON对象不会改变$name');
    }
    final json = snapshot();
    (json['peers'] as List).first['alias'] = '新别名';
    backend.emit(StateNotice(DaemonState.fromJson(json)));
    await tester.pump();
    await tester.pump();
    expect(builds['_DevicesCard'], 1, reason: '设备深层字段变化必须刷新');
  });
}
