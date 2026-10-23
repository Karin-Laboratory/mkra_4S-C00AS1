# SHARP 4S-C00AS1 EPG欠落修正の統合記録（2026-10-23）

## 対象と作業範囲

- 基準: `Karin-Laboratory/mkra_4S-C00AS1` `main` / `47e2c364e8f0f29e04039432be140cd63b685c2f`
- 比較PR: #1 `fix/epg-preserve-partial-scan-20261023`、head `133cb684e3bc8e0276f7d1813e9f3c250b15d1b9`（Draft）
- 統合作業は独立worktree/branchで行う。既存のローカル作業ツリーやmainは変更しない。
- 4S/CT141の現行状態は今回直接照会していない。保存済みのCT141資料は2026-10-18時点のsnapshotで、現状を示すものではない。

## 差分評価

PR #1は`scan-4k.cjs`の最終cache filterから`lastSuccess`依存の7日上限を取り除き、`endAt >= now - 6h`の有効イベントを保持する。旧cacheの`lastSuccess`が欠落していても番組自体の終了時刻までは表示可能な情報を残すという方向は正しい。回帰テストは旧形式cacheで、失敗局のfuture eventが残ることを確認する。

既存のpublic `main`には、4S `status.epg.storedEvents`がplaceholderの0でも選局走査を行う修正、EPGStation予約APIの設定可能化、録画中・予約前のtuner deferが既に含まれる。ローカルの`mkra-hdd-study` checkoutにはさらにBS2Kや4S実験に関する51個の独立コミットがあるが、これらをまとめて移植しない。公開mainと重複する互換修正だけを維持し、不要なBS2K/ホスト固有変更は除外した。

public sourceの`198.51.100.10`は予約済み文書用IPとしてそのまま保ち、private LAN IPを公開コードへ移さない。

ButlerX配下のローカル候補も保全したまま読み取り比較した。`work/bs4k-production-hardening/scan.cjs`はlastSuccess 7日filterを持ちPRの修正対象と一致。`work/bs4k-fix/scan.cjs`は失敗局のeventを時刻に関係なく残すため、失敗が続くと古い番組が期限なしで残る。`work/bs4k-epg-aggregator/scan.cjs`は別版で、public scannerに無条件で上書きできない。いずれもファイルは変更・削除していない。対象public repoのmainはcleanで、全変更は別worktreeに作成した。

## 原因の評価

**確認できたコード上の欠落機構:** 旧4K cache filterは、今回走査できなかった局について`lastSuccess`がない、または7日より古いと、番組の`endAt`が未来でもcacheから削除していた。PR #1の変更はこの局単位の消失を防ぐ。更新・延長された同一イベントは`networkId:serviceId:eventId`キーで置換し、未終了の古い番組も`endAt`から6時間を過ぎた後に破棄するため無期限には残らない。

**入力経路の既知障害:** 過去の2026-10-07調査では4SへのTCP connect timeoutが再現され、カーネルの`tcp_tw_recycle=1`が原因と特定された。runtime設定変更後は同一負荷でtimeoutが解消した。別途、EPG tap周期約14.2秒に対し5秒のscanner dwellが短く、初回は8局すべて新着0だった。`endAt`が無い入力に`duration`から終了時刻を補う順序にも不具合があった。25秒dwell等の対応後、保存ログでは全8局が正常になった。この履歴は現在の稼働状態や今回の503原因を証明しない。

今回、4S TS stream、tuner selection、現在の503発生、CT141 API、timer実行時刻は照会していない。したがって、いまの画面欠落についてA（TS/503）とB（cache filter）のどちらが主因かは未確定。修正はBのデータ消失を防ぐもので、失われたイベントを生成・復元しない。

| 層 | 保存済み証拠による評価 | 今回の状態 |
|---|---|---|
| A. 4S TS / tuner / 503 | 過去にTCP connect障害とtuner busy 503の経路を確認。2026-10-07の修正後に負荷試験pass | 現在のAPI/TSは未照会 |
| B. 4K scanner/cache | `lastSuccess`欠落・古さによる有効event削除がソース上で確認できる | 修正と合成回帰テストを追加 |
| C. 通常BS H-EIT | 別scanner/cache/networkId 4。6局分のH-EIT parser既存テストあり | 現行受信結果は未照会 |
| D. proxy/server/API | 4/11 cacheを統合。日時・network・service条件の合成API試験あり | 現行レスポンス未照会 |
| E. EPGStation/SQLite | 保全snapshot DBの総番組3,575件、networkId 11が1,921件（8 service）、networkId 4が1,654件（6 service）。DB snapshot SHA-256 `b2da21cf96712df4173505f7ecf5e42e75419e6d093dca3e731dc4517cbf1eed` | snapshotのみread-only確認。現行DBではない |
| F. timer/予約競合 | 2026-10-18 snapshotの4K timerはboot後5分・2時間周期。scannerは180秒以内の予約を見てdeferし、systemd flockと組合せる | 現行timer/recordingは未照会 |

PR本文は「historical 2,120件、restored 1,921件」と記録する。保存済みDB snapshotのnetworkId 11は1,921件で一致するが、2,120件の元集合・そのイベントID一覧は手元の証拠で確認できない。差の199件はこの変更や現在のDB snapshotから再構成できないため、引き続き未復元として扱う。予約DB/録画履歴はsnapshotに含まれるが、今回変更しておらず、件数はEPG修正成果と混同しない。

## 統合したコード

- PR #1のlastSuccess非依存cache保全を採用。
- 4K cacheの受け入れ範囲を`networkId=11`および8対象serviceに限定。networkId 4の通常BSを4K scanの最終filterで巻き込まない。
- event upsertとproxy統合を`networkId:serviceId:eventId`で照合。更新後の同一eventを置換し、4/11を混ぜない。
- 終了時刻が未到来のcache entryを保ち、終了から6時間後に期限切れとする。最終成功時刻の有無だけでは削除しない。
- malformed/不正構造cacheはscan開始時にエラーとして止め、通常のcache pathへ書かない。cacheは同じdirectoryの一時ファイルへfsyncしてrenameする。
- streamが設定dwellより早く切れた場合は失敗扱い。部分scanは各serviceの件数/range/`completeness=observed-window-only`を記録する。8局全て観測できないrunは`status=partial`とする。8局を観測しても全時間範囲の番組網羅を意味しない。
- 局ごとの更新は成功局の新しいeventだけをupsertし、失敗局や未列挙局の未終了cacheを保持する。
- 予約窓/tuner busy/API healthの既存defer動作は維持。サービス再起動や録画停止をscannerに追加しない。

## オフライン検証

`npm ci --ignore-scripts --prefix tools/epgstation`後、`npm test --prefix tools/epgstation`を実行する。

対象には以下を含む。

1. 8局の局別key/countと部分局保持
2. 4S placeholder `storedEvents=0`からのbootstrap
3. cache metadataなし、旧SQLite由来のrow形式相当
4. 503、短いTS stream、tuner busy、予約窓
5. 取得後の件数減少でも有効cacheを保持
6. 同一eventの延長/更新と重複排除
7. networkId 4/11の分離
8. EPGStation proxyの時刻/API条件
9. malformed cacheをbyte単位で保全
10. atomic rename failure時に旧cacheを保全

## 配備・復旧の判定

統合コードはsource-only。実機適用は**NO-GO（未実施）**。

次段階ではまずCT141のread-only API/SQLite/cacheをservice ID・network ID・時間帯別に比較し、4Sのread-only `/api/status`、`/api/tuners`、`/api/services/:id/programs`とログを確認する。通常のprograms照会はtunerを選局しない。TS stream開始、予約変更、systemd再起動、本番cache書込みはこの調査に含めない。

実適用の前には、current `programs.json` / `bs-programs.json`、EPGStation SQLite、設定、unit/timerを同一時点に独立backupしてSHA-256照合し、予約/録画中でないことを確認する。変更対象を4K scanner/proxy filesに限定し、cacheとDBは変更しない。合成テストとbackup確認の後、サービス再起動や本番cache書込みが必要なら、影響・rollback（backup復元と旧code再配置）を示して別途GO判定を待つ。
