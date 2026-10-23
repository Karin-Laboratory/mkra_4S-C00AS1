# 4S-C00AS1 4K選局・TSソース復旧 第2段階

調査日: 2026-10-23 JST

基準ソース: `588c82657418a33b2bfb5d69f5d0da56c29f25c1`

初回調査: 読み取り調査のみ。後続の復旧確認では、既存watcherによる揮発領域への復元と、利用者が行った4S電源操作後に安全条件を確認して4K stream試験・EPG再スキャンを実施。

## 接続経路

agent-101-vmからSSHでCT141 (`root@192.168.1.52`) に接続し、CT141から4S (`192.168.1.51:2323`) の既存Telnet root shellとHTTP API `:40772` を使用した。接続手順は [4S-ACCESS-ROUTE.ja.md](4S-ACCESS-ROUTE.ja.md) に記録した。APIはstatus / tuners / servicesのGETだけを実行し、新たなstream・選局要求は送っていない。

## 実機の状態と識別

- 4S hostname: `vixs`; Linux 3.10.24 ARM。
- `mkra` PID 828、実行パス `/tmp/mt/mkra-unified-auto`。引数は `serve --remux4k --tune --bs2k --channel-type BS4K --epg-tap --epg-refresh 5 --port 40772 --epg-dir /tmp/adsb --isdbs-channels /tmp/mt/mkra-bs-auto.tsv`。
- 実行ファイルは984,708 bytes、SHA-256 `d5d8511cc530fd0c0375fca34f3604a2e8fd1cb76b49e819327ffe5a8d40a9f3`、MD5 `48a383d70c8e463eec7d5e54ccaf47cc`。
- `/proc/828/wchan` は `inet_csk_accept`、3 threads、sleeping。採取時の `top` はmkraを約101% CPUと表示し、load averageは約27.5。これらは観測値であり、4K障害の原因と断定しない。
- `/api/status`, `/api/tuners`, `/api/services` はHTTP 200。tunerはavailable/freeで、利用者・セッションなし。BS2KとBS4Kのサービス一覧を返した。
- 21:57 JST時点で `/api/status` のstreamCountは0、sessionはnull。CT141の録画APIはactive records 0、現在有効な将来予約0、4K/2K scanner unitはいずれもinactive。
- 既存CLIの `remux4k --discover-only` を1回実行した。これはドライバを開き、graphを読み、buffer `GET_STATUS` を呼び、作成処理の前に終了する読み取り専用経路。結果は `no live 4K ES pair found`。選局要求・チューナー操作・REMUX作成は行っていない。
- CT141 watcherは45秒周期で動作し、API正常として `HEALTHY; no upload/start`。これは稼働確認であり、4K選局の回復を試すwatcherではない。
- 生ログは265,279 bytes、SHA-256 `b428dddf3a817d3b1bcc670c6923e9a48cdc82b4c75da40956943d3df6517b7b`。予約・録画情報や生ログはGitへ収録していない。

## ログ時系列と確認できた原因

既存ログではBS2K受信時にDEMUX・full-TS ring・output接続・reader・buffer readyが順に成功し、約731千TS packetsを処理して、`cleanup_ok=true` で終了している。続いてTSIの設定とidentity mappingが元へ戻ったという記録がある。その後、8局の4K stream要求がすべてTS source生成に失敗した。代表的なエラーは、要求サービスとADSBの取得済みpackageがsettleしないというもの。BS4Kの新しい映像・音声ES pairが見つからないとのエラーも残り、ログはgreen/watching状態を要求している。

これはHTTP 503が根本原因だという証拠ではない。HTTP handlerはsource生成失敗を503へ変換する。失敗した段階は、BS4K選局完了の確認またはライブES取得以前である。

ただし、取得ログは要求SID・ADSB取得済みSID・busy値・RF/TSID比較の各値を記録していない。したがって、ADSBが選局要求を処理していないのか、native受信がwatching状態にないのか、または別の状態遷移が止まったのかは区別できない。ここが現時点の未確定点であり、ハードウェア／電源状態を原因と断定しない。

## ソース・過去成功版との照合

現行 `src/native_selection.rs` は、検証済みADSBイメージの選局要求だけでなく、取得済みSID/NID、busy値、RF、TSID、BIDの一致を要求する。4K source開始処理は選局要求後最大11秒待ち、取得済みサービスが一致したうえで同じ映像・音声ES pairへの新規書き込みを確認する。5秒後の `TUNE_RESUME` は、ES graphが未発見の場合に一度だけ試す。MPT等の古い表示情報だけで成功扱いしない設計である。

過去の実機成功記録は [4K切替修正記録](EPGSTATION-OTHER-4K-TRANSITION-20261016.ja.md) と [検証マトリクス](VALIDATION-MATRIX.ja.md) にある。NHK BSP4Kおよび一部のBS2K後4K予約では取得済みpackageと新しいESを確認しているが、すべての局・すべての電源状態の保証ではない。現行ソースからは、取得状態を厳密に確認する判定条件の緩和が解決策だと示す証拠はない。

ARMv7クロスビルドの出力は実機バイナリとサイズ・SHA-256・MD5のすべてが一致した。従って、この基準ソースと実機バイナリの内容は一致する。差異を仮定してソースを変更する理由は見つからなかった。

## 再起動後の復旧確認（22:03–22:14 JST）

利用者から、前面LEDが緑→電源ボタン操作で赤→再び緑になり、`/tmp` のmkraが消えた可能性があるとの連絡があった。読み取り確認で4Sのuptimeが約84秒、`/tmp/mt` が消失、mkraプロセスなし、API接続拒否を確認した。したがって4Sが再起動して揮発領域が初期化された状態と整合する。LED色だけから電源状態や原因は断定しない。

CT141の既存 `4s-mkra-watch.timer` は、scan lock解放後に既定のSHA-256検証済みバイナリとチャンネル設定を `/tmp/mt` へ復元し、mkraを起動した。これは既存watcherによる通常復旧であり、手動のプロセス停止・ファイル上書き・設定変更は行っていない。起動後のwatcherは `HEALTHY; no upload/start`。実行バイナリSHA-256は基準ソースのクロスビルドと一致する `d5d8511cc530fd0c0375fca34f3604a2e8fd1cb76b49e819327ffe5a8d40a9f3`。

4S APIとチューナー状態が正常、録画中0件・有効な予約直前0件・scan lockなし・チューナーfreeを確認してから、既存の4K stream endpointを1回だけ試験した。HTTP 200で1 MiBを約4秒間取得できた。nativeログではvideo/audio ES ringが進行し、REMUX出力は約14.35 MB、74,748 TTS packets、resync 0。接続終了後にチューナーはfreeへ戻った。これは4K TS sourceが取得できた証拠だが、映像・音声のデコード再生までは検証していない。

同じく録画・予約・チューナー・scan lockの安全条件を再確認し、既存の `bs4k-epg-scan.service` を1回実行した。22:08:27–22:14:19 JST、status 0、8/8局成功、invalidCount 0、全局 `completeness=observed-window-only`。サービス別取得件数はSID 151:260、211:180、141:285、171:204、181:250、101:324、161:249、221:343。更新後のキャッシュは2,159番組（既存範囲を含む）。

EPGStationの今後24時間の8局すべてでAPI表示件数とキャッシュ件数が一致（37, 41, 42, 36, 38, 35, 25, 49）。30分以上の空白はSID 161の10月24日04:01–04:59のみ検出。SQLiteの `integrity_check=ok`、program 3,657、recorded 21、reserve 3、video_file 21。今後7日間のSQLite番組とキャッシュも局別件数が一致（SID 101:319、141:281、151:255、161:244、171:200、181:246、211:179、221:338）。schedule APIの `days=7` クエリは約1日分しか返さなかったため、7日確認はSQLiteとキャッシュの読み取り比較によるもので、7日分のUI表示自体を検証したものではない。

同じ起動バイナリで、再起動前はライブ4K ES pairが見つからず、復元・起動後のstream取得と全局スキャンは成功した。これらの事実は再起動後に受信状態が回復したことを示すが、原因となった内部状態や、電源ボタン操作・再起動と回復との因果までは特定できない。ソース変更や新規バイナリ配備は不要と判断し、行っていない。

通常BS2K scanは4S再起動中に `scan deadline` で失敗した記録がある。BS2Kキャッシュは既存の部分失敗保護で保持されたと確認しているが、BS2Kを再スキャンして復旧したとは扱わない。

## watcherと復旧可否

CT141の `4s-mkra-watch.timer` は45秒ごとにwatcherを実行する。API正常時は `HEALTHY; no upload/start` として終了する。API異常時も既存mkraプロセスが確認できれば「停止・上書きしない」と延期し、録画・予約・scan lockを確認してから、プロセス不在時だけvolatile `/tmp/mt` へ検証済みファイルを配置して起動する。

4SのBusyBox起動スクリプト・`/etc/inittab`には独自mkra起動設定が見つからず、`/tmp/mt`にも手動の通常停止・再開スクリプトはない。実際の復旧はCT141側に既設のwatcherが行った。プロセスkill、フラッシュ書込み、4S設定変更は実施していない。

## オフライン検証

- `cargo test --locked`: 214 passed, 0 failed。
- `./build-device.sh --locked`: ARMv7 static ELF成功、984,708 bytes。
- ビルドSHA-256 `d5d8511cc530fd0c0375fca34f3604a2e8fd1cb76b49e819327ffe5a8d40a9f3`、MD5 `48a383d70c8e463eec7d5e54ccaf47cc`。実機の値と一致。

ソース修正は行っていない。今回の4K取得とEPGキャッシュは復旧したが、4Sの再起動でどの内部状態が変わり回復したか、またBS2K scan deadlineの原因は未確定である。次の調査では、録画・予約・チューナー使用の安全性を維持しつつ、選局要求値と取得済み状態、native decoder/ES状態、BS2K scanのdeadline時系列を同時に記録する。
