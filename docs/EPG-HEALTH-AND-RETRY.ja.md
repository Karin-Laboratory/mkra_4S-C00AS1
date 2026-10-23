# EPG健全性監視と安全なBS4K再試行

## 対象と設計

CT141上で5分ごとに読み取り監視を実行し、`/var/lib/bs4k-epg/health.json`へ最新状態、`monitor.jsonl`へ履歴を保存する。番組タイトル、録画ファイル名、録画TS、CAS鍵、認証情報は記録しない。JSONLは10 MiBごとにローテーションし、過去5世代を残す。正常状態の基準スナップショットは初回導入時に`--baseline`で日時付き保存し、最大30件保持する。

監視は4Sのstatus/tuner/service API、CT141 proxyのprogram cache、EPGStation APIとSQLite、4K/BS2K cache、スキャナーとwatcherのsystemd状態、録画・予約、共通scan lock、4S uptime/OEM service readiness/mkraハッシュを採取する。4S側の読取りは、既存の安全制約付きTelnet helperで`/proc/uptime`、OEM process/device、実行バイナリSHA-256を参照する。通常監視からstream endpointや選局要求は発行しない。

各4Kサービスについて局別取得成功時刻、キャッシュ件数、次24時間件数と最終終了時刻、30分以上の空白候補、proxy件数、SQLite件数、前回スナップショットとの差分を記録する。放送編成上の空白は自動で欠落断定せず、`gapCandidates30m`として提示する。`api/status`の`epg.storedEvents`は既知の互換placeholderのため判定に使わない。

## 状態と再試行

- `healthy`: API、cache、局別取得時刻とSQLite整合性が正常。
- `degraded`: 一部の局の情報が古い/欠落、結果不一致、直近scanner失敗など。
- `deferred`: 録画中、予約直前、スキャン中など一時的な安全待ち。
- `critical`: 4S APIとtuner APIの両方が到達不能など、即時確認が必要。
- `unknown`: 必須情報を取得できず、原因を分類できない。

`bs4k-retry.timer`は5分ごとにretry controllerを確認する。2回連続した異常サンプル、明示的な4K scan失敗または局別12時間超の古さがある場合だけ、既存`bs4k-epg-scan.service`を開始する。開始前に以下をすべて確認し、確認不能はdeferする。

- mkra API/versionと4S OEM tuner/card/abs/adsb、driver/deviceが準備済み。
- 4S uptimeが10分以上。
- tuner free、録画0件、予約の安全余裕180秒以上。
- BS4K/BS2K scannerがinactive、共有scan lock free。
- cache filesystem空き2 GiB以上、MemAvailable 256 MiB以上。

再試行は共通lockを使う既存scannerへ委譲し、TS試験・4S操作・mkra停止/再起動は行わない。失敗後は30分、60分、120分の指数バックオフ（上限4時間）。3回失敗したら自動再試行を停止し、`critical`と人手確認理由を記録する。成功scanを確認するとbackoffをリセットする。BS2Kは既存30分timerに任せ、retry controllerから直接起動しない。

## 導入と確認

監視サービスは`bs4k-monitor.timer`、retry controllerは`bs4k-retry.timer`を使う。retry unitはrootで動くが、実行内容を固定したスクリプトと既存scannerの起動に限定し、systemd hardeningを適用する。テストではAUTO_RETRYをモックし、録画、予約、busy tuner、scan lock、4S起動直後、API timeout、OEM未準備、連続失敗を検証する。

実機反映前に既存`/usr/local/bin/bs4k-health`、monitor unit/timer、現在のJSONLとhealth stateのハッシュ付きバックアップを作る。新monitorは既存の5分timerから読み取り採取する。結果が期待通りでなければunitを戻し、monitor timerを停止せず旧コマンドへ戻す。retry unit/timerはmonitorの実機確認後に有効化し、retry状態とjournalを確認する。

## 既知の限界

passive monitorは映像/音声デコードやMMT ES進行を検査しない。4K TS取得状態は「未試験」と明記する。通常BS2K scanのdeadlineはsystemd resultとcache鮮度で監視するが、再取得が成功するまで根本原因未解決と扱う。7日間のデータはSQLite直接参照で比較し、EPGStationのUI/APIが全7日を実際に返すこととは区別する。

## CT141導入記録（2026-10-23 JST）

基準branch `fix/epg-preserve-partial-scan-integrated-20261023` のソースを起点に、復旧記録commit `3e869d0`を含む新規branch `feat/epg-health-monitoring-safe-retry-20261024`で実装した。導入commitは`2265ae01f123940cb0701aea70d644631ba48d1c`。`/usr/local/bin/bs4k-health`の実機SHA-256は`a9177d49225e49cadbef6be4bd3773b593ec08f1b662ea9de1cccdf84bae3293`。

既存monitor timerを維持し、監視serviceを置換してoneshot実行を確認した。22:39 JSTの基準snapshotは`/var/lib/bs4k-epg/snapshots/baseline-20261023T223933+0900.json`。実測は`healthy`、failure/warningなし、8局のproxy/cache/SQLite件数は24時間・7日とも一致、SQLite integrityは`ok`。BS4K cacheは2,159件、4S PID 861、実行path `/tmp/mt/mkra-unified-auto`、binary SHA-256は期待値と一致、tuner free、OEM readiness true。録画0件、180秒以内の予約0件、scan lock free。monitorはストリームを開かず、TS取得状態は未試験と記録した。

独立バックアップは`/var/backups/epg-health-monitor-20261023T223631+0900`、manifest SHA-256 `f023425965a4cab6a6b1138080a45f7ec7c871cb4d4b4d6e1bdfccc504ca3021`。24ファイルのバックアップhashを照合し、EPGStation DBはSQLite backup APIで複製して`integrity_check=ok`を確認した。保全時のDBはprogram 3,650、recorded 21、reserve 3、video_file 21。現行health cache、監視ログ、既存サービス、scanner、watcher、設定をバックアップした。

監視差し替え前に実配備コードとリポジトリを比較した。live `bs-eit.cjs`と`epg-cache.cjs`は一致。live `scan.cjs`は基準`scan-4k.cjs`と実接続先の既定値以外同じで、status schemaも一致。live `scan-bs.cjs`はBS cache、取得/録画安全判定、収集構造を維持し、接続先を実機値へ固定している。live `server.cjs`には既存EPGStation向けSSE `/api/events`、genre正規化、cache ID解決とprogram-stream動作差がある。live watcherはbinary/config pinと接続先を埋め込みにしている。これらの本番固有ファイルは上書きしていない。既存のdirtyなローカル作業treeも変更していない。

既存BS2K timerは22:32:56–22:33:58 JSTに正常終了し、6局のH-EITを取得した。直前の`scan deadline`の根因は未確定で、monitor/retry実装でscanner本体を変更していない。retry timerは現行健康状態確認後に有効化したが、初回判定はdeferred/no retryで、scannerは起動していない。正常時mkraを停止・再起動していない。
