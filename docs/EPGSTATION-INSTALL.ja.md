# 外部LinuxサーバへのEPGStation新規導入

この手順は**新しい導入先で実行する手順書**。今回の資料補完では実際の4S-C00AS1・既存EPGStation・予約に適用していない。配布unitの構文と合成APIでのscanner/proxyはホスト検証するが、白紙のサーバから実機までの通し試験を行ったという意味ではない。

## 前提・構成

- 受信機は[SETUP](SETUP.md)の統合mkraが起動済み。内蔵CAS・受信条件・ファームガードに適合すること。
- 外部Linux/systemdサーバ上にEPGStation 2.10.0、Node、FFmpeg/FFprobe、Python 3、ビルド環境と録画ストレージを用意する。
- 開発実測はNode18/aribts2.1.12/EPGStation2.10.0。公開NodeテストはNode22でも実行する。Node22でEPGStation本体の新規buildまで実機検証済みとはしない。依存の互換性は導入先で確認する。
- 以下のEPGStationはSQLite、HTTP8888、`/opt/EPGStation`を想定。別DB/パスならbackupとunitを合わせる。
- `198.51.100.10`は文書用の受信機アドレス。必ず実環境へ置き換える。APIを外部インターネットへ開放しない。

```text
受信機:40772 ← proxy:40773 ← EPGStation:8888
       ↑         ↑
  4K scanner   4K / 通常BS cache
  BS2K scanner（共通flock、録画前は延期）
```

## 1. 既存導入との衝突を確認

既にサービス・設定・録画があるホストへ、新規導入手順をそのまま実行しない。まず読取りだけ行う。

```sh
systemctl status epgstation bs4k-epg-proxy bs4k-epg-scan.timer bs2k-epg-scan.timer --no-pager
ss -ltn
ls -ld /opt/EPGStation /opt/bs4k-epg /var/lib/bs4k-epg /etc/mkra
```

既設なら第7節のバックアップを取り、設定差分を確認して保守時間に移行する。私的shimを使う旧環境では、番組streamの終了処理が重複しないようにする。

## 2. 依存とソースを準備（新規サーバのみ）

Debian/Ubuntu系の例。Nodeは利用するディストリビューション/公式手順で用意し、`/usr/bin/node`の位置と版を確認する。下のaptはNodeを暗黙更新しない。

```sh
sudo apt-get update
sudo apt-get install git build-essential python3 ffmpeg sqlite3 curl util-linux
node --version
npm --version
ffmpeg -version
ffprobe -version
command -v node
```

本Forkの配置元を準備する。以後`MKRA_SOURCE`はこのcheckoutの絶対パス。

```sh
git clone https://github.com/Karin-Laboratory/mkra_4S-C00AS1.git mkra-source
cd mkra-source
MKRA_SOURCE="$PWD"
npm ci --ignore-scripts --prefix tools/epgstation
npm test --prefix tools/epgstation
```

EPGStation本体の手順・設定項目は[公式2.10.0 setup](https://github.com/l3tnun/EPGStation/blob/v2.10.0/doc/linux-setup.md)と[config template](https://github.com/l3tnun/EPGStation/blob/v2.10.0/config/config.yml.template)を基準にする。新規ディレクトリだけにcloneする。

```sh
sudo install -d /opt/EPGStation /opt/bs4k-epg
sudo chown "$(id -un):$(id -gn)" /opt/EPGStation /opt/bs4k-epg
git clone --branch v2.10.0 --depth 1 https://github.com/l3tnun/EPGStation.git /opt/EPGStation
cd /opt/EPGStation
npm run all-install
npm run build
cp config/config.yml.template config/config.yml
cp config/operatorLogConfig.sample.yml config/operatorLogConfig.yml
cp config/epgUpdaterLogConfig.sample.yml config/epgUpdaterLogConfig.yml
cp config/serviceLogConfig.sample.yml config/serviceLogConfig.yml
cp config/enc.js.template config/enc.js
```

既存のconfigへ`cp`で上書きしない。依存buildが失敗した場合はそのログと公式要件を照合し、稼働中の別環境で置き換えて解消しない。

## 3. プロキシ・scanner・実行ユーザーを配置

新規の専用ユーザーを作る。既に存在する場合は作り直さず所属と権限を確認する。

```sh
sudo useradd --system --home-dir /var/lib/bs4k-epg --shell /usr/sbin/nologin bs4k-epg
sudo useradd --system --home-dir /var/lib/epgstation --shell /usr/sbin/nologin epgstation
cp "$MKRA_SOURCE"/tools/epgstation/*.cjs /opt/bs4k-epg/
cp "$MKRA_SOURCE"/tools/epgstation/package.json "$MKRA_SOURCE"/tools/epgstation/package-lock.json /opt/bs4k-epg/
npm ci --ignore-scripts --prefix /opt/bs4k-epg
sudo chown -R root:root /opt/bs4k-epg
sudo install -d -o epgstation -g epgstation /var/lib/epgstation /recorded
sudo chown -R epgstation:epgstation /opt/EPGStation
sudo install -d -m 0755 /etc/mkra
sudo install -m 0644 "$MKRA_SOURCE/tools/epgstation/bs4k-epg.tmpfiles.conf" /etc/tmpfiles.d/bs4k-epg.conf
sudo systemd-tmpfiles --create /etc/tmpfiles.d/bs4k-epg.conf
sudo install -m 0644 "$MKRA_SOURCE/tools/epgstation/epg.env.example" /etc/mkra/epg.env
```

新規サーバの`/recorded`は**十分な空きのある録画ストレージ**を管理者が用意する。上記はmountや初期化をしない。既存データを持つ録画先へ再帰chownしない。

`/etc/mkra/epg.env`を編集する。全3サービスが同じファイルを読む。systemd EnvironmentFile形式で、`export`やshellの`$変数`展開は使わない。

```ini
MKRA_URL=http://198.51.100.10:40772
EPGSTATION_URL=http://127.0.0.1:8888
LISTEN_HOST=127.0.0.1
LISTEN_PORT=40773
CACHE_FILE=/var/lib/bs4k-epg/programs.json
BS_CACHE_FILE=/var/lib/bs4k-epg/bs-programs.json
STATUS_FILE=/var/lib/bs4k-epg/status.json
ARIBTS_PATH=/opt/bs4k-epg/node_modules/aribts
TZ=Asia/Tokyo
```

局表は受信機側、cacheは外部host側。cache初期ファイルは存在しなくてもよい。空の番組を捏造して埋めない。lockは両scannerとwatcherが同じ`/var/lib/bs4k-epg/scan.lock`を使う。

## 4. EPGStation設定

公式template全体を残し、必要な項目を変更する。これは差分例であり、これだけをconfig.yml全体にしない。

```yaml
port: 8888
mirakurunPath: http://127.0.0.1:40773/
dbtype: sqlite
epgUpdateIntervalTime: 10
recorded:
  - name: recorded
    path: /recorded
ffmpeg: /usr/bin/ffmpeg
ffprobe: /usr/bin/ffprobe
```

保存先・thumbnail/log等が実行ユーザーから書けることを確認する。ネットワーク4/11のサービスIDは同一化しない。4K→BSのtype表示変換はproxyが担当する。

10bit HEVCをH.264 Baselineへ変換する設定には`-pix_fmt yuv420p`が必要。2.10.0では時刻指定録画のstream待ち後に古い時刻でさらに待つ問題も実測した。第7節のbackup後、既知の構成だけに`fix-transition-playback.py`を適用できる。設定数やソースの形が違えば停止するため、強制適用しない。

## 5. unitを登録してAPIだけ確認

これは**新規サーバ**の操作例。既存の同名unitを上書きしない。

```sh
sudo install -m 0644 "$MKRA_SOURCE/tools/epgstation/bs4k-epg-proxy.service" /etc/systemd/system/
sudo install -m 0644 "$MKRA_SOURCE/tools/epgstation/bs4k-epg-scan.service" /etc/systemd/system/
sudo install -m 0644 "$MKRA_SOURCE/tools/epgstation/bs4k-epg-scan.timer" /etc/systemd/system/
sudo install -m 0644 "$MKRA_SOURCE/tools/epgstation/bs2k-epg-scan.service" /etc/systemd/system/
sudo install -m 0644 "$MKRA_SOURCE/tools/epgstation/bs2k-epg-scan.timer" /etc/systemd/system/
sudo install -m 0644 "$MKRA_SOURCE/tools/epgstation/epgstation.service.example" /etc/systemd/system/epgstation.service
sudo systemd-analyze verify /etc/systemd/system/bs4k-epg-proxy.service /etc/systemd/system/bs4k-epg-scan.service /etc/systemd/system/bs2k-epg-scan.service /etc/systemd/system/epgstation.service
sudo systemctl daemon-reload
sudo systemctl start bs4k-epg-proxy.service epgstation.service
curl --fail --max-time 5 http://127.0.0.1:40773/api/services
curl --fail --max-time 5 http://127.0.0.1:8888/api/channels
```

この時点ではtimerを有効にしていない。EPGStationが新規DBを作り、APIが応答することを確認する。proxyはcache未収集ならprogramsが空でもよい。APIに2K6局と4K系局が存在すること、1基のチューナーであることを確認する。

## 6. 初回収集と定期timer

**ここからは実際に選局する。** 受信機・EPGStationに録画がなく、予約が直近にない保守時間で実施する。2K後のHDMI復帰未解決を理解してから進める。

```sh
sudo systemctl start bs4k-epg-scan.service
journalctl -u bs4k-epg-scan.service -n 80 --no-pager
sudo systemctl start bs2k-epg-scan.service
journalctl -u bs2k-epg-scan.service -n 80 --no-pager
curl --fail --max-time 10 'http://127.0.0.1:40773/api/programs?networkId=4'
curl --fail --max-time 10 'http://127.0.0.1:40773/api/programs?networkId=11'
```

4K scannerは対象serviceを選局して本体SIの取得を待つ。本体statusの`storedEvents=0`はplaceholderで、起動条件にしない。初回から実際の`/api/services/{id}/programs`で判定する。情報が届かなければ失敗serviceとして記録し、次の保守時間に再確認する。新規cacheが空なら録画検証へ進まない。

通常BSは標準60秒で6局H-EITを収集し、揃わなければ旧cacheを保持して失敗する。4Kは各serviceの成功/失敗を記録し、失敗時の古いcacheには最大7日等の保持条件がある。単にscannerがexit0で終わったことではなく、status/cache内の各局番組数と鮮度を確認する。

```sh
python3 - <<'CHECK'
import json
from pathlib import Path
for name in ['programs.json','bs-programs.json']:
    data=json.loads((Path('/var/lib/bs4k-epg')/name).read_text())
    counts={}
    for p in data['programs']:
        key=(p['networkId'],p['serviceId']);counts[key]=counts.get(key,0)+1
    print(name,counts)
CHECK
sudo systemctl enable --now bs4k-epg-proxy.service epgstation.service bs4k-epg-scan.timer bs2k-epg-scan.timer
systemctl list-timers --all bs4k-epg-scan.timer bs2k-epg-scan.timer
```

周期は通常BS30分、4K2時間。予約開始3分前は延期し、通常BSは収集中も接近を検査する。共通flockが競合巡回を避けるが、手動の即時録画との競合を完全には防げない。UI取り込みは別周期で、イベント未対応時にはさらに約15分＋処理時間がかかる場合がある。

## 7. バックアップ・パッチ・ロールバック

設定・DB・proxy・unit・cache・予約一覧を**非公開の外部host上**へ退避する。録画のない時間に取得する。DB単体のcpではWALを取りこぼすためSQLite backup APIを使う。ソースDBの存在を厳密に確認し、間違った名前で空DBを新規作成しない。

```sh
BACKUP_DIR="/var/backups/mkra-epg-$(date +%Y%m%dT%H%M%S)"
sudo install -d -m 0700 "$BACKUP_DIR"
sudo cp -a /etc/mkra "$BACKUP_DIR/mkra-config"
sudo cp -a /opt/EPGStation/config "$BACKUP_DIR/epgstation-config"
sudo cp -a /opt/bs4k-epg "$BACKUP_DIR/proxy-source"
sudo cp -a /var/lib/bs4k-epg "$BACKUP_DIR/proxy-cache"
sudo cp -a /etc/systemd/system/bs4k-epg* /etc/systemd/system/bs2k-epg* /etc/systemd/system/epgstation.service "$BACKUP_DIR/"
sudo python3 - "$BACKUP_DIR/database.sqlite3" <<'BACKUP'
import sqlite3,sys
from pathlib import Path
source=Path('/opt/EPGStation/data/database.db').resolve(strict=True)
target=Path(sys.argv[1])
if target.exists(): raise SystemExit('backup already exists')
with sqlite3.connect(source.as_uri()+'?mode=ro',uri=True) as src:
    with sqlite3.connect(target) as dst:
        src.backup(dst)
        if dst.execute('PRAGMA integrity_check').fetchone()[0]!='ok': raise SystemExit('integrity failure')
BACKUP
```

2.10.0のSQLite位置は公式`DBOperator.ts`の`data/database.db`を確認したもの。MySQL構成では上のSQLite操作を使わず、管理者の正規backup手順を使う。既存導入では元のunit有効状態・予約一覧も保存する。バックアップは録画番組本体の複製ではない。

任意パッチの適用例（新規または保守中のidle環境だけ）:

```sh
sudo python3 "$MKRA_SOURCE/tools/epgstation/fix-transition-playback.py" --apply --backup "$BACKUP_DIR/playback-and-timing"
sudo systemctl restart epgstation.service
```

パッチは実行中録画を検査し、変更前config/TS/distを保存する。適用済み・想定外なら止まる。TypeScriptを後日rebuildするとdistだけの変更を失い得るため、TSとdist両方の差分を管理する。

戻す際は、まず新しい収集timerの**次回起動を停止**し、active scanner/録画が終わるまで待つ。保守時間にproxy・設定・unitを退避版へ戻してdaemon-reloadする。既存予約・録画履歴を失うため、DBの巻き戻しは通常の復旧に使わない。必要なら現在DBも別途backupして、喪失する差分を確認してから行う。受信機をresetする手順ではない。

## 8. 番組表から録画・再生を検証

UI `http://<EPG-host>:8888/`で通常BS6局とBS4Kの番組表を確認する。番組表から自分の短い試験予約を作り、開始、ファイル増加、終了、録画一覧を確認する。元の予約と混同しない。

CLIの隣接予約試験はEPGStation hostで、保守時間にだけ明示的に実行する。

```sh
cd "$MKRA_SOURCE"
python3 tools/epgstation/verify-transition.py --record --gap 0 \
  --bs2k-channel 400141 --bs4k-channel 1100141 \
  --output evidence/local-transition-test
```

このツールは45秒2K→75秒4Kの2予約を作り、前後予約guardを確認して待つ。1基のため先行末尾が約15秒短くなることがある。ファイルは保持する。実予約の成功を確認しても元の全番組無欠落を保証しない。APIはlocalhost8888固定。

録画IDとvideoFile IDは自分の結果から取得する。`verify-recorded-av.py`は録画先`/recorded`とlocalhost8888を前提とする。他の配置では手動でffprobe/ffmpegを使う。

```sh
python3 tools/epgstation/verify-recorded-av.py RECORD_ID --output evidence/local-av
npm install --prefix target/browserdeps --ignore-scripts playwright-core
NODE_PATH="$PWD/target/browserdeps/node_modules" EPGSTATION_URL=http://127.0.0.1:8888 \
  node tools/epgstation/verify-playback.cjs RECORD_ID VIDEO_FILE_ID evidence/local-playback/
```

ブラウザ試験には`/usr/bin/chromium`が必要。録画検証はHEVC/MPEG-2と音声の実decode、MP4再生5秒以上、終了コード・警告を確認する。ミュート試験を人による聴取成功とは数えない。負荷を分離するためAV decodeとブラウザ再生は直列に行う。

番組表ボタン操作を自動検証する場合は`verify-guide-record.cjs --record`を使うが、約30秒後にその試験予約を削除して終了する。この途中終了は番組全編の自然終了試験ではない。録画・DB・画面・ログをGitへ追加しない。

## 9. RAM再投入との接続

[MKRA-AUTO-RESTORE](MKRA-AUTO-RESTORE.ja.md)のbundleを別途作る。watcherの`MKRA_API`とscannerの`MKRA_URL`を同じ受信機へ設定し、watcherのproxy/EPGStation URLを合わせる。watcherも同じscan.lockを使い、実行ユーザーが読めることを確認する。watcher用stateディレクトリも別に用意する。

正常APIなら再投入しない。異常でも既存プロセスがあれば強制killしない。[トラブル対応](TROUBLESHOOTING.ja.md)へ進み、cold boot全自動復旧を保証する設定として扱わない。
