# 通常BSの受信経路

2026-10-16の実機検証を基にした公開用資料。純正コードやダンプは含まない。

## 結論

SC1501AのISDB-SロックだけではTS取得にならない。復調出力、SoC入力割当、DEMUXの論理ポート、ring/readerを一致させる必要があった。

`SC1501A ISDB-S → physical TSI0 → logical TSI1 → type12 DEMUX → SYSTEM/TS ring → host reader`

## 設定の要点

1. FEM path1にDev0x2を割り当て、通常BS用FED1メッセージでRFとTSIDを指定する。BS朝日はRF 11,727,480kHz / TSID0x4010。wire周波数はBCD `0x01172748`。
2. 物理TSI0を188バイトserial入力に設定。
3. ioctl0x53の44バイト要求で論理port1を物理ASI0へ割当。word0=1、word10=0。電気設定は設定済み物理0から取得。
4. type12 DEMUXを生成し、TSI入力要求を`[device,0,1,0]`とする。論理0を指定した対照実験は0バイトだった。
5. PID=-1でSYSTEM/TS、PID0ではDATA/PSI_SECTIONを読める。
6. size=0のdescriptorもreader commitして返却する。自分が生成したring/readerだけを解放し、純正所有物に触れない。

入力クロスバーMMR0x61f0はidentity0x10→0x00へ変化。復帰時には保存した設定へ戻す。4K診断REMUXが取れてもHDMIの純正デコーダが戻るとは限らない。

実装: `src/bs2k.rs`、`src/loopback.rs`、`src/xcdrv.rs`、`src/tuner.rs`、`src/gr_i2c.rs`。

## 独立した受信確認

BS朝日の15秒取得は32,932,148バイト、175,171パケット、約17.56Mbps。全188バイト同期、TEI0、非null PIDのCC異常0、完結PSI/SI1,690セクションのCRC不一致0。PATのTSID0x4010とサービス151/152/153、MPEG-2 PID0x140 / AAC PID0x141を確認した。

別TSのEITが見えることだけを、その局を選局して受信できた証拠とはしない。後に6局を個別選局し、映像音声のデコードで確認した。

## 6局設定

BS朝日は内蔵設定。追加5局は`config/bs-additional-main.tsv`。TSIDは16進表記、周波数はkHz。NHK BSは受信波から確認したTSID0x40F1を使う。古い局表の値を流用しない。

| 主サービス | SID | TSID | RF kHz |
| --- | --- | --- | --- |
| BS朝日 | 151 | 4010 | 11727480 |
| BS-TBS | 161 | 4011 | 11727480 |
| BSテレ東 | 171 | 4012 | 11727480 |
| BS日テレ | 141 | 40D0 | 11957640 |
| BSフジ | 181 | 40D1 | 11957640 |
| NHK BS | 101 | 40F1 | 11996000 |

CSはアンテナ受信できる環境で試験していない。型番上のISDB-S対応は、基板配線・ファーム制御までの動作保証ではない。
