# 統合版が受信・復号・配信する仕組み

本家otti83さんのBS4K経路を土台に、通常BS受信とライブCASを追加した。両方式は同じ物理チューナーを切り替えて使う。[ABI資料](ABI.md)は版依存の詳細、[制限](LIMITATIONS.md)は未解決事項を扱う。

## BS4K: 純正ESからREMUX

```text
アンテナ → CXD2868ER(RF) → SC1501A(ISDB-S3/TLV)
  → 純正MMT_DEMUX・CAS処理 → 映像/音声ESリング
                              ├→ 純正decoder → HDMI（接続される場合）
                              └→ mkra所有REMUX → 192B TTS → 188B TS → HTTP
```

BS4Kの復号はこのES取得より前に純正側で行われる。本家は複数readerを持てるESリングへREMUXを追加する実装を開発した。ES生成が継続すれば純正decoderのreaderが0でもHTTP取得できる場合があるが、HDMIが映ることを意味しない。

REMUXはPAT/PMTとPCR/HEVC/AAC-LATMを生成し、mkraが4バイトtimestampを除いて188バイトにする。既存の純正録画機能で番組を録画する必要はない。

## 通常BS: 新しいTS入力・CAS経路

```text
アンテナ → CXD2868ER → SC1501A(ISDB-S)
  → physical TSI0 → crossbar: logical TSI1
  → mkra所有type12 DEMUX → SYSTEM/TS ring → host reader
  → PAT/PMT/ECM追跡 → 既存card daemonでECM処理 → CPU MULTI2 → HTTP
```

FEM path1のDev0x2・FED通常BS選局を使う。188バイトserial入力とDEMUXの論理ポートを揃える。物理0→論理0で0バイトだった試験に対し、物理0→論理1で放送TS/PATを取得した。

readerはsize0でもdescriptorを返す必要がある。mkra所有のgraphを解放した後で保存TSI設定を復元し、writer/readerが残る場合は強制破棄せず次の選局をブロックする。

CASは稼働中の純正card daemonを使用。既存flockで直列化し、動的queue/boot/純正imageを確認、INS30/P2=2とINS34/P2=2で必要な処理を行う。実鍵はRAM内だけで使用する。通信timeoutが曖昧なら遅延応答を再取引と混同しないよう再送を止める。詳しくは[受信経路](BS2K-RECEPTION.ja.md)・[CAS](BS2K-CAS-INVESTIGATION.ja.md)。

## 方式切替と4K選局完了の照合

BS2K終了後はTSIを戻し、次の4K要求でFEMの方式と初期化を復元する。4K選局には本家が解析した純正ADSBの変更要求を使う。要求を送れたことを完了とはしない。

統合モードの最終版では、ハッシュが一致するadsbのsessionから要求/取得済みSID・NID、busy、RF/TSID/BID/OIDを読み、実際に取得したサービスが要求と一致することを確認する。さらに同じ映像・音声ESペアで新しい書込みが進むことを250ms間隔で観測し、接続前にsessionを再照合する。

MH-SDTだけ、次いでMPTファイルだけに頼る方式は、2K後に両方が古く残る実例で不十分と判明した。現在の統合経路の選局確認を「SIファイル2つが一致すればよい」と説明しない。4K専用の従来経路はソースに残るが、その説明を統合経路へ混ぜない。[失敗と修正](EPGSTATION-OTHER-4K-TRANSITION-20261016.ja.md)。

## 一基を共有するHTTP broker

同一TSキーは受信・CAS・ringを一つだけ作り、複数subscriberへ渡す。異なるキーは503で排他する。`/api/services/{id}/stream`もTSキーへ解決され、サービス別に全PIDを分離する機能ではない。最後のsubscriberが離れてから約3秒のlingerがあり、次局の最初の要求が503になることもある。

ネットワークID4（通常BS）と11（BS4K）を含めてサービスID・番組IDを作る。同じSID151でも400151と1100151で区別する。

## EPGStationと再投入の役割

- 本体mkra: 選局・TS受信・CAS・単一受信系の排他。4KのMH-EITをファイル/section ringから収集する。
- 外部EPGホスト: BS2KのH-EITを解析し、4K番組表と別cacheへ保存。proxyが両cacheを併合しEPGStationへ提供する。
- EPGStation: 番組表UI・予約DB・録画と再生変換。proxyの番組streamはcacheの終了時刻+1秒で閉じる。実時間の番組延長追従ではない。
- watcher: RAM上のmkraが消えたときだけ、hash付きbundleを投入。正常サーバや異常だが残る既存プロセスを上書きしない。

[新規導入](EPGSTATION-INSTALL.ja.md)と[運用トラブル対応](TROUBLESHOOTING.ja.md)を参照。

## 過去の説明との関係

[BS4K専用版の説明](history/BASELINE-HOW_IT_WORKS.md)を保存した。「mkraは復号しない」「視聴を止めない」は元の4K取り出し方式の説明で、2K統合後の全体には当てはまらない。「通常BSはソフトで不可能」も後の正の対照・経路検証で訂正した。地デジ・CSは別途実証が必要。
