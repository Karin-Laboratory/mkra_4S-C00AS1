# 非破壊的な確認と段階的な復旧

この文書の更新では実機操作をしていない。下記は利用者が状態を見て使う手順。最初は読取りだけ行い、録画・予約・既存PIDを保全する。**reset・kill・設定変更・ファイル削除を最初の診断にしない。** フラッシュ更新、未知GPIO/電源IPC、handle総当たりは使わない。

## 最初に外部ホストへ記録する

受信機アドレスは文書用。実IPへ置き換える。stream endpointはここでは叩かない（選局を引き起こすため）。

```sh
curl --max-time 5 http://198.51.100.10:40772/api/status
curl --max-time 5 http://198.51.100.10:40772/api/tuners
curl --max-time 5 'http://127.0.0.1:8888/api/recording?isHalfWidth=true'
curl --max-time 5 'http://127.0.0.1:8888/api/reserves?isHalfWidth=true'
systemctl status bs4k-epg-proxy bs4k-epg-scan bs2k-epg-scan 4s-mkra-watch --no-pager
journalctl -u 4s-mkra-watch -u bs4k-epg-scan -u bs2k-epg-scan --since '-20 min' --no-pager
```

管理shellへ接続済みなら受信機で以下を読む。出力は外部ホストで保持し、RAMに大きなdumpを作らない。

```sh
cat /proc/uptime
cat /proc/meminfo
mount
ps
pidof tuner
pidof card
pidof abs
pidof adsb
ls -l /proc/[0-9]*/exe 2>/dev/null
ls -l /dev/vixs/xcodedrv /tmp/bdh_out_msgh.lst
cat /proc/sys/net/ipv4/tcp_tw_recycle
dmesg | tail -n 80
```

予約ID・番組名・個別パス・実IP等を含むので生ログは非公開にする。カード応答/鍵を追加取得したり、生メモリを丸ごと採取したりしない。

## 症状別の判断

| 症状 | 読み取りで確認するもの | まず行うこと | しないこと/限界 |
| --- | --- | --- | --- |
| 赤LED/standby、ESなし | LED、uptime、純正process、API到達性 | 予約を確認し、利用者が通常のリモコン電源ON。その後boot/サービス安定を待つ | 自動wake成功を仮定しない。未知電源IPC/GPIOを送らない |
| RAM不足/Connection refused | meminfo、mountで/tmp種別、dmesgのlowmemorykiller、既存ファイルのサイズ | 視聴を閉じ、新しいcapture/backupを止める。必要な小ログを外部へ退避する | `/tmp/*`一括削除、card/IPCファイル削除、純正UIの無根拠な再起動をしない |
| 緑LED/HDMI黒、HTTP正常 | 4K HTTP/AVの過去または保守時の試験、native状態とテレビ実表示を分ける | 録画が動いているなら維持。HDMI問題として記録する | stream成功をHDMI復帰と呼ばない。確認のために稼働録画を別局へ変えない |
| 503 tuner busy | tuners.users、isUsing、予約、両scanner ActiveState | 自分の不要な視聴接続を閉じる。正常終了と約3秒lingerを待つ | priorityで横取りできると思わない。録画・他人の接続を強制停止しない |
| 2K後の4K予約失敗 | 同時刻のOperator/mkraログ、要求SIDと実取得、sourceエラー、使用bin hash | 現行source/bundleかを外部資材で照合し、次の保守時間に更新を計画 | 古いMPT/MH-SDTやHDMIだけで選局成功を断定しない。オンエア中に差替えない |
| watcherが復元しない | state.json、journal、bundle manifest、systemd、scan.lock権限、URL | HEALTHY/WAIT/DEFER/BACKOFFを区別し、条件が解けるまで待つ | 既存mkraがある状態でkill/上書きしない。失敗counterだけ消して連打しない |
| EPGが空/古い | 各cache updatedAt・局数、scanner status、実programs API、EPGStation updater | 予約なしの時間にservice単位の収集結果を確認。失敗cacheを保全 | storedEvents=0だけを判定に使わない。DBを削除/初期化しない |

## RAM不足からの復旧の順番

1. 自分が始めた保存・受信接続を通常の方法で終了し、dmesg/空き容量を外部へ記録する。
2. 外部保存先でhash照合済みかつ**自分が生成したと確認できる非使用の標本**だけ、個別にRAMから除去できる。対象を列挙・照合してから行い、一括削除はしない。IPC、純正設定、実行中binは対象外。
3. 大きなTSやarchiveは今後ホスト/HDDへ直接保存する。本体/tmpへの追加ステージをしない。
4. mkra不在・録画なし・純正サービス正常ならwatcherの条件を満たすまで待つ。既存processが残る場合は自動で停止させない。

開発では約66MB TSと複数約33MB標本が重なりOOMになり、約140MBの確認済み重複を除去して回復した。**一般的に140MB削除すればよいという基準ではない。** 純正UIの再起動はその時の限定復旧であり、公開の一般復旧コマンドにしない。

## 再投入のログを読む

- `HEALTHY; no upload/start`: 正常。録画中でも書き換えない。
- `mkra process exists but API is unhealthy`: 状態保全のため停止。PID/exe/引数を外部へ控え、保守時間に所有者が対処を決める。
- `OEM ... not ready` / `recording` / `scan lock held`: 故障断定ではなく安全待ち。純正service・録画・scannerが落ち着くまで待つ。
- `bundled ... integrity failure`: 外部資材が不整合。既存bundleを残して**新しい出力先**へmake_bundleし、ローカル照合後、保守時間に配置を切り替える。
- `BACKOFF`: 15〜300秒。timer45秒前後＋実測転送107秒も加わる。予約直前の電源復帰で間に合う保証はない。
- tcp_tw_recycleのガード拒否: 古いhelper/bundleの可能性を確認する。現行は完全一致の一時設定コマンドだけ許可。guardやfirewallを無効化して迂回しない。

`watch.py --self-test`はホスト上で試験する。`--dry-run`は実機へ接続するがRAM転送・起動をしない。`--dry-run`でも外部のstate/lockは扱うため、読取りだけの診断と同一ではない。

## 保守時間での次段階

API・processが戻らない場合でも、まず記録と外部資材のhashを保全する。予約と録画がないことを確認し、所有者が認識できる保守手順で**自分のmkraだけ**の正常停止/起動を検討する。CASがlatched、cleanupでgraph保持、純正serviceが異常な場合は強制解放やresetループを使わない。

HDMI復帰には利用者のコンセントresetが有効だった過去例があるが、最後の保守手段。必要データを外部へ保存し、録画なし、HDDへの書込み完了・安全な解除を確認してから検討する。自動で電源を切らず、reset後はRAM配置が消えることを前提にする。確実な自動復旧を保証しない。

予約競合やscanner失敗でDBを巻き戻さない。rollbackは[導入手順のバックアップ節](EPGSTATION-INSTALL.ja.md#7-バックアップパッチロールバック)に従い、元の予約・録画履歴を保持する。
