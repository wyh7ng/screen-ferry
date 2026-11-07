import 'package:flutter/material.dart';

import '../l10n/l10n.dart';
import '../state/app_scope.dart';
import '../state/app_state.dart';
import '../theme/tokens.dart';
import '../widgets/surface.dart';

/// 与普通连接状态分开订阅，后台测量窗口更新只重建这张卡片。
class PerformanceCard extends StatelessWidget {
  const PerformanceCard({super.key});

  @override
  Widget build(BuildContext context) {
    final state = AppScope.of(
      context,
      aspects: const {AppAspect.performance, AppAspect.devices, AppAspect.local, AppAspect.clock},
    );
    final l = context.l10n;
    final c = SfColors.of(context);
    final peers = state.performance.peers;
    return SfCard(
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.stretch,
        children: [
          CardHeader(icon: Icons.query_stats_rounded, title: l.performanceTitle),
          const SizedBox(height: 10),
          Text(l.performanceHint, style: TextStyle(color: c.muted, height: 1.5)),
          const SizedBox(height: 6),
          Text(l.performanceBackgroundHint, style: TextStyle(color: c.muted, height: 1.5)),
          if (peers.isEmpty)
            Padding(
              padding: const EdgeInsets.only(top: 14),
              child: Text(l.performanceEmpty, style: TextStyle(color: c.muted)),
            ),
          for (final peer in peers) ...[
            const SizedBox(height: 18),
            Text(
              peer.fingerprint == null ? l.performanceLocal : state.displayNameOf(peer.fingerprint!, l.peerFallback),
              style: const TextStyle(fontWeight: FontWeight.w700),
            ),
            const SizedBox(height: 4),
            Text(
              l.performanceWindowAge(
                (peer.metrics.windowMs / 1000).toStringAsFixed(1),
                peer.measuredAt == null ? l.performanceTimeUnknown : timeAgo(l, peer.measuredAt!),
              ),
              style: TextStyle(fontSize: 12, color: c.muted),
            ),
            const SizedBox(height: 10),
            SingleChildScrollView(
              scrollDirection: Axis.horizontal,
              child: SizedBox(width: 720, child: _LatencyTable(metrics: peer.metrics)),
            ),
            const SizedBox(height: 8),
            Text(l.performanceUnits, style: TextStyle(fontSize: 12, color: c.muted)),
            const SizedBox(height: 12),
            _QueueRow(title: l.performanceCapture, queue: peer.metrics.capture),
            const SizedBox(height: 8),
            _QueueRow(title: l.performanceOutbound, queue: peer.metrics.outbound),
          ],
        ],
      ),
    );
  }
}

class _LatencyTable extends StatelessWidget {
  const _LatencyTable({required this.metrics});
  final PerformanceMetrics metrics;
  @override
  Widget build(BuildContext context) {
    final l = context.l10n;
    final stages = [
      l.performanceCaptureQueue,
      l.performanceSendQueue,
      l.performanceSend,
      l.performanceInject,
      l.performanceRtt,
      l.performanceScheduler,
      l.performanceDisplayQuery,
      l.performanceCursorSwitch,
      l.performanceHeartbeatSend,
    ];
    Widget cell(String text, {bool title = false}) => Padding(
      padding: const EdgeInsets.symmetric(vertical: 7, horizontal: 6),
      child: Text(
        text,
        style: TextStyle(fontSize: 12.5, fontWeight: title ? FontWeight.w700 : FontWeight.w400, fontFeatures: tabular),
      ),
    );
    return Table(
      columnWidths: const {0: FixedColumnWidth(175)},
      children: [
        TableRow(
          children: [
            for (final s in [l.performanceStage, l.performanceCount, 'P50', 'P95', 'P99', l.performanceMax, '>100 ms'])
              cell(s, title: true),
          ],
        ),
        for (var i = 0; i < PerformanceMetrics.stages.length; i++)
          _row(stages[i], metrics.latencies[PerformanceMetrics.stages[i]] ?? const LatencySummary(), cell),
      ],
    );
  }

  TableRow _row(String title, LatencySummary latency, Widget Function(String) cell) {
    String ms(int us) => latency.count == 0 ? '—' : (us / 1000).toStringAsFixed(2);
    return TableRow(
      children: [
        cell(title),
        cell('${latency.count}'),
        for (final us in [latency.p50Us, latency.p95Us, latency.p99Us, latency.maxUs]) cell(ms(us)),
        cell('${latency.over100ms}'),
      ],
    );
  }
}

class _QueueRow extends StatelessWidget {
  const _QueueRow({required this.title, required this.queue});
  final String title;
  final QueueSummary queue;
  @override
  Widget build(BuildContext context) {
    final l = context.l10n;
    final c = SfColors.of(context);
    final values = [
      (l.performanceDepth, '${queue.depth}'),
      (l.performanceHighWater, '${queue.highWater}'),
      (l.performanceOldest, queue.oldestUs == null ? '—' : '${(queue.oldestUs! / 1000).toStringAsFixed(2)} ms'),
      (l.performanceMaxWait, '${(queue.maxWaitUs / 1000).toStringAsFixed(2)} ms'),
      (l.performanceCoalesced, '${queue.coalesced}'),
      (l.performanceOverflows, '${queue.overflows}'),
    ];
    return DecoratedBox(
      decoration: ShapeDecoration(color: c.card2, shape: continuous(14)),
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Wrap(
          spacing: 16,
          runSpacing: 8,
          children: [
            Text(title, style: const TextStyle(fontWeight: FontWeight.w600, fontSize: 12.5)),
            for (final (label, value) in values)
              Text(
                '$label $value',
                style: TextStyle(fontSize: 12, color: c.muted, fontFeatures: tabular),
              ),
          ],
        ),
      ),
    );
  }
}
