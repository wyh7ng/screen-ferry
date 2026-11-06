import '../ipc/messages.dart';
import '../l10n/l10n.dart';

String transportChoice(AppLocalizations l, InputTransport transport) => switch (transport) {
  InputTransport.tcp => l.inputTransportTcp,
  InputTransport.auto => l.inputTransportAuto,
  InputTransport.quic => l.inputTransportQuic,
  InputTransport.quicReliable => l.inputTransportQuicReliable,
};

/// 实际协议只能由后台报告；配置中的auto或缺字段不能推断为TCP。
String actualTransport(AppLocalizations l, InputTransport? transport) => switch (transport) {
  InputTransport.tcp => 'TCP',
  InputTransport.quic => l.inputTransportQuicActual,
  InputTransport.quicReliable => l.inputTransportReliableActual,
  InputTransport.auto || null => l.inputTransportUnknown,
};

String? transportFallback(AppLocalizations l, String? reason) => switch (reason) {
  null || '' => null,
  'quic_unreachable' => l.inputTransportFallbackUnreachable,
  'quic_unavailable' => l.inputTransportFallbackUnavailable,
  _ => l.inputTransportFallbackUnknown,
};

InputTransport? reportedTransport(String value) => switch (value) {
  'tcp' => InputTransport.tcp,
  'quic' => InputTransport.quic,
  'quic_reliable' => InputTransport.quicReliable,
  _ => null,
};
