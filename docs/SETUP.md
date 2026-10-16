# 統合版のビルド・配置・起動

既に本体の管理手段が使えることを前提とする。telnet有効化・内部フラッシュ変更・HDD初期化はこの手順に含まない。導入は自分の録画・視聴がない保守時間に行う。本資料更新では稼働中機器へ適用していない。

## 1. ソースとビルド

```sh
git clone https://github.com/Karin-Laboratory/mkra_4S-C00AS1.git
cd mkra_4S-C00AS1
git remote add upstream https://github.com/otti83/mkra_4S-C00AS1.git
rustup target add armv7-unknown-linux-musleabihf
./tools/build_unified.sh
file target/armv7-unknown-linux-musleabihf/release/mkra
readelf -d target/armv7-unknown-linux-musleabihf/release/mkra
```

純Rust・依存crateなし。純正バイナリや放送TSはビルドに不要。期待形式はARM EABI5 little-endianの静的ELF、動的依存なし。[Releases](https://github.com/Karin-Laboratory/mkra_4S-C00AS1/releases)ではライセンス・局表付きarchiveとSHA-256を配布する。

## 2. 本体の読み取り確認

受信機が緑LEDで純正サービスを起動していることを確認。管理shellで以下を読む。

```sh
cat /proc/uptime
cat /proc/meminfo
mount
pidof tuner
pidof card
pidof abs
pidof adsb
ls -l /dev/vixs/xcodedrv
ls -l /tmp/bdh_out_msgh.lst
```

赤LEDならリモコンで電源ON。緑でも黒画面や通信異常は別に確認する。watcherを使う場合はboot後45秒やRAM余裕等の条件がある。[非破壊的な確認と復旧](TROUBLESHOOTING.ja.md)を先に読む。

## 3. RAMへ配置

既存の管理手段でARM版を`/tmp/mt/mkra`、局表を`/tmp/mt/bs-additional-main.tsv`へ転送する。`/tmp/mt`作成・chmod・RAM書込みは導入時だけ行う。MD5/SHA-256を転送前後で比較し、古い実験バイナリと混同しない。

既存mkraが稼働中なら、実行ファイル・引数・録画状態を確認して保守時間を取り、所有者の運用手順で切り替える。純正プロセスや未知PIDのkill、実行中バイナリの上書きはしない。

`tools/deploy.py --smoke/--serve`は本家4K中心の配備・試験ツールで、通常BS6局込みの統合版を自動設定するものではない。新規統合配置はここで示す起動、または[manifest付きbundle](MKRA-AUTO-RESTORE.ja.md)を使う。

## 4. 統合起動

本体管理shell:

```sh
chmod 755 /tmp/mt/mkra
/tmp/mt/mkra serve --remux4k --tune --bs2k --channel-type BS4K --port 40772   --isdbs-channels /tmp/mt/bs-additional-main.tsv   --epg-tap --epg-refresh 5 --epg-dir /tmp/adsb
```

`--bs2k`には`--remux4k --tune`が必要。BS朝日は内蔵、他5局には追加TSVが必要。network4と11のIDを保つ。二重起動はfrontend lockで拒否する。

まずforegroundでエラーを確認する。常駐時もログは小さく保ち、受信機RAMへ長いTSやarchiveを保存しない。電源断で配置が消えるのは意図した動作。外部サーバからの[再投入](MKRA-AUTO-RESTORE.ja.md)を使い、内部フラッシュへ自動起動設定を書かない。

## 5. 接続の確認

管理ホストで、文書用アドレスを自分の受信機へ置き換える。下は状態読取りだけで選局しない。

```sh
curl --fail --max-time 5 http://198.51.100.10:40772/api/version
curl --fail --max-time 5 http://198.51.100.10:40772/api/tuners
curl --fail --max-time 5 http://198.51.100.10:40772/api/services
```

ライブstream要求は実際に選局する。通常BS後にHDMIが戻らない可能性を理解し、録画のない時間にだけ実施する。HTTP200やcodec名の表示で終えず、外部保存した標本の映像・音声をdecodeする。[実機検証表](VALIDATION-MATRIX.ja.md)。

## 6. EPGStation

新しい外部Linuxサーバのproxy/scanner/timer/config/backup/録画検証は[新規導入手順](EPGSTATION-INSTALL.ja.md)へ。EPGStationを本体へインストールする構成ではない。

本家由来のフラッシュ有効化等の説明は[旧SETUP](history/BASELINE-SETUP.md)に履歴として保存した。現行手順がフラッシュ変更を要求するものではない。
