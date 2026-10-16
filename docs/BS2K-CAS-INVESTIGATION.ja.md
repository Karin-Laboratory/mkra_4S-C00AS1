# 内蔵CASとBS2Kライブ復号

## 受信と復号を分ける

最初の188バイトTSではPAT/PMTは正常だったが映像・音声は暗号化されていた。純正type16 DESCRAMBLERの生成だけではPID・鍵スロット・接続が揃わず、映像にならなかった。現在は既存card daemonに通常BS用ECMを処理してもらい、CPUでMULTI2復号する。

## 採用した通信

- 稼働中の純正card daemonと、既存ICDのSysV queue/shared memoryを利用する。
- queue番号は`/tmp/bdh_out_msgh.lst`から毎回解決し、boot ID・受信task・実行ファイルハッシュ・queue空状態を照合する。
- `/tmp/card.lock`のflockとプロセス内排他で直列化。カードのinit/resetや新しい認証処理を実行しない。
- 従来方式のINS34/P2=0では応答が得られても期待した復号条件にならなかった。純正のcard type2分岐に合わせ、INS34/P2=2でECM、INS30/P2=2で必要なパラメータを取得する。
- パラメータ・control wordはRAM内で扱い、鍵・カード番号・生CAS応答をログやファイルに出力しない。
- タイムアウトで取引の完了が不明になった場合、通信をpoisonして自動再送を禁止する。遅延応答の取り違えやshared memory上書きを避ける。

実装は`src/cas.rs`。公開ソースにはコマンドの形式と処理だけを含め、実カードの値は含めない。ハッシュ・固定ABIは検証した純正版専用のガードであり、純正バイナリ本体は不要・非同梱。

## ライブ処理

`src/descramble.rs`がPAT→PMT→CA descriptor→ECM PIDの関係を追跡。分割sectionを再組立てしCRC・continuityを確認する。ECM更新に合わせて必要な鍵を更新し、先行する暗号化パケットを有限量だけ待機する。

同一ECMを毎回カードへ送らず、更新時のみ再処理する。未知PIDへ推測で鍵を適用したり、復号できていないのにscrambling controlだけを消したりしない。

`tools/multi2.rs`はscalar実装。参照先はstz2012/libarib25の固定コミットで、[NOTICE](../tools/third-party/libarib25-NOTICE.md)とApache-2.0を保持する。参照fixtureは単調な合成入力・合成鍵から生成したテストベクトルであり、放送鍵ではない。

## 検証と限界

保存TSの復号でMPEG-2 1440×1080 / AACを実デコードした後、ライブHTTPへ統合。通常BS6局の主映像音声、BS2K120秒連続ライブ復号を確認した。

BS日テレのPID0x248はPMTから対応を確定できず、暗号化を残す。主映像音声成功を全PIDの復号成功とはしない。CAS応答成功だけでも映像成功とはしない。

長時間の鍵更新・全サービス・有料契約・CSは未検証。HDMI復帰問題はライブ復号とは別に残る。
