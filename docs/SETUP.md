# 導入手順

telnet が使える状態から、TVTest で BS4K が映るところまで。所要 15 分ほどです。

---

## 0. 前提: 本体への root telnet

**この repo は telnet がすでに使える状態から始まります。**

4S-C00AS1 は工場出荷状態では telnet を開けていません。有効化するには
SPI フラッシュ上の U-Boot 環境変数（`bootargs`）を書き換えて、`init=` に
`busybox telnetd -p 2323 -l /bin/sh` を足す必要があります。**方法は個体や道具立てで
変わるため、この手順書では扱いません。**各自の方法で用意してください。

参考として、CH341A で吸ったダンプにパッチを当てるスクリプトを
[`tools/contrib/patch_bootargs.py`](../tools/contrib/patch_bootargs.py) に置いてあります。
**これを使う場合も、以下の2点は方法によらず共通の地雷です。**

1. **`init=` の `-c` スクリプトトークンにドット（`.`）を入れてはいけない。**
   Linux の `init/main.c: unknown_bootoption()` が「ドットを含み `=` を持たないトークン」を
   module パラメータとみなして**丸ごと捨てます**。`-c` の中身が消えて `sh -c secureboot` になり、
   PID 1 が死んで panic します。`/mmc/boot.sh` と書いて実際にこれで起動しなくなりました。
2. **`sleep` を先に置き、パイプを使わない。** `sleep` してから動く background job は
   生き残りますが、起動直後から fork し続けるループ（`printf ... | sh` 形式）は
   初期ブートの不安定区間で消えます。

**書き込む前に必ずフラッシュを吸ってバックアップしてください。**それが唯一の復旧手段です。
他人のダンプを焼いてはいけません（`ethaddr` にその個体の MAC アドレスが入っています）。

telnet の確認:

```sh
telnet 192.168.0.40 2323
# パスワードなしで root の /bin/sh が出る
cat /proc/cmdline        # telnetd 入りの init= が見える
ls /dev/vixs/            # xcodedrv があること
```

---

## 1. ★ 本体の電源を入れて LED を緑にする

**これを飛ばすと必ず失敗します。**

待機（赤 LED）では **A/V パイプラインも MMT の ES リングも存在しません。**
mkra は取り出す対象そのものが無いので、必ず 0 バイトになります。しかも
**赤のまま放置すると約 23 分で電源が落ちます。**

リモコンで電源を入れ、**BS4K のチャンネルを表示している状態**にしてください。

---

## 2. mkra を入手する

### Releases から（推奨。Rust 不要）

[Releases](../../releases) から `mkra` をダウンロードします。
armv7 musl の完全静的バイナリなので、本体側に必要なものはありません。

### ソースからビルドする

```sh
rustup target add armv7-unknown-linux-musleabihf
./build-device.sh
# -> target/armv7-unknown-linux-musleabihf/release/mkra
```

Docker もクロス gcc も sysroot も不要です（依存クレート 0 の純 Rust）。
検証したければ `cargo test`（197 テスト、実機不要）も走ります。

---

## 3. 配備して、動くことを確認する

```sh
python3 tools/deploy.py --host 192.168.0.40 --binary ./mkra --smoke
```

これが行うこと:

1. 本体の状態を確認（待機中なら、その場で警告して止まります）
2. 以前に自分が起動した mkra があれば停止（純正プロセスには触りません）
3. `/tmp/mt/mkra` へ base64 で転送し、**md5 を本体側で検証**
4. `mkra graph --sniff` — ドライバのライブグラフを読むだけ（read-only）
5. `mkra remux4k --discover-only` — 本体のライブ ES リングを報告するだけ（read-only）
6. `mkra remux4k --seconds 10` — 実際に 10 秒キャプチャ
7. HTTP で母艦へ回収し、**188 バイト同期率・CC エラー・`ffprobe` を確認**

期待する出力:

```
  pulled 30162432 of 30162432 bytes -> ./smoke.ts
  160438 packets, 100.00% aligned, 5 PIDs, 0 CC errors
  ffprobe: codec_name=hevc width=3840 height=2160 codec_name=aac_latm
```

**`ffprobe` まで通って初めて成功です。**この機種では「ioctl が成功した」
「バイト数が増えた」「TSI が lock して CRC が進んだ」のいずれも、
有効な MPEG-TS の証拠になりませんでした。実際に絵が出るかどうかだけが証拠です。

`/tmp/mt` は tmpfs なので、**再起動すると消えます**。その場合は同じコマンドをもう一度実行してください。

---

## 4. サーバとして起動する

```sh
python3 tools/deploy.py --host 192.168.0.40 --binary ./mkra --serve
```

実際に本体で起動されるのはこれです:

```sh
/tmp/mt/mkra serve --remux4k --tune --channel-type BS4K --port 40772 \
    --epg-dir /tmp/adsb > /tmp/mt/mkra.log 2>&1 &
```

| オプション | 意味 |
|---|---|
| `--remux4k` | ストリーム要求のたびにタップを組み、切断で完全破棄する。**アイドル中は本体に一切触りません** |
| `--tune` | 本体が映していないチャンネルを要求されたら、**本体に選局させる**。付けないと他 9 局は 503 になります |
| `--channel-type BS4K` | Mirakurun-BS4K 系クライアント向け。素の BonDriver_Mirakurun なら `--channel-type BS`（`deploy.py --channel-type BS`） |
| `--epg-dir /tmp/adsb` | 本体の MMT-SI から番組表を作る |
| `--ring <bytes>` | クライアント毎バッファ。既定 2 MiB（4K で約 0.55 秒）。回線が不安定なら増やす |
| `--es-level-abort <n>` | **変更しないでください。**既定 48。§6 参照 |

停止:

```sh
python3 tools/deploy.py --host 192.168.0.40 --stop
```

---

## 5. クライアントを繋ぐ

設定値は [docs/API.md](API.md) にまとめてあります。要点だけ:

- **TVTest + BonDriver_LinuxMirakurunBS4K**: `SERVER_HOST` に本体 IP、`SERVER_PORT=40772`、
  `CHANNEL_TYPES="BS4K"`、`DECODE_B25=0`
- **EPGStation**: `mirakurunPath` に `http://192.168.0.40:40772`。
  mkra 自身が Mirakurun 互換サーバなので、**別に Mirakurun を立てる必要はありません**

動作確認:

```sh
curl -s http://192.168.0.40:40772/api/version
curl -s http://192.168.0.40:40772/api/channels
curl -sm 15 http://192.168.0.40:40772/api/services/1100141/stream -o test.ts && ffprobe test.ts
```

---

## 6. 運用上の注意

すべて実際に起きたことに基づいています。

1. **LED が緑でないと何も取れません。** 待機中は ES リングが存在しません。
2. **選局すると本体の画面も変わります。** 視聴中の人がいるときに録画を始めないこと。
3. **`--es-level-abort` を無効化・緩和しないでください。**
   タップが消費を止めるとライブ ES リングが滞留し、本体のデコーダが枯渇し
   （`HDMI UFLOW`）、本体が ES を張り替えようとしても**こちらの接続が残っているため失敗**し、
   サービスが PAUSE して「受信できません」になります。閾値内で降りれば影響は一瞬で済みます。
4. **画面が「受信できません」になったら、まずリモコンで選局し直すか、待機→電源オン。**
   再起動は最後の手段で構いません。RF は無関係に生きていることが多いです。
5. **有線推奨。** 4K は 29 Mbps 出ます。
6. **発熱は下がりません。** 復号を止めても CPU idle 88.8%、体感の発熱も変わりませんでした。
   熱源は RF フロントエンド・電源・HDMI 出力側とみられます。完全に落とすならリモコンで待機にしてください。
7. **凍結中のコマンドを実行しないでください**（`gr` / `demux` / `tsrec` / `dmd-*` / `cpld` / `isdbs-*`）。
   ストリームは出ないうえ、一部は I2C を叩くので実行後に再起動が要ります。
   [docs/LIMITATIONS.md](LIMITATIONS.md) を参照してください。

---

## 7. トラブルシュート

| 症状 | 原因と対処 |
|---|---|
| `--smoke` が「No live elementary streams」 | 待機中です。リモコンで電源を入れて 4K チャンネルを表示させてください |
| ストリームが 503 になる | `--tune` なしで起動しています。本体が映していないチャンネルは配信されません |
| 出力が 0 バイト | `/tmp/mt/mkra.log` に中断理由が出ます。`--with-tsinfo` を付けていないか確認してください |
| 画面が「受信できません」になった | §6-3。リモコンで選局し直すか、待機→電源オン |
| アップロードした mkra が壊れている | 実行中のバイナリは上書きできません（ETXTBSY）。`deploy.py` は自動で停止しますが、手動で入れた場合は先に止めてください |
| クライアント側でドロップが出る | `--ring` を増やす、有線にする。ドロップ数は `/api/tuners` の `streamInfo.drop` に出ます |
| 再起動したら消えた | `/tmp/mt` は tmpfs です。`deploy.py` をもう一度実行してください |
| telnet が繋がらない | 電源投入から 20 秒以上待ってください（`bootargs` の `sleep 15` があります） |
