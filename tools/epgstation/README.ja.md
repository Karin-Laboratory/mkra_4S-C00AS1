# EPGStation用EPGプロキシ

実機開発ではEPGStation 2.10.0 / Node.js 18 / aribts 2.1.12を使用した。設定やDBは個別環境に依存するため同梱しない。導入前にconfig・DB・既存サービスをバックアップし、録画のない時間に作業する。

## 構成と依存

- `bs-eit.cjs`: PID0x12のH-EITをCRC確認後に解析。network4の通常BS6局だけを収集。
- `scan-bs.cjs`: 通常BSを標準60秒受信し、`bs-programs.json`をatomic更新。6局揃わない場合は旧cacheを保持。
- `scan-4k.cjs`: network11の4Kサービスを巡回し、mkraのEPGから`programs.json`を作る。
- `server.cjs`: 両cacheを併合してMirakurun APIを補完。チャンネル種別のBS4KをBSに表示変換し、IDは保持する。

プロキシは番組IDをnetwork/serviceへ解決し、サービス配信につなぐ。公開版ではcache上の終了時刻+1秒でHTTPを終了するため、私的環境の`mkra-compat.cjs`は不要。この終了処理の公開用整理は合成HTTP試験で確認したもので、稼働中の実機へ再配備していない。既存のshimを併用すると二重に終了制御するため、導入時に役割を確認する。

## 配置例

`198.51.100.10`は受信機を表す文書用アドレスであり、実環境へ置換する。

```sh
cd tools/epgstation
npm ci --ignore-scripts
# 任意の常設ディレクトリへこのディレクトリ全体を配置する。
export MKRA_URL=http://198.51.100.10:40772
export ARIBTS_PATH="$PWD/node_modules/aribts"
export CACHE_FILE=/var/lib/bs4k-epg/programs.json
export BS_CACHE_FILE=/var/lib/bs4k-epg/bs-programs.json
export LISTEN_HOST=127.0.0.1
export LISTEN_PORT=40773
node server.cjs
```

cacheディレクトリは実行ユーザーが書けるように用意する。別プロセスで`scan-bs.cjs`と`scan-4k.cjs`を動かす。両scannerは同じ受信機と同じcacheの設定を使い、`/var/lib/bs4k-epg/scan.lock`のflockで直列化する。

```sh
flock -n -E 0 /var/lib/bs4k-epg/scan.lock node scan-bs.cjs
flock -n -E 0 /var/lib/bs4k-epg/scan.lock node scan-4k.cjs
```

scannerはEPGStationが同じサーバの`http://127.0.0.1:8888`にある構成を想定。受信機使用中・予約開始3分前は延期する。通常BS巡回中も予約接近を検査するが、即時録画との競合を完全には防げない。API取得失敗時に安全確認を省略しない。

mkraは`--epg-tap --epg-refresh 5 --epg-dir /tmp/adsb`付きで起動する。4K側は最初の番組情報が揃う前にscannerが`epg-empty`として延期する場合がある。初期の4K受信でEPGが得られることを確認してから定期巡回を設定する。

EPGStationの`config/config.yml`例:

```yaml
mirakurunPath: http://127.0.0.1:40773/
```

`bs2k-epg-scan.service` / `.timer`は通常BS30分巡回の例。`serialized-scan.conf`は既存4K serviceへのdrop-in例で、`scan-4k.cjs`を`scan.cjs`として配置する想定。パス・実行ユーザー・Nodeの位置は環境に合わせる。4K scannerの2時間程度のtimer、プロキシ常駐service、共通lockの用意も必要。

## 2.10.0での録画・再生修正

`fix-transition-playback.py`は既知の設定でのみ使う任意パッチ。

- Baseline H.264の6つの変換設定へ`-pix_fmt yuv420p`を加え、10bit HEVCからのブラウザ用変換失敗を修正。
- 時刻指定予約で、HTTP配信開始を待った時間を再度待機しないよう、現在時刻から残り待機を計算。

EPGStationがidleであることを検査し、変更前ファイルを`--backup`へ退避する。設定数やソースの形が違う場合は停止する。他バージョンや既に修正済みの環境へ強制適用しない。DBやサービス再起動はこのスクリプトに含めない。

```sh
python3 fix-transition-playback.py --apply --backup /path/to/new-backup-directory
```

## テスト

```sh
npm ci --ignore-scripts
npm test
python3 -m py_compile *.py
```

合成EIT、ネットワークID分離・検索・経路変換、番組ストリームEOFと解放を試験する。ハードウェアを使わない。

実機録画用の`verify-transition.py`と`verify-guide-record.cjs`は、明示的に`--record`を付けた場合に予約を作る。公開作業では実行していない。検証時には単一チューナーの制約と既存予約を確認し、出力先を非公開の`evidence/`へ指定する。ブラウザ試験には別途playwright-coreとChromiumが必要。`EPGSTATION_URL`でUI接続先を変更できる。`verify-playback.cjs`には対象のrecord IDとvideoFile IDを指定する。録画データをリポジトリへ追加しない。

## 制限・参照

cacheの終了時刻はsnapshotで、番組延長・時刻変更に継続追従しない。通常BS6局の番組表が見えることは全期間・全局の録画保証ではない。隣接録画では先行番組が短縮される場合がある。

参照: [aribts](https://github.com/rndomhack/node-aribts)、[EPGStation 2.10.0](https://github.com/l3tnun/EPGStation/tree/v2.10.0)、[実測結果](../../docs/EPGSTATION-BS2K.ja.md)。
