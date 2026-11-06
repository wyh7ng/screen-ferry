import 'package:flutter_test/flutter_test.dart';
import 'package:screenferry_settings/state/app_state.dart';

class LogBackend extends DemoBackend {
  LogBackend() : super(animate: false);
  final reads = <ReadRunLogRequest>[];
  @override
  bool send(Request request) {
    if (request is ReadRunLogRequest) {
      reads.add(request);
      return true;
    }
    return super.send(request);
  }

  void reply(
    ReadRunLogRequest request, {
    required String cursor,
    String text = 'line',
    int next = 1,
    bool fresh = false,
    bool more = false,
    String? error,
  }) => emit(
    RunLogNotice(
      requestId: request.requestId,
      cursor: cursor,
      next: next,
      fresh: fresh,
      more: more,
      error: error,
      bytes: next,
      linesTotal: next,
      lines: [RunLogLine(at: '12:00:00', level: RunLogLevel.info, text: text)],
    ),
  );
}

void main() {
  testWidgets('游标原样传回，重复与旧请求不重复追加或倒退', (tester) async {
    final backend = LogBackend();
    final state = AppState(backend: backend)..start();
    addTearDown(state.dispose);
    await tester.pump();
    state.watchRunLog(true);
    final first = backend.reads.single;
    backend.reply(first, cursor: 'opaque-a', fresh: true);
    await tester.pump();
    backend.reply(first, cursor: 'opaque-a', fresh: true);
    await tester.pump();
    expect(state.runLog, hasLength(1));
    await tester.pump(const Duration(seconds: 2));
    final second = backend.reads.last;
    expect(second.cursor, 'opaque-a');
    backend.reply(second, cursor: 'opaque-b', text: 'second', next: 2);
    await tester.pump();
    backend.reply(first, cursor: 'opaque-a', fresh: true);
    await tester.pump();
    expect(state.runLog.map((l) => l.text), ['line', 'second']);
    state.watchRunLog(false);
  });

  testWidgets('错误保留游标，more只在当前页面拉下一批且保持单请求', (tester) async {
    final backend = LogBackend();
    final state = AppState(backend: backend)..start();
    addTearDown(state.dispose);
    await tester.pump();
    state.watchRunLog(true);
    final first = backend.reads.single;
    await tester.pump(const Duration(seconds: 2));
    expect(backend.reads, hasLength(1), reason: '前一请求仍在等待');
    backend.reply(first, cursor: 'one', fresh: true, more: true);
    await tester.pump();
    await tester.pump(const Duration(milliseconds: 16));
    expect(backend.reads, hasLength(2));
    backend.reply(backend.reads.last, cursor: 'bad', error: 'busy', more: true);
    await tester.pump();
    expect(state.runLog, hasLength(1));
    expect(state.runLogError, 'busy');
    await tester.pump(const Duration(seconds: 2));
    expect(backend.reads.last.cursor, 'one');
    final pending = backend.reads.last;
    state.watchRunLog(false);
    backend.reply(pending, cursor: 'stale', fresh: true, text: 'must not appear');
    await tester.pump();
    expect(state.runLog.single.text, 'line');
  });
}
