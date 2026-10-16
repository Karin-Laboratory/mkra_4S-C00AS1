# BS2K / BS4K統合APIとクライアント設定

受信機のTCP40772はMirakurun互換API、外部EPG proxyの標準TCP40773は番組cache等を補完するAPI。別途Mirakurun本体は不要だが、完全互換ではない。`/api/version`の3.9.0は互換表示、mkra自身は`/api/status`で確認する。

## 起動とクライアント

```sh
mkra serve --remux4k --tune --bs2k --channel-type BS4K --port 40772   --isdbs-channels /tmp/mt/bs-additional-main.tsv   --epg-tap --epg-refresh 5 --epg-dir /tmp/adsb
```

通常BSはBS、既存4KはBS4Kとして広告する。TVTestでは使用するMirakurun対応BonDriverの対応typeを確認し、可能ならBSとBS4Kを対象にする。4Kだけ対応するBonDriverの設定を2Kにも使えるとは保証しない。BSのみ受けるクライアント向けにはproxyのBS表示変換を使い、IDの違いは保持する。

EPGStationは通常`mirakurunPath: http://127.0.0.1:40773/`として外部proxyへ接続する。[新規導入手順](EPGSTATION-INSTALL.ja.md)。本体40772への直結だけでは通常BSの番組cacheや番組ID録画の終了処理は補完されない。

## ID・チャンネル

`ServiceItemId = networkId * 100000 + serviceId`。通常BSのnetwork4とBS4Kのnetwork11を区別する。通常BS番組IDは`ServiceItemId * 100000 + eventId`でnetworkを含む。

| 主サービス | 通常BS API ID | 通常BSキー | BS4K API ID |
| --- | --- | --- | --- |
| NHK | 400101 | BS_TS40F1 | 1100101 |
| 日テレ | 400141 | BS_TS40D0 | 1100141 |
| 朝日 | 400151 | BS_TS4010 | 1100151 |
| TBS | 400161 | BS_TS4011 | 1100161 |
| テレ東 | 400171 | BS_TS4012 | 1100171 |
| フジ | 400181 | BS_TS40D1 | 1100181 |

朝日の151/152/153は内蔵設定。追加5局は`--isdbs-channels`のTSVを使う。`--channels`は既存4K局表の差替え用で、追加ISDB-S局表とは役割が異なる。

### 本家由来の4K系局表

以下は実装の内蔵テーブルであり、全行の現在の放送・受信・CAS・8K再生を保証しない。今回の2K後の4K切替実測はNHK・日テレ・TBSに限る。

| SID | Mirakurun id | 名前 | チャンネルキー | RF (kHz) | TSID |
|---:|---:|---|---|---:|---|
| 101 | 1100101 | NHK BSP4K | BS17_0 | 12,034,360 | 0xB110 |
| 102 | 1100102 | NHK BS8K | BS14_0 | 11,976,820 | 0xB0E0 |
| 141 | 1100141 | BS日テレ 4K | BS7_2 | 11,842,560 | 0xB072 |
| 151 | 1100151 | BS朝日 4K | BS7_0 | 11,842,560 | 0xB070 |
| 161 | 1100161 | BS-TBS 4K | BS17_1 | 12,034,360 | 0xB111 |
| 171 | 1100171 | BSテレ東 4K | BS7_1 | 11,842,560 | 0xB071 |
| 181 | 1100181 | BSフジ 4K | BS17_2 | 12,034,360 | 0xB112 |
| 211 | 1100211 | ショップチャンネル | BS11_0 | 11,919,280 | 0xB0B0 |
| 221 | 1100221 | 4K QVC | BS11_1 | 11,919,280 | 0xB0B2 |
| 231 | 1100231 | （NVRAM に名前なし → `Service 231`） | BS11_2 | 11,919,280 | 0xB2B1 |

## エンドポイントの違い

| メソッド・パス | 本体40772 | proxy40773 |
| --- | --- | --- |
| GET `/api/version` `/api/status` | 互換version・状態 | 本体へ転送 |
| GET `/api/tuners` `/api/tuners/0` | 一基、users、drop等 | typeをBSへ表示変換 |
| GET `/api/channels` `/api/channels/{type}` `/{type}/{channel}` | 登録局と絞込み | BS4K→BS表示、キーから経路解決 |
| GET `/api/channels/{type}/{channel}/services` `…/services/{id}` | 局内サービス | 転送・表示変換 |
| GET `/api/services` `/api/services/{id}` | networkを含む全ID | 同じIDを保持 |
| GET `/api/programs` `/api/programs/{id}` `/api/services/{id}/programs` | 読めた4K SI情報。2K H-EITの直接収集なし | 通常BS/4K cacheを併合 |
| GET/HEAD `/api/channels/{type}/{channel}/stream` `/api/services/{id}/stream` | 188B MPEG-TS、終わりは接続切断まで | GETを転送。proxyのHEAD stream動作は未保証 |
| GET `/api/programs/{id}/stream` | 未実装 | cacheからサービスを解決し、終了時刻+1秒でEOF |
| `/api/events`・設定書換等 | 未実装 | 補完しない |

`/api/status`の本体`epg.storedEvents`は0のplaceholderで、実際の番組数ではない。EPG有無は`/api/programs`やproxyのcacheで確認する。

### クエリ・復号・優先度

- 本体channels: `type` / `channel` / `name`。services: `serviceId` / `networkId` / `type` / `name` / `channel.type`。
- 本体programs: `serviceId` / `networkId`。proxyではさらに`startAt` / `endAt`で時間範囲を絞れる。
- streamの`decode`は受理するが、スクランブル版/復号版を切り替えない。BS4Kは純正復号ES、BS2KはmkraのライブCAS/MULTI2を使う。外部B25による二重復号を必須としない。
- `priority` / `X-Mirakurun-Priority`は受理・usersへ表示するが、既存受信の強制横取りを行わない。
- サービスstreamも多重全体を共有する。未知PIDの暗号化が残る場合がある。

### ステータスと選局の注意

| コード/理由 | 意味・確認 |
| --- | --- |
| 200 | 本体streamは最初のデータを得てから返す。AV成功は別途decodeで確認 |
| 404 | 未登録の局・サービス・番組。全Mirakurun APIの存在を仮定しない |
| 503 `tuner busy with …` | 別TS使用中、または解放後linger中。録画を止めて回避しない |
| 503 `too many stream clients` | client上限、既定4。不要な自分の視聴接続を閉じる |
| 503 source関連 | 待機、版不一致、受信/CAS/ES/cleanup等。本文と同時刻ログを確認 |
| proxy 502 | backend接続/idle timeoutなど。本体の状態も確認 |
| proxy 410 | cache番組の配信時刻が範囲外 |

統合モードの選局完了は取得済みADSB sessionと新規ES書込みで判定する。MPT/MH-SDTは古いまま残るため、それだけでは確認しない。約4秒という古い目安を保証値にせず、再試行やCAS待ちを見込む。成功してもHDMI復帰を意味しない。

## 出力形式

BS4Kは192B TTSのtimestampを除去した188B TS。代表PIDはPCR0x1001、HEVC0x1011、AAC-LATM0x1100。通常BSは元の188B TSを保ち、PAT/PMT/ECMを追跡し、既知の暗号化AVを復号する。通常BSのPID/codecを4Kの値へ固定してはいけない。

観測手順は[トラブル対応](TROUBLESHOOTING.ja.md)、測定の範囲は[検証表](VALIDATION-MATRIX.ja.md)。古い説明は[履歴版API](history/BASELINE-API.md)として残す。
