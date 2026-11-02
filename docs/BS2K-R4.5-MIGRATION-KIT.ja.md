# R4.5 旧統合版からgraceful対応runtimeへの移行キット

Status: **オフライン実装・合成検証済み。管理ホストへの配置と受信機移行は未実施。**

この資料はR4における一時点の読取り観測を、現在状態として扱わず、次回保守の判断材料として保存する。R4.5では受信機・管理ホストへの接続、service/timer変更、停止、転送、選局、電源操作を行っていない。

## 旧版終了経路の証拠台帳

| 区分 | 根拠 | 結論 |
| --- | --- | --- |
| `VERIFIED_STATIC` | R3の基点`main`の`src/main.rs`にはSIGINT/SIGTERM handler登録がない。`cmd_serve`はfrontend flockを関数スコープで保持し、HTTP accept loopの後に`broker.shutdown()`を呼ぶ。`src/http.rs`の旧`serve`は`listener.incoming()`を受け続け、通常のclient切断ではaccept loopを終了しない。`panic=abort`である。 | 旧版にコードで保証された外部からのgraceful shutdown経路は確認できない。HTTP client disconnectもプロセス停止ではない。 |
| `VERIFIED_STATIC` | `src/bs2k.rs`、`src/remux4k.rs`等のDropは明示的teardownを行うが、processを通常signalで止めても旧版がそれらへ到達するコード経路は見つからない。 | SIGTERMでRust Drop、source/graph cleanup、TSI復元が実行されると仮定できない。kernelがprocess終了時にfd/flockを閉じても、driver graph cleanup成功を示さない。 |
| `OBSERVED` | R4時点の実行中ELFはSHA-256 `d5d8511cc530fd0c0375fca34f3604a2e8fd1cb76b49e819327ffe5a8d40a9f3`。過去の公開検証表は同hashを最終4K切替build、開発原本prefix `ccf4332`と対応付けている。 | R4の対象はR3 graceful buildではなかった。これは過去の一時点の識別情報であり、現時点の稼働状態ではない。 |
| `HISTORICAL` | 過去のwatcher再投入記録は、server不在時のRAM配置・起動成功を示す。旧プロセスをclean shutdownした証拠ではない。 | 旧版の正常停止・再投入を過去実績で証明できない。 |
| `VERIFIED_STATIC` + `UNKNOWN` | R3 buildはSIGINT/SIGTERM handler、HTTP accept停止、broker/source停止、handler drain、owner Drop、lock解放の順を実装し、ホストテスト済み。device signal ABIと実driver cleanupは未試験。candidate SHA-256 `1c38a830aa33172c54b29cbcdb395feb0c8b3acc53949a7d0a814d5030b26b40`。 | R3 buildのhost側shutdown契約は確認済み。実機で正常終了する保証はまだない。 |

したがって、現行旧版へSIGTERM/SIGINTを送る案は採用しない。guard helperが`/tmp/mt`配下のPIDへsignal可能であることは、対象ELFがsignalを処理する証拠ではない。

## 移行方式の比較

| 方式 | リスク・競合 | 復帰性 | 判断 |
| --- | --- | --- | --- |
| A. 旧版の正常終了経路を使う | その経路はソース・記録から確認できない。signal時のsource/driver cleanupが不明 | 低い | 不採用 |
| B. watcherによる通常の再投入処理 | API healthy時は何もしない。API異常でも既存mkra processがあれば停止・上書きせず延期する | watcher標準経路では稼働中ELFを切替できない | 単独では不採用 |
| C. watcher runtimeを先にgraceful版へ更新し、後日オペレータが計画的に電源OFF/ON | 同一機器を使い続けられる。受信機のRAM上にある旧processは、その電源遷移で初めて確実に消える。サービス停止方法・電源動作・作業窓が未確認なら実行しない | 管理側の旧bundleを別保管し、host runtimeは切戻せる。受信機の旧版再配置は別の計画的bootが必要 | 同一機器移行の条件付き第一候補。今回未実施。ACを抜く操作や自動電源操作は計画に含めない |
| D. 独立した隔離受信機で先に検証 | 本番との競合を避けやすいが、同等の予備機がある根拠はない | 予備機の準備・条件一致に依存 | 予備機が確認できれば最も低リスク。現在は可用性UNKNOWN |

Cは受信機でのgraceful shutdownを意味しない。旧版に安全な停止経路がない現状で、保守窓にオペレータが正規の電源操作を行う案である。製品の電源操作がOSを正常終了させるか、どの操作が完全なcold bootになるかは未確認なので、実機移行前に製品の既存手順を確認する。確認できなければ`NO-GO`。

## watcher runtime契約

R4時点のlegacy runtimeには`bundle.json`がなかった。一方、稼働中`watch.py`自身に実行ファイル・局表のSHA-256/MD5と実行ファイルsizeが埋め込まれ、そのsource hashと`sharp_telnet.py` hashもR4のprivate記録で測定されている。`maintenance_switch.py`はこの**R4で観測した完全一致profileだけ**を例外として認識する。Python ASTから数値・文字列literalだけを読み、ソースをimport/実行しない。watcher、helper、binary、config、inline pinsのどれかが違えば拒否する。未知のmanifest欠落を一律許容しない。

新しいbundleはschema v2にする。`bundle.json`がbinaryのsize/SHA-256/MD5、configのSHA-256/MD5、watcherのSHA-256、Telnet helperのSHA-256を固定し、`watch.py`自身も全資材を検証する。schema v1、manifest欠落、余分/不足field、hash mismatch、symlink、非root実行時のgroup/world writable fileは拒否する。SHAは整合性pinであり署名やビルド来歴証明の代わりではない。

bundle作成は既存`make_bundle.py`を使う。出力先は毎回新しいディレクトリとし、既存bundleを上書きしない。

```sh
./build-device.sh --locked
python3 tools/mkra-watch/make_bundle.py \
  --binary target/armv7-unknown-linux-musleabihf/release/mkra \
  --output dist/mkra-watch-r4.5-unique
```

作成bundleのbinary SHAが上記graceful candidate hashと一致しなければSTOPする。別hashを正規candidateと認定するには、別途build reviewとRunbook更新が必要。

## runtime切替・rollback tool

- `maintenance_switch.py`と`switch_runtime.py`は同じ管理ディレクトリへ別名で配置し、実行前に両方のsource SHA-256を照合する。レビュー済みhashは順に`68b324a0dd6ac70b071c1902f5fbb74a9c3ec246ca2f2b68d9b8332d48e9c064`、`61d6cd8ad1ed68e05616b86c93c3193e3b6e95cc02af5234054408b5e4f41799`。`maintenance_switch.py`はR4.5 helperを後者のhashに固定し、保守記録にもhelper hashを保存する。異なるhashは拒否する。
- `maintenance_switch.py preflight --target-bundle DIR` はruntime fingerprint、unit/timer状態、API/tuner、recording/reservation/scanner状態を読む。file/stateを書かず、flockも取得しないため、この結果はsnapshotでありrace-freeな許可ではない。`enter`は既存watcher/scan lockを取って再検証する。
- `enter --confirm-preflight --target-bundle DIR`は現在runtimeとschema v2 targetの両fingerprintをmaintenance stateへ記録してから、既存lockの取得とtimer待機へ進む。
- `switch_runtime.py install --bundle DIR`はmaintenance shellを終了した後に実行する。maintenance stateが`SESSION_EXITED`、関連timerがdisabled/inactive、serviceがinactive、watcher/scan lockを取得できること、bundleが事前承認hashと同じことを要求する。
- installは旧runtime filesを`/var/lib/4s-mkra-watch/runtime-backups/`以下のroot専用0700ディレクトリへ保存し、backup hashをmaintenance recordへ書いてからstageを検証する。runtimeの固定4ファイルを置換し、manifestを最後に置く。途中失敗は旧filesへrollbackを試みる。異常時はtimerをresumeせずbackupとstateを保全する。
- `resume --confirm-clean`は元runtimeまたは事前承認したtarget fingerprintのみ受け付ける。target runtimeなら新watcherが既存APIをread-onlyで確認し、通常状態へ戻す。
- `switch_runtime.py rollback`はtimer/service停止と両lock取得を再確認し、保存backupのhashを検証して旧runtimeを復元する。rollback資材/hash/stateが不一致なら拒否する。

## 条件付き実機移行手順

今回実行する手順ではない。各段階で条件を満たさなければSTOPする。

1. ユーザーが予定録画・視聴のない保守窓を確保する。API users=0、EPG recording=0、reservation guard内なし、scanner idle、OEM正常、receiver active、同一boot A baselineのfrontend/TSI/graph owner判別を読み取り確認する。
2. R4.5 kitとruntime bundleを別途レビュー済みのprivate管理領域へ配置し、target binary hash、bundle hash、unit hash、既存timer状態、既存runtime fingerprint、rollback容量を別プロセスでreadbackする。**本ロットではhostへ配置していない。**
3. read-only `preflight --target-bundle`を実行し、JSON結果をprivate logへ保存する。その後`enter --target-bundle`が再検証し、既存watcher/scan lockを保持したままtimerを待機させる。
4. maintenance shell終了後、`switch_runtime.py install`でhost watcher bundleだけを切り替える。この工程は受信機processを停止・signalせず、受信機ファイルにも触れない。
5. 同一bootのTSI/frontend/graph baselineを比較し、他資源が変化していないことを確認する。旧受信機serverのbinaryは依然旧版なので、ここでgraceful対応済みとは判定しない。
6. `resume --confirm-clean`でhost watcher、API、proxy、EPGStationを確認する。APIがhealthならwatcherは既存processを上書きしない。受信機はまだ旧ELFのままである。
7. 別の明示的なR5保守窓で予約・録画・視聴を再確認し、オペレータが正規手順で一度だけpower OFF/ONする。自動電源操作はしない。watcherが新bundleからcandidateを再投入した後、`python3 watch.py --verify-runtime`で`/proc/<pid>/exe`が指す実行中ELFのSHA-256を読み取り照合する。正確なhashが一致しない、APIが戻らない、graph/TSI ownerが不明なら再試行せずSTOP。
8. mismatchがあれば、受信機を追加で操作しない。host watcher runtimeをrollbackする場合もtimer停止・lock保護とidle条件を再確立し、記録済みbackupだけを使う。

## R4.5 オフライン検証

- runtime contract test: schema v2全資材検証、古い/部分manifest拒否、完全一致legacy inline-pin profile、式評価なしを確認。
- runtime switch test: 事前target不一致拒否、bundle install後の旧profile rollback、途中失敗時の自動rollback、rollback資材改変時の復元拒否を確認。
- read-only preflight test: state writeやflockなし、timer/service/API read checksのみであることを確認。
- running image verifier test: process pathが固定runtimeであり、`/proc/<pid>/exe` SHAがtarget pinと一致する場合だけPASS。
- `python3 -m unittest discover -s tools/mkra-watch -p 'test_*.py'`: **45/45 PASS**（maintenance switch testsを含む）。
- `cargo test --locked`: **216 passed / 0 failed**。
- `./build-device.sh --locked`: **PASS**。ELF32 ARM EABI5、静的リンク、`INTERP`なし。SHA-256 `1c38a830aa33172c54b29cbcdb395feb0c8b3acc53949a7d0a814d5030b26b40`。
- 生成schema v2 bundleを独立プロセスで検証し、`watch.verify_bundle()`がPASS。watcher self-testもPASS。
- `cargo fmt --check`は既存ソース全体のフォーマット差分を報告するためFAIL。自動整形は行わず、無関係な既存Rustファイルを変更していない。
- 実機上のsystemd/runtime切替、graceful終了、graph/TSI cleanup、電源操作と再投入は **UNKNOWN / NOT RUN**。
- 管理ホストのsystemd unit、旧runtime、実機signal/Drop/graph cleanup、bundle実配備、電源動作、再起動後のcandidate起動・API復帰: **UNKNOWN（未実施）**。

このkitはhost runtimeの安全な切替/rollback経路をオフラインで用意したものであり、実機移行成功を証明しない。次回実機R5は、同一boot baseline、受信機のidle/active状態、予約空き、private staging/backup、host runtime readbackを全て満たし、電源操作が正規かつ復元可能と確認できるまで**NO-GO**。
