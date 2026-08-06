# API とクライアント設定

mkra は本体の `:40772` で **Mirakurun 互換の HTTP API** を喋ります。
別途 Mirakurun を立てる必要はありません。

`/api/version` は `{"current":"3.9.0","latest":"3.9.0"}` を返します。

---

## 1. クライアント設定

### TVTest + BonDriver_LinuxMirakurunBS4K（4K 対応版）

```ini
[GLOBAL]
SERVER_HOST="192.168.0.40"    ; 本体の IP
SERVER_PORT=40772
CHANNEL_TYPES="BS4K"
DECODE_B25=0                  ; mkra は復号しません。?decode= は受理して無視します
PRIORITY=0
SERVICE_SPLIT=0               ; 0=/api/channels, 1=/api/services。どちらも実装済み
```

### 素の BonDriver_Mirakurun

mkra 側を `--channel-type BS` で起動し（`deploy.py --channel-type BS`）、
`CHANNEL_TYPES` 行を書きません。配信されるのは同じ平文 MPEG-TS で、
`ChannelType` はクライアントへのラベル以上の意味を持ちません。

### EPGStation

`config.yml` の `mirakurunPath` に `http://192.168.0.40:40772` を設定します。

### MacTVTest

Mirakurun 基点 URL に `http://192.168.0.40:40772/` を設定します。
`/api/services`・`/api/services/{id}/stream`・`/api/programs`・`logo` のみを使う実装です。

---

## 2. チャンネルとサービス ID

Mirakurun の `ServiceItemId` は `network_id`（11）＋ `service_id` を 5 桁ゼロ詰めした
10 進数です。BS4K は 10 波です。

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

チャンネル表は本体の NVRAM から復元したものをバイナリに埋め込んであります。
差し替えたい場合は `serve --channels <TSV>` で外部ファイルを渡せます。

---

## 3. エンドポイント

| メソッド | パス | 内容 |
|---|---|---|
| GET | `/api/version` | バージョン |
| GET | `/api/status` | サーバ状態 |
| GET | `/api/tuners` `/api/tuners/0` | チューナー一覧。使用中かどうか、クライアント別のドロップ数 |
| GET | `/api/channels` | チャンネル一覧 |
| GET | `/api/channels/{type}` `/{type}/{channel}` | 絞り込み |
| GET | `/api/channels/{type}/{channel}/services` `/services/{id}` | そのチャンネルのサービス |
| GET | `/api/services` `/api/services/{id}` | サービス一覧・単体 |
| GET | `/api/programs` `/api/programs/{id}` | 番組表（`--epg-dir` 指定時） |
| GET | `/api/services/{id}/programs` | そのサービスの番組表 |
| GET, HEAD | `/api/channels/{type}/{channel}/stream` | **MPEG-TS**（`video/MP2T`） |
| GET, HEAD | `/api/services/{id}/stream` | **MPEG-TS**（`video/MP2T`） |

### クエリパラメータ

- `/api/channels`: `type` / `channel` / `name`
- `/api/services`: `serviceId` / `networkId` / `type` / `name` / `channel.type`
- `/api/programs`: `serviceId`
- ストリーム: `decode`（**受理して無視します**。復号は本体側で済んでいます）、
  `priority`、`X-Mirakurun-Priority` ヘッダ

### ステータスコード

| コード | 意味 |
|---|---|
| 200 | 正常 |
| 404 | 該当するチャンネル・サービス・番組がない |
| 503 `tuner busy with <ch>` | **別のチャンネルを配信中。**チューナーは 1 個しかありません |
| 503 `too many stream clients` | `--max-clients`（既定 4）を超えた |
| 503 `no TS source available` | 取り出し口を開けなかった。本体が待機中の可能性 |

**ストリームは最初の 1 バイトが届いてから 200 を返します。**
黙って 200 を返して永遠に何も来ない、という状態を作らないためです
（それをやると `/api/tuners` が使用中のまま固まり、他の全チャンネルが 503 になります）。

---

## 4. 選局の扱い

`--tune` を付けて起動した場合、本体が今映していないサービスを要求されると、
**mkra が本体自身の選局要求を送ります**。約 4 秒で切り替わり、
**テレビの画面もそのチャンネルに変わります。**

`--tune` なしの場合、本体が映しているサービス以外は 503 になり、理由が本文に入ります。

```
channel BS17_0 is service 101, but the box is showing 141 (TLV stream 0xb072).
Select it on the box first — mkra does not tune yet.
```

誤ったサービス名でストリームを渡すとクライアントのサービス表を汚すため、黙って流しません。

---

## 5. 出力される TS

- 188 バイト MPEG-TS（本体からは 192 バイト TTS で出るので、mkra が剥がしています）
- PAT / PMT は再構成器が生成し、**実サービスを名乗ります**（例: TSID=45170、program=141）
- PMT: PCR `0x1001`、HEVC `0x1011`、AAC-LATM `0x1100`
- 実測: 60 秒連続 216,976,064 バイト（28.9 Mbps）、CC エラー 0

```
ffprobe: Program 141
  Stream #0:0 hevc (Main 10), yuv420p10le(tv, bt2020nc/bt2020/arib-std-b67), 3840x2160
  Stream #0:1 aac_latm
```

---

## 6. 実装していないもの

- `/api/programs` は `--epg-dir` を付けないと空配列を返します
- デスクランブル関連の API（そもそも復号は本体側で完了しています）
- チューナーの排他制御は「1 チャンネルのみ」の単純な形です
- 追いつかないクライアントには**古いバイトを捨てて**現在に追従させます。
  捨てた数は `/api/tuners` の `streamInfo.drop` に出ます
