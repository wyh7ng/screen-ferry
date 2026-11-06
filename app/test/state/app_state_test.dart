import 'package:flutter_test/flutter_test.dart';
import 'package:screenferry_settings/state/app_state.dart';

/// 保留命令不执行，测试确认、失败和超时之间真实的界面状态。
class PendingBackend extends DemoBackend {
  PendingBackend() : super(animate: false);
  final commands = <ExecuteRequest>[];

  @override
  bool send(Request request) {
    if (request is ExecuteRequest) {
      commands.add(request);
      return true;
    }
    return super.send(request);
  }

  void result(ExecuteRequest command, {required bool ok}) =>
      emit(CommandResultNotice(session: command.session, id: command.id, ok: ok, message: ok ? '' : '保存失败'));
}

void main() {
  testWidgets('移除失败恢复设备，旧失败不覆盖后续设置', (tester) async {
    final backend = PendingBackend();
    final state = AppState(backend: backend)..start();
    addTearDown(state.dispose);
    await tester.pump();
    final peer = state.peers.first;
    state.forget(peer);
    expect(state.peerOf(peer.fingerprint), isNull);
    backend.result(backend.commands.last, ok: false);
    await tester.pump();
    expect(state.peerOf(peer.fingerprint), isNotNull);

    state.updateSettings(const SettingsPatch(accent: 1));
    final earlier = backend.commands.last;
    state.updateSettings(const SettingsPatch(accent: 2));
    backend.result(earlier, ok: false);
    await tester.pump();
    expect(state.appearance.accent, 2, reason: '第一条失败只撤销第一条');
    backend.goOffline();
    await tester.pump();
  });

  testWidgets('没有新快照也会按时撤销未确认的修改', (tester) async {
    final backend = PendingBackend();
    final state = AppState(backend: backend)..start();
    addTearDown(state.dispose);
    await tester.pump();
    final original = state.sharing;
    state.setSharing(!original);
    expect(state.sharing, !original);
    await tester.pump(const Duration(seconds: 4));
    expect(state.sharing, original);
    expect(state.toast.value?.text, isNotEmpty);
  });

  testWidgets('断线清除配对，重启复用编号不能确认旧会话', (tester) async {
    final backend = PendingBackend();
    final state = AppState(backend: backend)..start();
    addTearDown(state.dispose);
    await tester.pump();
    final id = backend.requestPairing('旧设备');
    await tester.pump();
    final old = state.prompts.single;
    backend.goOffline();
    await tester.pump();
    expect(state.prompts, isEmpty);
    backend.session = 'new-session';
    backend.goOnline();
    backend.emit(
      PairingNotice(
        PairingPrompt(
          session: backend.session,
          id: id,
          peerName: '新设备',
          fingerprint: 'new',
          short: 'new',
          code: '123 456',
        ),
      ),
    );
    await tester.pump();
    expect(state.prompts.single.peerName, '新设备');
    expect(state.replyPairing(id, accept: true, session: old.session), isFalse);
    expect(backend.commands, isEmpty);
    expect(state.replyPairing(id, accept: true, session: backend.session), isTrue);
    expect(backend.commands.single.session, 'new-session');
    backend.goOffline();
    await tester.pump();
  });

  testWidgets('旧邀请的迟到结果不完成新的邀请', (tester) async {
    final backend = PendingBackend();
    final state = AppState(backend: backend)..start();
    addTearDown(state.dispose);
    await tester.pump();
    state.dialAddress('192.168.1.2', invite: true);
    final first = state.dial!.requestId;
    state.clearDial();
    state.dialAddress('192.168.1.3', invite: true);
    final second = state.dial!.requestId;
    backend.emit(DialedNotice(address: '192.168.1.2', invite: true, requestId: first));
    await tester.pump();
    expect(state.dial!.done, isFalse);
    backend.emit(DialedNotice(address: '192.168.1.3', invite: true, requestId: second));
    await tester.pump();
    expect(state.dial!.succeeded, isTrue);
    backend.goOffline();
    await tester.pump();
  });

  test('强调色配置越界时回退到蓝色', () {
    final appearance = Appearance();
    addTearDown(appearance.dispose);
    for (final value in [-1, 5, 255]) {
      appearance.apply(accent: value);
      expect(appearance.accent, 0);
    }
    appearance.apply(accent: 4);
    expect(appearance.accent, 4);
  });
}
