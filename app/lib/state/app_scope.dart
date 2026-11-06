import 'dart:async';

import 'package:flutter/widgets.dart';

import 'app_state.dart';
import 'aspects.dart';
export 'aspects.dart' show AppAspect;

/// 保持子树身份稳定，仅让语义发生变化的状态域通知订阅者。
class AppScope extends StatefulWidget {
  const AppScope({super.key, required this.state, required this.child});
  final AppState state;
  final Widget child;

  @override
  State<AppScope> createState() => _AppScopeState();

  static AppState of(BuildContext context, {required Set<AppAspect> aspects}) {
    assert(aspects.isNotEmpty);
    _Domains? scope;
    for (final aspect in aspects) {
      scope = InheritedModel.inheritFrom<_Domains>(context, aspect: aspect);
    }
    return scope!.state;
  }

  /// 只取状态、不订阅变化，用在事件回调里。
  static AppState read(BuildContext context) => context.getInheritedWidgetOfExactType<_Domains>()!.state;
}

class _AppScopeState extends State<AppScope> {
  int _clock = 0;
  late final Timer _timer;
  @override
  void initState() {
    super.initState();
    // 相对时间独立于网络刷新，避免连接时长在静止页面停住。
    _timer = Timer.periodic(const Duration(seconds: 30), (_) => setState(() => _clock++));
  }

  @override
  void dispose() {
    _timer.cancel();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) => ListenableBuilder(
    listenable: widget.state,
    child: widget.child,
    builder: (_, child) =>
        _Domains(state: widget.state, revisions: {...widget.state.revisions, AppAspect.clock: _clock}, child: child!),
  );
}

class _Domains extends InheritedModel<AppAspect> {
  const _Domains({required this.state, required this.revisions, required super.child});
  final AppState state;
  final Map<AppAspect, int> revisions;

  @override
  bool updateShouldNotify(_Domains oldWidget) => !identical(state, oldWidget.state) || revisions != oldWidget.revisions;

  @override
  bool updateShouldNotifyDependent(_Domains oldWidget, Set<AppAspect> dependencies) =>
      !identical(state, oldWidget.state) ||
      dependencies.any((aspect) => revisions[aspect] != oldWidget.revisions[aspect]);
}
