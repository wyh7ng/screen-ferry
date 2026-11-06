import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:screenferry_settings/app.dart';
import 'package:screenferry_settings/state/app_state.dart';

import '../support.dart';

Map<String, Object?> sample() {
  final data = jsonDecode(File('test/fixtures/ipc_samples.json').readAsStringSync()) as Map;
  final json = (((data['notices'] as List).cast<Map>().firstWhere((n) => n['type'] == 'state'))['state'] as Map)
      .cast<String, Object?>();
  (json['settings'] as Map)['language'] = 'zh';
  return json;
}

class TransportBackend extends DemoBackend {
  TransportBackend({required bool supported}) : super(animate: false, quicAvailable: supported);
  final pending = <ExecuteRequest>[];
  @override
  bool send(Request request) {
    if (request case ExecuteRequest(request: UpdateSettingsRequest(settings: final settings))
        when settings.inputTransport != null) {
      pending.add(request);
      return true;
    }
    return super.send(request);
  }
}

Future<AppState> launch(WidgetTester tester, DemoBackend backend, {double width = 1160}) async {
  useLocale(tester);
  tester.view.physicalSize = Size(width, 900);
  tester.view.devicePixelRatio = 1;
  addTearDown(tester.view.reset);
  final state = AppState(backend: backend)..start();
  addTearDown(state.dispose);
  await tester.pumpWidget(ScreenFerryApp(state: state));
  await tester.pump();
  await tester.pump(const Duration(seconds: 1));
  return state;
}

Future<void> settle(WidgetTester tester) async {
  for (var i = 0; i < 6; i++) {
    await tester.pump(const Duration(milliseconds: 100));
  }
}

Future<void> openMenu(WidgetTester tester) async {
  final menu = find.byKey(const ValueKey('input-transport-menu'));
  await tester.ensureVisible(menu);
  await tester.pump();
  await tester.tap(menu);
  await settle(tester);
}

void main() {
  testWidgets('没有QUIC能力时仅TCP可选择，已有非TCP配置也能改回', (tester) async {
    final backend = DemoBackend(animate: false, quicAvailable: false);
    final state = await launch(tester, backend);
    state.go(SfPage.settings);
    await settle(tester);
    await openMenu(tester);
    final items = tester.widgetList<PopupMenuItem<InputTransport>>(find.byType(PopupMenuItem<InputTransport>)).toList();
    expect(items.where((i) => i.enabled).map((i) => i.value), [InputTransport.tcp]);
    await tester.tap(find.text('TCP（默认）').last);
    await settle(tester);
    expect(find.textContaining('当前后台未包含 QUIC'), findsOneWidget);
    final json = sample();
    json['session'] = backend.session;
    (json['local'] as Map)['quic_available'] = false;
    (json['settings'] as Map)['input_transport'] = 'quic';
    backend.emit(StateNotice(DaemonState.fromJson(json)));
    await settle(tester);
    await openMenu(tester);
    expect(
      tester
          .widgetList<PopupMenuItem<InputTransport>>(find.byType(PopupMenuItem<InputTransport>))
          .singleWhere((i) => i.value == InputTransport.tcp)
          .enabled,
      isTrue,
    );
    await tester.tap(find.text('TCP（默认）').last);
    await settle(tester);
  });

  testWidgets('协议选择使用请求确认且失败回滚，实际状态不会跟着意图改变', (tester) async {
    final backend = TransportBackend(supported: true);
    final state = await launch(tester, backend, width: 375);
    state.go(SfPage.settings);
    await settle(tester);
    await openMenu(tester);
    await tester.tap(find.text('自动：优先 QUIC').last);
    await settle(tester);
    final request = backend.pending.single;
    expect((request.request as UpdateSettingsRequest).settings.inputTransport, InputTransport.auto);
    expect(state.settings.inputTransport, InputTransport.auto);
    expect(state.inputTransport, InputTransport.tcp);
    backend.emit(CommandResultNotice(session: request.session, id: request.id, ok: false, message: '不能建立QUIC连接'));
    await settle(tester);
    expect(state.settings.inputTransport, InputTransport.tcp);
    expect(find.textContaining('剪贴板和文件始终使用 TCP'), findsOneWidget);
    expect(tester.takeException(), isNull);
  });

  testWidgets('总览与设备页显示后台实际协议及回退原因，老后台显示未知', (tester) async {
    final backend = DemoBackend(animate: false);
    final state = await launch(tester, backend);
    final json = sample();
    (json['settings'] as Map)['input_transport'] = 'auto';
    (json['link'] as Map)['input_transport'] = 'tcp';
    (json['link'] as Map)['fallback_reason'] = 'quic_unreachable';
    final peer = (json['peers'] as List).first as Map;
    peer['input_transport'] = 'tcp';
    peer['fallback_reason'] = 'quic_unreachable';
    backend.emit(StateNotice(DaemonState.fromJson(json)));
    await settle(tester);
    expect(find.text('实际输入链路 · TCP'), findsOneWidget);
    expect(find.textContaining('当前已回退 TCP'), findsOneWidget);
    state.openDevice(peer['fingerprint'] as String);
    await settle(tester);
    expect(find.text('实际输入链路 · TCP'), findsOneWidget);
    expect(find.textContaining('当前已回退 TCP'), findsOneWidget);
    (json['link'] as Map).remove('input_transport');
    (json['link'] as Map).remove('fallback_reason');
    backend.emit(StateNotice(DaemonState.fromJson(json)));
    state.go(SfPage.overview);
    await settle(tester);
    expect(find.text('实际输入链路 · 未知'), findsOneWidget);
    expect(find.textContaining('当前已回退 TCP'), findsNothing);
  });
}
