# BS2K / BS4KのEPG連携ツール

**新規導入は[導入手順](../../docs/EPGSTATION-INSTALL.ja.md)を参照。** proxy、両scanner、timer、共通設定、backup、録画・再生検証を一続きで説明する。既存ホストへ無条件に配置・再起動するスクリプトではない。

| ファイル | 役割 |
| --- | --- |
| server.cjs | network4/11のcache統合、BS表示変換、番組stream終了制御 |
| bs-eit.cjs / scan-bs.cjs | 通常BS6局のH-EIT解析/60秒収集 |
| scan-4k.cjs | 4K各サービスを選局し、実際のprograms APIから取得 |
| epg-cache.cjs | 4K cacheの検証・network/service/event単位merge・原子的保存 |
| epg.env.example | 全3サービスのEnvironmentFile雛形。実IPは利用者が設定 |
| bs4k-epg-proxy.service | localhost40773のproxy常駐 |
| bs4k-epg-scan.service / .timer | 共通flock付き4K巡回、2時間周期 |
| bs2k-epg-scan.service / .timer | 同じflock付き通常BS巡回、30分周期 |
| health.py / bs4k-monitor.service / .timer | passive EPG/cache/proxy/SQLite監視、局別差分とローテート付き記録 |
| bs4k-retry.service / .timer | 安全条件・指数backoffを確認して既存4K scannerだけ再試行 |
| bs4k-epg.tmpfiles.conf | 実行ユーザーのcache directoryと共通lockを作成 |
| epgstation.service.example | 新規導入向けEPGStation常駐例 |
| serialized-scan.conf | 既存4K unitを移行する場合のdrop-in例。新規unitと二重管理しない |

`MKRA_URL`と`EPGSTATION_URL`、cacheパス、ARIBTS_PATHを全serviceで統一する。新規配置では`scan-4k.cjs`をその名前で使い、古い`scan.cjs`へのrenameは不要。

## ホスト試験

```sh
npm ci --ignore-scripts --prefix tools/epgstation
npm test --prefix tools/epgstation
python3 -m compileall -q tools/epgstation
```

`test_health.py`はAPI 503/timeout、1局/複数局欠落、placeholder値、cache破損、30分空白候補、録画/予約/lock、4S起動直後、OEM readiness、backoff上限、3回失敗停止、正常復帰をモック検証する。監視はstream endpointを呼ばず、4SのTS取得を自動試験しない。運用仕様は[健全性監視と安全な再試行](../../docs/EPG-HEALTH-AND-RETRY.ja.md)。

合成EIT、ID/経路、番組EOF、placeholderカウンタ0での4K初回収集、局別partial、503/短TS/予約延期、旧cache保全、破損時停止、atomic writeを確認する。`status=complete`は全8サービスの観測を示すだけで、番組表の全時間範囲の網羅を示さない。実機へは接続しない。

本体のstatus.storedEventsは0のplaceholderなのでscannerの起動条件にしない。4Kは各serviceの実programsから取得し、通常BSはCRC正常な放送H-EITだけを収集する。

## 動作範囲

開発時のEPGStation2.10.0での実測と、公開用EOF処理/新規配布unitのホスト試験を区別する。公開初版のEOFはcache終了時刻snapshotで、延長追従ではない。今回の配布設定を稼働中の実機へ適用していない。

`fix-transition-playback.py`は特定2.10.0構成向けの任意パッチで、設定/TS/distをbackupし、想定外なら停止する。`verify-transition.py`/`verify-guide-record.cjs`は`--record`で本当に予約を作るので、導入先の保守時間にだけ実行する。`verify-recorded-av.py`はlocalhost8888・録画先/recordedを前提とする。

[検証マトリクス](../../docs/VALIDATION-MATRIX.ja.md) / [トラブル対応](../../docs/TROUBLESHOOTING.ja.md)。生ログ・設定・DB・録画はGitへ追加しない。
