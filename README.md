# mkra — SHARP 4S-C00AS1 BS2K / BS4K 統合版

**[otti83さんの mkra](https://github.com/otti83/mkra_4S-C00AS1) を基にした、Karin-Laboratoryの実験的なForkです。** 本家のMITライセンス・著作権表示・コミット履歴を保持しています。

## 1. プロジェクト概要

SHARP 4S-C00AS1の放送受信機能を利用し、通常BS（ISDB-S）のMPEG-TSとBS4K（ISDB-S3）のMMTから変換したMPEG-TSを、同じMirakurun互換HTTP APIから配信します。Node.jsやUbuntu chrootを受信機に入れる必要はなく、ARMv7向けの静的実行ファイル1本で動作します。

開発・実機検証は2026年10月15〜16日。これはメーカー公式の機能ではありません。BS2K切替後のHDMI映像復帰など未解決の問題があり、安定版ではありません。

## 2. オリジナル作者への謝辞

本家作者 **otti83さん** の、4S-C00AS1の解析、ViXSドライバ制御、BS4KのMMT/ES取り出しとREMUX、HTTP API、ビルド・配備ツールが本プロジェクトの土台です。

- 本家: https://github.com/otti83/mkra_4S-C00AS1
- [本家READMEの保存版](docs/UPSTREAM-README.ja.md)（接続例のアドレスのみ匿名化）

このForkで加えたのは次節の通常BS経路・ライブ復号・統合運用などです。本家のBS4K実装を今回新規に開発したものとは扱いません。

## 3. 本家から追加した機能

- SC1501Aの通常BS受信、物理TSI0→論理TSI1の入力割当、DEMUX・TSリング・host readerによる188バイトTS取得。
- 稼働中の内蔵CASとの直列化された通信、ECM更新追跡、CPUによるライブMULTI2復号。カード固有情報・実際の鍵は同梱せず、ログにも出力しません。
- BS2K/BS4K共通のMirakurun互換HTTP API。同一TSの接続を共有し、異なるTSへの競合要求は拒否します。
- 通常BS6局の設定、EIT番組表プロキシとEPGStation連携、RAMへの定期再投入ツール。
- 2K→4K切替時に、古いMPTファイルではなく純正ADSBの取得済みサービス状態と新しい映像・音声リングを確認する修正。

## 4. 対応機種・必要な環境

- SHARP **4S-C00AS1**、BSアンテナ、正常に受信・視聴できる内蔵CAS環境。
- 本体が電源ON（緑LED）で、既にroot telnet等の管理手段が使えること。本Forkはtelnet有効化やフラッシュ変更を自動実行しません。
- 純正Linux・tuner/card/abs/adsb等のサービスを稼働させたまま使用します。
- 固定ABI・純正実行ファイルのハッシュ照合を使います。異なるファームウェアやMASPRO/I-O DATA等の類似機種では未検証です。
- ビルド用Linux/macOSとRust。EPG連携には別のLinuxサーバ、Node.js（18以上）、Python 3、EPGStation 2.10.0相当が必要です。

## 5. BS4K / BS2Kの対応状況

| 方式 | 状態 | ネットワークID |
| --- | --- | --- |
| BS4K | 本家のMMT→REMUXを維持。2KからNHK BSP4K・BS日テレ4K・BS-TBS4Kへの配信・録画切替を実機確認 | 11 |
| 通常BS2K | NHK BS、BS日テレ、BS朝日、BS-TBS、BSテレ東、BSフジの主サービスを受信・映像音声デコード確認 | 4 |
| CS / その他サービス | 未検証。受信保証なし | — |

サービスIDが同じでも、API上は `networkId * 100000 + serviceId` で区別します。例: BS朝日2Kは`400151`、BS朝日4Kは`1100151`。チャンネル種別の表示をBSへ変換するEPG互換プロキシでも、このIDは統合しません。

## 6. インストール・起動

ソースだけでビルドでき、純正バイナリ・ダンプ・放送TSは不要です。

```sh
git clone https://github.com/Karin-Laboratory/mkra_4S-C00AS1.git
cd mkra_4S-C00AS1
git remote add upstream https://github.com/otti83/mkra_4S-C00AS1.git
rustup target add armv7-unknown-linux-musleabihf
./tools/build_unified.sh
# 出力: target/armv7-unknown-linux-musleabihf/release/mkra
```

[Releases](https://github.com/Karin-Laboratory/mkra_4S-C00AS1/releases)またはActionsの`mkra-armv7`からも取得できます。SHA-256を照合してください。バイナリを既存の管理手段で本体の`/tmp/mt/`へ転送し、実行権限を付けます。既存mkraとの二重起動を避け、録画中に入れ替えないでください。

本体側の最小起動例:

```sh
/tmp/mt/mkra serve --remux4k --tune --bs2k --channel-type BS4K --port 40772
```

6局すべてを使う場合は`config/bs-additional-main.tsv`も`/tmp/mt/`へ転送します（BS朝日は内蔵設定）。

```sh
/tmp/mt/mkra serve --remux4k --tune --bs2k --channel-type BS4K --port 40772   --isdbs-channels /tmp/mt/bs-additional-main.tsv   --epg-tap --epg-refresh 5 --epg-dir /tmp/adsb
```

管理PC側の確認例（`198.51.100.10`は文書用アドレス。自分の受信機へ置換）:

```sh
curl http://198.51.100.10:40772/api/services
curl http://198.51.100.10:40772/api/services/400151/stream -o test.ts
# curlを終了すると受信が解放されます。録画中は別TSを要求しないでください。
```

配置先はRAMなので電源断で消えます。[RAM自動再投入](docs/MKRA-AUTO-RESTORE.ja.md)と[HDD保存](docs/HDD-INSTALL-STUDY.ja.md)を参照してください。

## 7. EPGStation / TVTestへの接続

Mirakurun互換サーバはTCP **40772**。TVTestでは、Mirakurun対応BonDriver等の接続先を受信機に設定し、BS/BS4Kのチャンネルを読み込みます。画像で確認された他者の成功例と、本Forkの実測結果は区別してください。本ForkのTVTest環境全般の互換性は未検証です。

EPGStationの新規導入は[完全な導入手順](docs/EPGSTATION-INSTALL.ja.md)を参照してください。[EPGツール](tools/epgstation/README.ja.md)により、別サーバ上のEIT収集・互換プロキシ（標準40773）を介します。EPGStationの`mirakurunPath`は通常`http://127.0.0.1:40773/`です。プロキシだけで既存環境の全設定を置き換えられるわけではありません。番組ID録画の終了処理と、必要に応じた2.10.0用パッチも確認してください。

## 8. 実機検証結果

- 通常BS6局でライブ受信と映像・音声の実デコード。
- BS2Kで120秒の連続ライブ復号、同一TSへの複数接続共有。
- BS2K→BS4KのHTTP配信切替。
- EPGStationの番組表から予約し、録画ファイルをブラウザで再生。
- 2K→BS日テレ4K、および2K→BS-TBS4Kの間隔0秒の時刻指定予約を自動終了まで確認。生成4Kファイルの映像・音声10秒デコードにも成功。
- 統合版のRAM自動再投入・重複防止を確認。

これらは開発時の実測です。今回の公開作業では稼働中の受信機・EPGStationを変更せず、ソースのテストとクロスビルドを行います。CI成功を実機での再検証成功とは扱いません。

詳細: [試行錯誤の全体記録](docs/TRIAL-RETROSPECTIVE-20261015-16.ja.md)、[2K→4K録画失敗の修正](docs/EPGSTATION-OTHER-4K-TRANSITION-20261016.ja.md)。

## 9. 現在の制限と未解決の問題

- **物理チューナー1基**。BS2KとBS4Kの別TS同時受信はできません。EPG巡回も録画と競合します。
- 通常BS切替後の**HDMI映像復帰は未解決**。HTTPの4K配信復帰とは別問題です。純正視聴を使う運用では復旧操作が必要になる場合があります。
- BS日テレの一部PID（実測0x248）には暗号化が残ります。主映像音声の成功を全PIDの完全復号とは扱いません。
- CS・一部サービス・他ファームウェア・他機種は未検証です。
- 長時間の安定運用は未確認。120秒の試験で長時間録画を保証しません。
- コンセントOFF/ONからの自動復旧について、通し試験は未完了です。
- EPGStationの番組延長・時刻変更追従は継続課題。単一チューナーの連続予約では`allowEndLack`等により先行番組末尾が約15秒短くなる場合があります。
- 同一TSの配信は多重化全体を共有し、サービスごとの完全なPID分離を保証しません。
- 認証やTLSのない管理APIです。信頼できるネットワーク内で利用してください。

## 10. ライセンス・免責事項

[MIT LICENSE](LICENSE)は本家の表示を保持しています。MULTI2参照実装由来の注意事項とApache-2.0ライセンスは[third-party NOTICE](tools/third-party/libarib25-NOTICE.md)を参照してください。

無保証です。メーカーの保証対象外となる操作や、視聴・録画に影響する可能性があります。自己所有の機器・正当な視聴権限の範囲で利用し、適用される法令・契約を確認してください。純正ファームウェア、鍵、カード情報、放送録画は配布しません。

### 技術資料

- [通常BS受信経路](docs/BS2K-RECEPTION.ja.md)
- [CAS調査・ライブ復号](docs/BS2K-CAS-INVESTIGATION.ja.md)
- [EPGStation連携と制限](docs/EPGSTATION-BS2K.ja.md)
- [自動再投入](docs/MKRA-AUTO-RESTORE.ja.md)
- [開発履歴・公開監査](docs/PUBLICATION.ja.md)
- [ABI資料](docs/ABI.md) / [統合API資料](docs/API.md)

### 追加監査で整備した資料

- [通常BS6局・4K切替の匿名検証マトリクス](docs/VALIDATION-MATRIX.ja.md)
- [スタンバイ・RAM・黒画面・競合・再投入の確認と復旧](docs/TROUBLESHOOTING.ja.md)
- [完全版I-O DATAを含む3社更新ファイル比較](docs/OEM-FIRMWARE-COMPARISON.ja.md)
- [現在の制限](docs/LIMITATIONS.md) / [統合版の構造](docs/HOW_IT_WORKS.md) / [API](docs/API.md) / [本体側導入](docs/SETUP.md)

本家由来の過去の否定結果・説明は`docs/history/`に履歴として保持しています。
