import 'package:flutter_test/flutter_test.dart';
import 'package:screenferry_settings/ipc/messages.dart';

void main() {
  test('旧后台缺能力与实际协议时不猜测TCP', () {
    expect(Local.fromJson({}).quicAvailable, isFalse);
    expect(DaemonSettings.fromJson({}).inputTransport, InputTransport.tcp);
    expect(Link.fromJson({}).inputTransport, isNull);
    expect(PeerInfo.fromJson({}).inputTransport, isNull);
    expect(Link.fromJson({'input_transport': 'future_protocol'}).inputTransport, isNull);
  });

  test('实际协议及稳定回退码独立于配置', () {
    final state = DaemonState.fromJson({
      'local': {'quic_available': true},
      'settings': {'input_transport': 'auto'},
      'link': {'input_transport': 'tcp', 'fallback_reason': 'quic_unreachable'},
      'peers': [
        {'input_transport': 'quic_reliable', 'fallback_reason': null},
      ],
    });
    expect(state.local.quicAvailable, isTrue);
    expect(state.settings.inputTransport, InputTransport.auto);
    expect(state.link.inputTransport, InputTransport.tcp);
    expect(state.link.fallbackReason, 'quic_unreachable');
    expect(state.peers.single.inputTransport, InputTransport.quicReliable);
  });

  test('传输设置补丁使用snake_case并保留默认TCP', () {
    for (final (value, wire) in [
      (InputTransport.tcp, 'tcp'),
      (InputTransport.auto, 'auto'),
      (InputTransport.quic, 'quic'),
      (InputTransport.quicReliable, 'quic_reliable'),
    ]) {
      final patch = SettingsPatch(inputTransport: value);
      expect(patch.toJson(), {'input_transport': wire});
      final settings = DaemonSettings.fromJson({}).apply(patch);
      expect(settings.inputTransport, value);
      expect(patch.without(settings).isEmpty, isTrue);
      expect(settings.withDevice(const DeviceSettings(fingerprint: 'p')).inputTransport, value);
    }
  });
}
