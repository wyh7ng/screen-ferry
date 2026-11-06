import 'package:flutter/material.dart';

import '../ipc/messages.dart';
import '../l10n/l10n.dart';
import '../state/transport_text.dart';
import '../theme/tokens.dart';

/// 接收后台实际连接信息，不能从偏好设置推断。
class InputTransportStatus extends StatelessWidget {
  const InputTransportStatus({super.key, required this.transport, this.fallbackReason});
  final InputTransport? transport;
  final String? fallbackReason;

  @override
  Widget build(BuildContext context) {
    final l = context.l10n;
    final c = SfColors.of(context);
    final reason = transportFallback(l, fallbackReason);
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Text(
          '${l.inputTransportActual} · ${actualTransport(l, transport)}',
          style: TextStyle(fontSize: 13, fontWeight: FontWeight.w600, color: c.muted),
        ),
        if (reason != null)
          Padding(
            padding: const EdgeInsets.only(top: 4),
            child: Text(reason, style: TextStyle(fontSize: 12.5, color: c.muted, height: 1.5)),
          ),
      ],
    );
  }
}
