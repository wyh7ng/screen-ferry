// 设置：外观、鼠标与剪贴板、通用，以及进入“关于”。按键映射、滚动方向等在“设备”里逐台设置。

import 'package:flutter/material.dart';

import '../l10n/l10n.dart';

import '../state/app_scope.dart';
import '../state/app_state.dart';
import '../state/transport_text.dart';
import '../theme/tokens.dart';
import '../widgets/controls.dart';
import '../widgets/dpi_field.dart';
import '../widgets/group.dart';
import '../widgets/page_scaffold.dart';
import '../widgets/pressable.dart';
import '../widgets/surface.dart';

class SettingsPage extends StatelessWidget {
  const SettingsPage({super.key});

  @override
  Widget build(BuildContext context) {
    final state = AppScope.of(context, aspects: const {AppAspect.settings, AppAspect.connection, AppAspect.local});
    final c = SfColors.of(context);
    final s = state.settings;
    final l = context.l10n;
    // 没连上后台时开关不可点（返回 null）。
    ValueChanged<bool>? set(SettingsPatch Function(bool) patch) =>
        state.online ? (v) => state.updateSettings(patch(v)) : null;
    return PageScaffold(
      title: l.navSettings,
      children: [
        SectionTitle(l.settingsThisComputer),
        SfGroup(
          children: [
            SfRow(
              icon: Icons.swap_horiz_rounded,
              title: l.settingsRole,
              subtitle: state.isServer ? l.settingsRoleServerHint : l.settingsRoleClientHint,
              trailing: SfSegmented<Role>(
                label: l.settingsRole,
                segments: [(Role.server, l.settingsRoleServer), (Role.client, l.settingsRoleClient)],
                value: state.role,
                onChanged: state.online ? state.setRole : null,
              ),
            ),
          ],
        ),
        SectionTitle(l.settingsAppearance),
        ListenableBuilder(
          listenable: state.appearance,
          builder: (context, _) => SfGroup(
            children: [
              SfRow(
                icon: Icons.contrast_rounded,
                title: l.settingsTheme,
                trailing: SfSegmented<ThemeMode>(
                  label: l.settingsTheme,
                  segments: [
                    (ThemeMode.system, l.settingsFollowSystem),
                    (ThemeMode.light, l.settingsLight),
                    (ThemeMode.dark, l.settingsDark),
                  ],
                  value: state.appearance.mode,
                  onChanged: state.online ? state.setTheme : null,
                ),
              ),
              SfRow(
                icon: Icons.translate_rounded,
                title: l.settingsLanguage,
                trailing: SfSegmented<UiLanguage>(
                  label: l.settingsLanguage,
                  // 语言名用它自己的文字写，换到看不懂的语言时也认得出来。
                  segments: [
                    (UiLanguage.system, l.settingsFollowSystem),
                    (UiLanguage.zh, '简体中文'),
                    (UiLanguage.en, 'English'),
                  ],
                  value: state.appearance.language,
                  onChanged: state.online ? state.setLanguage : null,
                ),
              ),
              SfRow(
                icon: Icons.palette_outlined,
                title: l.settingsAccent,
                subtitle: l.settingsAccentHint,
                trailing: Row(
                  mainAxisSize: MainAxisSize.min,
                  children: [
                    for (var i = 0; i < AccentChoice.all.length; i++) ...[
                      if (i > 0) const SizedBox(width: 10),
                      _Swatch(
                        choice: AccentChoice.all[i],
                        selected: state.appearance.accent == i,
                        onTap: state.online ? () => state.setAccent(i) : null,
                      ),
                    ],
                  ],
                ),
              ),
            ],
          ),
        ),
        SectionTitle(l.settingsMouseClipboard),
        SfGroup(
          children: [
            if (state.isServer) ...[
              SfRow(
                icon: Icons.mouse_outlined,
                title: l.settingsDpi,
                subtitle: l.settingsDpiHint,
                trailing: DpiField(
                  value: s.mouseDpi,
                  label: l.settingsDpi,
                  min: DaemonSettings.mouseDpiRange.min,
                  max: DaemonSettings.mouseDpiRange.max,
                  onChanged: state.online ? (v) => state.updateSettings(SettingsPatch(mouseDpi: v)) : null,
                ),
              ),
              SfRow(
                icon: Icons.devices_rounded,
                title: l.settingsPerDevice,
                subtitle: l.settingsPerDeviceHint,
                trailing: Icon(Icons.chevron_right_rounded, color: c.muted),
                onTap: () => state.go(SfPage.devices),
              ),
            ],
            SfRow(
              icon: Icons.content_paste_rounded,
              title: l.settingsClipboard,
              subtitle: l.settingsClipboardHint,
              trailing: SfSwitch(
                value: s.clipboard,
                label: l.settingsClipboard,
                onChanged: set((v) => SettingsPatch(clipboard: v)),
              ),
            ),
            SfRow(
              icon: Icons.storage_rounded,
              title: l.settingsClipLimit,
              subtitle: l.settingsClipLimitHint,
              trailing: _LimitSlider(
                value: s.clipboardLimitMb,
                onChanged: state.online ? (mb) => state.updateSettings(SettingsPatch(clipboardLimitMb: mb)) : null,
              ),
            ),
          ],
        ),
        SectionTitle(l.settingsResponsiveness),
        SfGroup(
          children: [
            SfRow(
              icon: Icons.alt_route_rounded,
              title: l.settingsInputTransport,
              subtitle: l.settingsInputTransportHint,
              trailing: _TransportMenu(
                value: s.inputTransport,
                enabled: state.online,
                quicAvailable: state.quicAvailable,
                onChanged: (value) => state.updateSettings(SettingsPatch(inputTransport: value)),
              ),
            ),
            Padding(
              padding: const EdgeInsets.fromLTRB(20, 0, 20, 14),
              child: Text(
                state.quicAvailable ? l.inputTransportExperimentHint : l.inputTransportUnavailable,
                style: TextStyle(fontSize: 13, color: c.muted, height: 1.5),
              ),
            ),
            SfRow(
              icon: Icons.speed_rounded,
              title: l.settingsFileRate,
              subtitle: l.settingsFileRateHint,
              trailing: DpiField(
                value: s.fileRateLimitMbps,
                min: 0,
                max: 1024,
                unit: 'MB/s',
                label: l.settingsFileRate,
                onChanged: state.online ? (v) => state.updateSettings(SettingsPatch(fileRateLimitMbps: v)) : null,
              ),
            ),
            if (state.isServer)
              SfRow(
                icon: Icons.mouse_outlined,
                title: l.settingsCoalesceMotion,
                subtitle: l.settingsCoalesceMotionHint,
                trailing: SfSwitch(
                  value: s.coalesceMotion,
                  label: l.settingsCoalesceMotion,
                  onChanged: set((v) => SettingsPatch(coalesceMotion: v)),
                ),
              ),
          ],
        ),
        SectionTitle(l.settingsGeneral),
        SfGroup(
          children: [
            SfRow(
              icon: Icons.power_settings_new_rounded,
              title: l.settingsAutostart,
              trailing: SfSwitch(
                value: s.autostart,
                label: l.settingsAutostart,
                onChanged: set((v) => SettingsPatch(autostart: v)),
              ),
            ),
            if (state.isServer)
              SfRow(
                icon: Icons.wifi_tethering_rounded,
                title: l.settingsAnnounce,
                subtitle: l.settingsAnnounceHint,
                trailing: SfSwitch(
                  value: s.announce,
                  label: l.settingsAnnounce,
                  onChanged: set((v) => SettingsPatch(announce: v)),
                ),
              ),
            SfRow(icon: Icons.folder_outlined, title: l.settingsDataDir, subtitle: state.dataDir, mono: true),
            SfRow(
              icon: Icons.info_outline_rounded,
              title: l.settingsAbout,
              subtitle: state.version.isEmpty ? null : l.settingsVersion(state.version),
              trailing: Icon(Icons.chevron_right_rounded, color: c.muted),
              onTap: () => state.go(SfPage.about),
            ),
          ],
        ),
      ],
    );
  }
}

/// 剪贴板单项上限的滑块：拖动时只改显示，松手才发给后台。
class _LimitSlider extends StatefulWidget {
  const _LimitSlider({required this.value, required this.onChanged});

  final int value;
  final ValueChanged<int>? onChanged;

  @override
  State<_LimitSlider> createState() => _LimitSliderState();
}

class _LimitSliderState extends State<_LimitSlider> {
  double? _dragging;

  @override
  Widget build(BuildContext context) {
    final c = SfColors.of(context);
    const range = DaemonSettings.clipboardLimitRange;
    final onChanged = widget.onChanged;
    final shown = (_dragging ?? widget.value.toDouble()).clamp(range.min.toDouble(), range.max.toDouble());
    return Row(
      mainAxisSize: MainAxisSize.min,
      children: [
        SizedBox(
          width: 180,
          child: Slider(
            value: shown,
            min: range.min.toDouble(),
            max: range.max.toDouble(),
            divisions: range.max - range.min,
            semanticFormatterCallback: (v) => '${v.round()} MB',
            onChanged: onChanged == null ? null : (v) => setState(() => _dragging = v),
            onChangeEnd: onChanged == null
                ? null
                : (v) {
                    setState(() => _dragging = null);
                    if (v.round() != widget.value) onChanged(v.round());
                  },
          ),
        ),
        SizedBox(
          width: 56,
          child: Text(
            '${shown.round()} MB',
            textAlign: TextAlign.end,
            style: TextStyle(fontFeatures: tabular, color: c.muted),
          ),
        ),
      ],
    );
  }
}

class _Swatch extends StatelessWidget {
  const _Swatch({required this.choice, required this.selected, required this.onTap});

  final AccentChoice choice;
  final bool selected;
  final VoidCallback? onTap;

  @override
  Widget build(BuildContext context) {
    final c = SfColors.of(context);
    return Pressable(
      onTap: onTap,
      radius: 999,
      pressScale: 0.9,
      selected: selected,
      semanticLabel: _accentName(context.l10n, choice),
      tooltip: _accentName(context.l10n, choice),
      child: AnimatedContainer(
        duration: Motion.fast,
        width: 40,
        height: 40,
        padding: const EdgeInsets.all(5),
        decoration: BoxDecoration(
          shape: BoxShape.circle,
          border: Border.all(color: c.fg.withValues(alpha: selected ? 1 : 0), width: 2.5),
        ),
        child: DecoratedBox(
          decoration: BoxDecoration(shape: BoxShape.circle, color: choice.light),
        ),
      ),
    );
  }
}

/// 强调色在当前语言里的名字。
String _accentName(AppLocalizations l, AccentChoice choice) => switch (choice.name) {
  'teal' => l.accentTeal,
  'purple' => l.accentPurple,
  'orange' => l.accentOrange,
  'pink' => l.accentPink,
  _ => l.accentBlue,
};

class _TransportMenu extends StatelessWidget {
  const _TransportMenu({
    required this.value,
    required this.enabled,
    required this.quicAvailable,
    required this.onChanged,
  });
  final InputTransport value;
  final bool enabled;
  final bool quicAvailable;
  final ValueChanged<InputTransport> onChanged;
  @override
  Widget build(BuildContext context) {
    final l = context.l10n;
    final c = SfColors.of(context);
    return PopupMenuButton<InputTransport>(
      key: const ValueKey('input-transport-menu'),
      tooltip: l.settingsInputTransport,
      enabled: enabled,
      initialValue: value,
      onSelected: onChanged,
      itemBuilder: (_) => [
        for (final transport in InputTransport.values)
          PopupMenuItem(
            value: transport,
            enabled: transport == InputTransport.tcp || quicAvailable,
            child: Text(transportChoice(l, transport)),
          ),
      ],
      child: Container(
        padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 9),
        decoration: ShapeDecoration(color: c.card2, shape: continuous(12)),
        child: Row(
          mainAxisSize: MainAxisSize.min,
          children: [
            ConstrainedBox(
              constraints: const BoxConstraints(maxWidth: 230),
              child: Text(
                transportChoice(l, value),
                maxLines: 2,
                overflow: TextOverflow.ellipsis,
                style: TextStyle(fontWeight: FontWeight.w600, color: enabled ? c.fg : c.muted),
              ),
            ),
            const SizedBox(width: 6),
            Icon(Icons.expand_more_rounded, size: 18, color: c.muted),
          ],
        ),
      ),
    );
  }
}
