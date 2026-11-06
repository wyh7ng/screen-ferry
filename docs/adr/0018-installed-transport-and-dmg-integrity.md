# ADR 0018：安装版输入传输选择与 DMG 完整性

状态：实施完成，本地整合通过，等待CI产物。日期：2026-11-06。

## 原因

提交5c4ccf6安装包仍使用TCP，符合原ADR0017的CLI实验限制：普通bundle未编译quic-experiment，resident固定调用TCP入口。这不是尝试QUIC后回退，而是安装包没有启用路径。用户要求排查并修正可用性。

同时，DMG制作hide_extensions配置触发SetFile -a E，给已经签名的外层.app添加com.apple.FinderInfo，导致安装后codesign --strict失败；ZIP在制盘前生成，签名正常。

## 决策

- schema7→8增加input_transport:tcp/auto/quic/quic_reliable，默认tcp保留现有连接行为。配置改动后重启引擎，保存失败按原请求结果回滚。
- 编译了quic-experiment的常驻后台支持同样的QUIC入口，CI两平台开发包使用bundle --quic-experiment。普通源码构建可继续不带feature；local.quic_available明确告知能力，非TCP设置在无feature构建中返回错误。
- 设置界面选择意图与当前连接事实分开。link/peer input_transport只有实际建立并协商后填值，离线为null；auto回退代码quic_unreachable只在实际回退端报告。QUIC表示允许位置数据报，纯可靠模式明确区分；数据和文件始终TCP。
- 主动连接由被控端发起，显式QUIC不回退适用于该主动端；控制端启用QUIC仍接受TCP客户端以兼容旧设备。两端偏好不一致时以实际状态为准，不用设置选项冒充连接协议。
- 默认不自动修改正在运行的用户设置。两端安装支持版本后，可在设置选择auto（优先QUIC、失败TCP）或严格QUIC；控制端也要启用QUIC监听。只有输入通道使用QUIC，配对与授权不变，禁止0-RTT。
- Windows现有按程序private/domain入站规则已覆盖TCP/UDP，保留规则，不额外扩展public或受限出站权限。
- DMG禁止签名后添加hide_extensions元数据。制盘结束必须只读挂载并严格验证外层app和helper，再卸载。校验或挂载失败删除无效产物，卸载只针对该次自建挂载目录，不递归删除挂载内容。
- 为无需完整Xcode复现制盘问题，支持受限 --dmg-from APP --dmg-output NEW.dmg；源先验签，不重编译、不重签、不修改源、不覆盖已有输出，不与编译/公证选项混用。

## 证据与边界

真实本机DMG回归已验证：从5c4ccf6 ZIP解出的干净副本重新制盘严格验签通过，旧坏DMG注入及无效映像均拒绝并清理；原安装app和用户挂载盘未改。

安装连接的被动/ICMP对照只能排查相关性，不能把QUIC切换当作Wi-Fi尖峰已经消除。当前没有Windows端同窗调度数据、TCP重传证据或有线Mac对照，不能确定183ms峰值的单一根因。
