# BS2K保守切替・復帰Runbook

Status: **オフライン設計・試験済み。実機適用は未検証。**

このRunbookは統合版mkra/watch­er/EPG scannerと受信機の保守切替を行う将来の保守窓向け。実行例のホスト名、接続先、unit、配置は実機の読取り確認後に照合する。公開記録にLANアドレス、認証情報、番組名、予約ID、固有PIDを含めない。

## 適用条件

以下を全部満たせない場合は `BLOCKED` として、timer・process・受信機を変更しない。

- 既存管理経路と、接続先を固定したガード付きTelnet helperを再確認できる。
- 保守窓が録画・視聴・EPG収集・予約と競合せず、所要時間と復帰余裕を含む。
- 受信機は稼働状態で、OEM `tuner` / `card` / `abs` / `adsb`、driver、IPC、APIが正常。赤LED/standbyから強制起動しない。
- 統合版実行ファイルのSHA-256、`/proc/<pid>/exe`、引数、起動ログ、watcher bundle/manifestが既知の稼働版と一致する。
- 統合版が新しいgraceful-stop対応ビルドで、起動ログに `graceful shutdown enabled (SIGINT/SIGTERM)` がある。
- API上の使用者0件に加え、EPGStation録画0件、予約なし、両scanner inactiveを確認する。APIの`isFree`だけでfrontendの所有権を判断しない。
- Aの試験前baselineを保存し、FEM割当、TSI、graph、readerの所有関係を説明できる。
- 現在のsystemd unit、timer enable/active状態、watcher bundleを検査できる。異なるunit/hashならSTOPする。
- オフライン試験済みの `maintenance_switch.py` を管理ホストへ別途安全に配置し、配布SHA-256を照合済み。

旧R4 runtimeのwatcher切替だけを行う場合は、[`R4.5移行キット`](BS2K-R4.5-MIGRATION-KIT.ja.md)の完全一致legacy profileとschema v2 target bundleを使う。legacyの稼働中受信機processへはsignalを送らず、host runtime切替・復帰を受信機の計画的power OFF/ONと別段階にする。通常の保守試験で受信機serverを停止する場合、下記Phase Cのgraceful build条件は引き続き必須。

この版では新しいdaemonや永久的なpause設定を追加しない。既存watcherの `watch.lock`、EPG scanner共通の`scan.lock`、systemd timer、既存guard helperを使う。

## 起動と所有権の根拠

| 項目 | 静的根拠 | 運用上の扱い |
| --- | --- | --- |
| watcher | `tools/mkra-watch/watch.py` の `run_once()` は `watch.lock` を非ブロッキング取得。`restore()` はEPG `scan.lock` も取得してからstage/startする。APIがhealthyならupload/startしない。 | 両lockを保守セッションが保持し、timerを止める。既にlock保持者がいれば開始しない。 |
| timer/service | `tools/mkra-watch/4s-mkra-watch.timer` は短周期、serviceは`Type=oneshot`。配置例を本番設定と同一と決めつけない。 | 実機のFragmentPath、unit file hash、enabled/activeを開始時に記録し、保守中はtimerをstop+disable。serviceを強制stopしない。 |
| EPG scanner | BS2K/BS4K serviceは同じ`/var/lib/bs4k-epg/scan.lock`を`flock -n`で取る。 | 保守中はそのlockも保持し、両scanner timerをstop+disableする。scanner processが残ればBLOCKED。 |
| 統合版 | `serve --bs2k` は `/tmp/mkra-tuner.lock` を起動時から保持。watcherは`/tmp/mt/mkra-unified-auto`をbackground起動する。 | 新buildのSIGTERM受信後、HTTP accept停止→broker stop→source Drop完了→HTTP handler drain→driver所有者破棄→frontend flock解放の順。 |
| source cleanup | `bs2k::Source::drop` はreader/PID/DEMUX/buffer/pipelineを解放し、TSIを復元して`cleanup_ok`を記録。4K `Tap::drop`もteardown順序を記録する。 | 終了ログだけではclean判定しない。B〜Eの同一boot read-only baselineと照合する。 |

正常な終了経路は、graceful対応buildの通常`SIGTERM`のみ。旧buildはSIGTERMでデフォルト終了しDropやworker cleanupを保証しないため、実行ファイルhashと起動ログが合わない場合はsignalを送らずNO-GO。`SIGKILL`、`kill -9`、強制graph破棄、TSI手動復元は禁止。

## 状態遷移

| 状態 | 判定 | 許可すること | 禁止・次状態 |
| --- | --- | --- | --- |
| `NORMAL` | timer通常状態、統合版/API健康 | 読取り監査 | 事前条件を全て確認して`PRECHECK`へ |
| `PRECHECK` | 資材hash、録画/予約、scanner、受信機、A baseline確認 | 読取りのみ | 不明・不一致なら`BLOCKED`へ。lockが取れなければ変更前に停止 |
| `MAINTENANCE` | watcher/scan lock保持、関連timer inactive/disabled、統合版正常終了 | baseline収集、別名の試験バイナリ配置と1回の予定試験 | watcher再投入・EPG scan不可。信号待ちやcleanup異常なら停止 |
| `RESTORE` | D baselineがclean、TSI/graph/reader解放を確認 | `maintenance_switch.py resume --confirm-clean` | API/EPG健全性が戻るまで監視 |
| `BLOCKED` | 事前条件不足 | 証拠収集 | service/timer/receiverを変更しない |
| `STOP_FAILED` | 正常終了signal後もPID/API/lockが残る、または終了結果不明 | privateログ保存、状態を読み取る | 再signalの連打、強制kill、試験は禁止 |
| `DIRTY` | B/D baselineでTSI、frontend、graph、readerのcleanを示せない | private証拠保存、専門調査へ引継ぎ | timerは停止状態のまま。reset、handle破棄、watcher再開は禁止 |
| `RESTORE_FAILED` | 正常復帰timeout、API/proxy/EPGStation不健全 | 読取り監視、ログ保全。active timerの既定backoffを待つ | 手動再起動連打やstate/backoff削除は禁止。health不明なら自動で正常扱いしない |

## Phase A：現状と試験前baseline

すべての出力は管理ホスト上のmode `0600` private logへ保存する。公開レポートには集計値とhashだけを匿名化して記録する。

1. watcher/systemd unit、bundle/manifest、統合版のbinary hash・argv・PID、起動ログを読む。統合版が既知hashでなければBLOCKED。
2. `/api/status`、`/api/tuners`、`/api/services`のみをGETする。stream endpointを開かない。tuner users/`isUsing`が0でなければBLOCKED。
3. EPGStationのrecording/reserves、scanner service/timer状態を読み取る。作業窓全体と10分以上の既存watcher guard/復帰余裕を含む予約がないことを確認する。予約時刻の余裕を確定できない場合はBLOCKED。
4. 受信機uptime/boot ID、OEM process、driver/IPC、`/proc/<pid>/exe`、共有frontend flock、FEM assignment、全対象TSI MMR/status、graph device/pipeline/buffer/port/reader inventoryをsource確認済みのread-only方法で採取する。
5. 現在の統合版ownerとwatcher/scanner timerを基準Aとして記録する。graph handleとownerが同定できない場合はBLOCKED。

グラフの件数一致だけでは同一状態としない。handle、親子接続、reader/writer参照、owner PID、boot identity、TSI値を照合する。比較できる同一機器・firmware・同一boot baselineがなければUNKNOWNのまま止める。

## Phase B：watcher/scanner保守待機

`maintenance_switch.py` はホスト管理者として実行する。helperは受信機へ直接接続しない。

```sh
sudo python3 /usr/local/libexec/4s-mkra-watch/maintenance_switch.py status
sudo python3 /usr/local/libexec/4s-mkra-watch/maintenance_switch.py preflight --target-bundle /path/to/new-bundle
sudo python3 /usr/local/libexec/4s-mkra-watch/maintenance_switch.py enter --confirm-preflight --target-bundle /path/to/new-bundle
```

`enter` は次の順序で動く。

1. 既存watcher bundle hashとAPI/tuner、recording/reservation、scanner idleをread-onlyで再確認する。
2. watcherの既存`watch.lock`、続いてscanner共通`scan.lock`をnonblocking取得する。取得失敗ならtimerに触れずBLOCKED。
3. unit file hashと各timerの元のenabled/active状態をmode `0600` の保守記録に保存する。
4. watcherおよびBS2K/BS4K EPG timerを`disable --now`し、serviceがinactiveになるのを待つ。serviceをkill/stopしない。
5. 2つのflockを保持したまま対話shellを開く。timer起動競合や手動watcher起動は既存nonblocking flockで即時拒否される。

`preflight`はread-only snapshotでありlock barrierではない。`enter`はcurrentとtarget fingerprintを記録し、同じAPI/tuner/recording/scannerチェックをlock取得後に繰り返す。通常保守でtarget runtimeを使わない場合は`--target-bundle`を省略する。旧R4 runtime切替ではtargetが必須。

保守shell終了後もtimerは無効のまま。helperやhostが途中終了しても自動再開せずfail-closedにする。保守記録は削除しない。再起動をまたいだ場合はboot ID mismatchで自動resumeを拒否する。

## Phase C：統合版の正常停止

保守shell内で、再度API users、recording/reserves、OEM process、対象PIDのexeとhash、稼働ログを照合する。新しいBS2K実験バイナリを配置する前に行う。

正常停止は、既存guard helperの `signal_microtest(pid, "TERM")` / `stop_microtest(pid)`を使用する。このhelperは`/proc/<pid>/exe`が`/tmp/mt/`内と検証できるPIDだけにTERMを送る。直接の任意`kill`文字列や`SIGKILL`は使わない。新buildの起動ログにgraceful handlerが確認できない場合はsignalを送らずSTOP。

正常終了の根拠はすべて必要。

- 統合版PIDが終了し、該当binaryのプロセスがない。
- server logに`active source and HTTP handlers drained`が出て、その後にcleanup warning / `cleanup_ok=false`がない。
- 共有frontend flockが解放され、driver descriptor ownerが残っていない。
- APIプロセス消失は想定されるが、これはcleanup成功の証明ではない。
- 基準BのFEM/TSI/graph/readerをAと比較し、製品ownerの基線へ復帰したと示せる。

signal後にPIDやlockが残る場合は`STOP_FAILED`。SIGTERM再送を反復せず、timerを無効のまま証拠を保全する。Bでgraph/TSIが未知・差異なら`DIRTY`、captureを中止する。

## Phase D：保守作業前後のbaseline

状態スナップショットのタイミングを固定する。

| 点 | タイミング | 比較目的 |
| --- | --- | --- |
| A | 統合版停止前、同一boot | process/lock/TSI/FEM/graph ownerの基準 |
| B | graceful停止後 | worker cleanup、descriptor/lock解放、製品graphへの復帰 |
| C | BS2K試験直前 | Bと同じ状態を維持し、録画/予約が変わっていないこと |
| D | BS2K試験後 | BS2K作成資源が解放されTSI/FEM/graphがB/Cへ戻ったか |
| E | watcher復帰後 | 統合版とwatcherが通常状態に戻ったか |

A〜Eは同じboot ID・firmware・採取方法で採り、差分を保存する。TSI MMR/status、FEM assignment、driver graph、pipeline/device/buffer/port/reader、pid/exe/lock、API tuner、recording/reserves、unit/timer、watcher stateを含む。生のhandleやPIDを公開しない。

## Phase E：復帰

試験後DをB/Cと比較する。`cleanup_ok=true`一行やgraph件数一致だけではresumeしない。TSI/FEM readback、reader/buffer参照、owner、OEM process/APIの安全が確認できなければ`DIRTY`でtimerを止めたままにする。

全比較がcleanの場合だけ、保守shellを抜けて次を実行する。

```sh
sudo python3 /usr/local/libexec/4s-mkra-watch/maintenance_switch.py resume --confirm-clean
```

resumeはhost boot ID、unit hashes、watcher bundle hashes、保守記録、timer状態を照合し、開始前と同じtimer enable/activeを戻す。その後lockを解放して既存oneshot watcherを起動し、最大15分、direct API、proxy、EPGStation healthをread-onlyで待つ。既存backoffは消さない。復帰成功後Eを保存して保守記録を`RESTORED`にする。

R4.5 target bundleを`enter --target-bundle`で事前承認した場合に限り、runtime fingerprintは開始前bundleまたはそのtarget bundleのいずれかを受け付ける。`switch_runtime.py install --bundle ...`はshellを終了した後に実行し、disabled timer、inactive service、空きshared locks、十分なrollback容量を再確認する。失敗時は旧runtime backupへのrollbackを試みる。bundle差替えだけでは受信機processは更新されず、API healthyならwatcherは稼働中serverを保持する。

API/EPG healthが戻らない場合は`RESTORE_FAILED`と記録する。timerは開始前状態で動作し通常のbackoff/retryを担う。強制起動、受信機reset、予約DB変更はしない。timer状態が部分復帰なら自動判断せずmanual review。

## ロールバックと停止判定

- 保守開始前に問題: `enter`がtimerを部分停止した場合、元のtimer状態へ戻す。それが確認できないときは`ENTER_FAILED`として管理ホストのsystemd状態だけを調べる。
- 統合版が正常停止しない: `STOP_FAILED`。lock保護が維持できている間はtimerを止めたまま。kill -9禁止。
- Dで不一致/unknown: `DIRTY`。試験を続けない。watcher/scanner再開禁止。私有ログ・baselineを保全し、root原因が確立した別保守計画まで待つ。
- 復帰不成功: `RESTORE_FAILED`。既存watcherのログ/backoff/APIだけを読み取り、timerが元状態か確認する。state.jsonを消さない。
- 保守helperのrecord/hashが不一致、host boot IDが変化、unit/bundleが変化したら`BLOCKED`。記録を上書きしない。

## オフライン検証の限界

`maintenance_switch.py` のfake systemd/flock testは競合・部分失敗・二重実行をホスト上で模擬する。Rustテストはsignal後のHTTP accept停止、broker session stop、source Drop完了待ち、handler drainをホストで確認する。これらは実機driver cleanup、受信機OS signal ABI、graph/TSI復元、管理ホスト上のsystemd配置を証明しない。初回実機保守停止は、hash照合済みgraceful buildの段階配備と読み取りbaselineの確認が済んだ別ロットで行う。

### R3 オフライン検証記録

- `python3 tools/mkra-watch/test_maintenance_switch.py`: **14/14 PASS**（fake systemd/flock。timer競合、既存lock、途中失敗、recording拒否、二重開始、復元中断・部分復元など）。
- `python3 -m unittest discover -s tools/mkra-watch -p 'test_*.py'`: R4.5時点 **44/44 PASS**（legacy/v2 runtime contract、runtime install/rollback、rollback資材破損拒否、実行中ELF hash確認を含む）。
- `cargo test --locked`: **216 passed / 0 failed**。
- `./build-device.sh --locked`: **PASS**。成果物はARM 32-bit EABI5、musl向け、静的リンク。`readelf`で`INTERP` segmentなし。成果物SHA-256: `1c38a830aa33172c54b29cbcdb395feb0c8b3acc53949a7d0a814d5030b26b40`。
- `git diff --check`: **PASS**。
- systemd実機unit適用、管理ホストへのtool配置、実機signal応答、driver/TSI/graph復元、watcher正常復帰: **UNKNOWN（未実施）**。

このため本Runbookと保守helperはオフライン検証済みの候補であり、運用へ配備済みとは扱わない。Phase 1相当の実機保守切替へ進む前に、管理ホストのunit・bundle hash照合、読取り専用A baseline、保守窓、当該graceful buildの限定配備を別途レビューし、最初のSIGTERMで正常終了とlock解放を実測する必要がある。

### R4 実機受入前の読取り専用確認（2026-11-02 JST）

判定：**BLOCKED / NO-GO**。本確認ではサービス停止・再起動、timer変更、プロセスsignal、選局、TSI/graph変更を行っていない。BS2K試験も**NOT RUN**。

管理ホストから既存の管理コンテナ、そこに導入済みのguarded Telnet helperを通じて対象機へ到達した。新規認証情報・接続経路は使用していない。出力はprivate mode `0600` のログに保存し、この公開記録ではIP、ホスト名、PID、プロセス引数、driver handleを省略した。

| 確認項目 | 観測 | 判定 |
| --- | --- | --- |
| OEM fingerprint | tuner、abs、xcdrv、libvixsの4対象すべてがPR #2の期待値と一致 | PASS |
| OEM/driver | OEMプロセス4種、driver device、IPCファイルが存在 | PASS（存在確認のみ） |
| 統合版 | 現行実行ファイルSHA-256 `d5d8511cc530fd0c0375fca34f3604a2e8fd1cb76b49e819327ffe5a8d40a9f3`。実行中プロセスがdriver descriptorと共有frontend write-lockを保持 | 観測済み。停止不可 |
| graceful build | R3で検証したSHA-256 `1c38a830aa33172c54b29cbcdb395feb0c8b3acc53949a7d0a814d5030b26b40` と現行実行ファイルは不一致 | BLOCKED。現行プロセスへTERMを送らない |
| API/利用状況 | tuner `isUsing=true`、利用者1件。録画0件、予約3件。予約一覧だけでは作業窓全体の無競合を証明できず、利用中表示も解消していない | BLOCKED |
| 電源/受信状態 | 読取り診断がstandby（赤LED）相当を報告。強制的な電源状態変更はしない | BLOCKED |
| watcher/EPG scanner | watcher timerおよびBS2K/BS4K scanner timerはactive/enabled。one-shot serviceは観測時inactive。watcher healthはhealthy | watcher競合条件を再評価するまで変更不可 |
| R3 runtime契約 | 現行管理環境に`bundle.json`が存在せず、R3 helperが要求するruntime fingerprintを構成できない | BLOCKED。`enter`は実行しない |
| TSI | `0x6000=0x01000000`、`0x6004=0x00000001`、`0x6008=0x01000100`、`0x600c=0x00000200`、`0x61f0=0x00000010`。過去の既知正常値と一致 | 値の一致は観測。現在のclean ownershipを証明しない |
| graph | devices 11、buffers 12、pipelines 2、ports 22を読取り観測 | 同一boot baselineなし。由来・所有者UNKNOWN |

過去の既知正常TSI値は照合できたが、graphについて同一boot・同一運用状態の正常baselineは見つかっていない。したがって2 pipelineを残留異常と断定も、通常状態と認定もできない。frontend assignmentの独立した正常baselineも未確立である。APIの利用者数、lock保持、driver descriptorとstandby警告の組合せは、保守開始条件を満たさない。

R3 helperを一時的な新規`/run`ディレクトリへ隔離配置し、SHA-256 `efd603bec580c329ca2a30e711b9f769736f40369cb3d529884ea542ae4ee181` と`--help`を確認した。systemd unit/timerやmanifestへの登録はしていない。副作用調査で`enter`はsnapshot作成後にtimer変更等を行うことを確認したため、実行していない。watcher `--dry-run`にもstate/lock更新の副作用があるため使用していない。

#### R5へ進む前に必要な条件

1. 保守時間全体で録画・視聴がなく、予約およびEPG scannerと競合しないことを再確認する。API tuner利用者が0で、対象機が通常稼働状態であること。
2. 現行統合版とwatcher bundle/manifestを一体として特定し、R3 helperが参照する`bundle.json`等のruntime契約を解決する。実際に稼働するgraceful対応バイナリのSHA・起動ログを確認し、現行プロセスを置換する手順を別途レビューする。
3. watcher/scanner timer、実行中watcher、共通lockの状態を、保守切替helperと同一バージョンで矛盾なく確認する。
4. 同一bootの停止前baselineを取り、frontend assignment、全TSI値、graphのowner・参照関係、reader/bufferを比較可能にする。graph件数だけでclean判定しない。
5. graceful停止は対応ビルドを特定し、最初の一度だけ行う。停止後にPID消失、log上のdrain/cleanup、frontend lock/descriptor解放、TSI/FEM/graph基線復帰をすべて確認する。いずれかがunknownなら停止状態を維持し、強制cleanupしない。
6. 受入後の復帰は保存済みの元bundle/hash/argvを照合してから行い、API・lock・watcher health・録画予約状態を確認する。

R4では実機状態の取得まで到達したが、上記の独立ブロッカーにより保守移行は実施していない。復帰試験のPASSは未取得である。

### R4.5 offline migration kit

[`docs/BS2K-R4.5-MIGRATION-KIT.ja.md`](BS2K-R4.5-MIGRATION-KIT.ja.md)は旧版signal/Drop経路の証拠、移行方式比較、legacy inline-pin compatibility、schema v2 bundle、host runtime install/rollback、実行中ELF hash readback、R5条件をまとめる。R4.5でhost/receiver変更はしておらず、power cycle方式も未実施。
