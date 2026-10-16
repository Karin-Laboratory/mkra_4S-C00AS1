> 本家由来のBS4K中心の資料です。統合版の起動・制限は[README](../README.md)を優先してください。

# mkra — SHARP 4S-C00AS1 を Mirakurun 互換の BS4K チューナーにする

SHARP の 4K チューナー **4S-C00AS1** の上で直接動く、単一の静的バイナリです。
本体が受信・復号している **BS4K（新4K衛星放送）を MPEG-TS として取り出し**、
Mirakurun 互換の HTTP API で配信します。TVTest や EPGStation からそのまま使えます。

```
TVTest / EPGStation ──HTTP──▶ mkra (4S-C00AS1 本体 :40772) ──▶ MPEG-TS (HEVC + AAC-LATM)
```

録画機能は使いません。本体が視聴のために組んでいる MMT デマルチプレクサの出力に、
**もう一つ MPEG-TS 再構成器をぶら下げているだけ**です。仕組みは
[docs/HOW_IT_WORKS.md](HOW_IT_WORKS.md) を読んでください。

---

## できること / できないこと

**先にここを読んでください。** 期待して機材を揃えると外します。

| | |
|---|---|
| ✅ **BS4K（新4K衛星放送）の MPEG-TS 取り出し** | HEVC Main10 / 3840×2160 / BT.2020 HLG + AAC-LATM、約 29 Mbps |
| ✅ **平文で出る** | 復号は本体の ACAS が済ませています。mkra は復号も鍵の操作も**していません** |
| ✅ Mirakurun 互換 API | TVTest / BonDriver_Mirakurun / EPGStation から利用可 |
| ✅ EPG（番組表） | 本体の MMT-SI キャッシュから 300 番組級。日本語タイトル付き |
| ✅ クライアントからの選局 | 本体自身の選局要求を送ります。約 4 秒で切り替わります |
| ❌ **BS/CS 2K（従来の BS・CS110）** | **出ません。**復調まで成功しますが TS 出口が成立しません |
| ❌ **地上デジタル** | **出ません。**2K と同じ出口を通るため同じ理由です |
| ❌ **同時に複数チャンネル** | チューナーが物理的に 1 個です。**同時に扱えるのは 1 チャンネル** |
| ❌ 裏録 | 上と同じ。本体が映しているチャンネルがそのまま配信対象になります |

**重要**: mkra が選局すると、**テレビの画面もそのチャンネルに変わります。**
誰かが視聴している最中に録画を始めると、その人の画面が切り替わります。

BS/CS 2K と地上デジタルがなぜ出ないのか、再開に何が必要かは
[docs/LIMITATIONS.md](LIMITATIONS.md) にあります。ソフトウェアだけでは進みません。

---

## 必要なもの

- **SHARP 4S-C00AS1** と、BS4K が映る状態のアンテナ
- **本体への root telnet**。この repo では**すでにある前提**です（[docs/SETUP.md](SETUP.md) §0）
- 母艦（macOS / Linux）と Python 3。**Rust は不要**です（Releases にビルド済みバイナリがあります）

---

## 導入

```sh
# 1. Releases から mkra をダウンロードする（あるいは ./build-device.sh でビルドする）
# 2. 本体の電源を入れて LED を緑にする
# 3. 配備して、実際に 4K が取り出せることを確認する
python3 tools/deploy.py --host 198.51.100.10 --binary ./mkra --smoke

# 4. 問題なければサーバとして起動する
python3 tools/deploy.py --host 198.51.100.10 --binary ./mkra --serve
```

`--smoke` は 10 秒だけ実際に 4K を取り出し、母艦へ持ち帰って
188 バイト同期率・CC エラー・`ffprobe` によるデコードまで確認します。
**「ioctl が成功した」は成功の証拠になりません**（この機種で実際に何度も騙されました）ので、
必ず絵が出るところまで確認してください。

導入の詳細は [docs/SETUP.md](SETUP.md)、
クライアント設定と API は [docs/API.md](API.md) にあります。

---

## ソースからビルドする

```sh
rustup target add armv7-unknown-linux-musleabihf
./build-device.sh          # 実機用: armv7 musl、完全静的、約 840 KB
cargo test                 # 197 テスト。実機不要
```

依存クレートはゼロ、Docker もクロス gcc も sysroot も要りません。
Releases のバイナリはこの repo の GitHub Actions が同じコマンドで作っています。

---

## 安全性について

mkra は次のことを**構造的に行いません**。

- eMMC / MTD / NAND / OTP / SPI フラッシュへの書き込み — 永続化の仕組みそのものがありません
- 鍵領域（`/dev/shm`）への読み書き
- 純正プロセスの kill・シグナル送信
- 本体が作ったバッファ・デバイスの破棄 — 触るのは自分で作ったものだけです

本体に置くファイルは `/tmp/mt/`（tmpfs）だけで、**電源を切れば完全に消えます**。

それでも**壊しうる操作はあります**。特にライブ ES リングへの接続を止めたまま放置すると、
本体のデコーダが枯渇して「受信できません」表示に陥ります。mkra はこれを検出して
自動的に降りますが（`--es-level-abort`、既定 48）、**この安全装置を無効にしないでください**。
詳細と、フォークする人向けの地雷リストは [docs/ABI.md](ABI.md) §5 にあります。

---

## ドキュメント

| | |
|---|---|
| [docs/SETUP.md](SETUP.md) | 導入手順。telnet がある状態から TVTest 視聴まで |
| [docs/API.md](API.md) | Mirakurun 互換 HTTP API とクライアント設定 |
| [docs/HOW_IT_WORKS.md](HOW_IT_WORKS.md) | 解析結果。何をどう取り出しているか（1ページ） |
| [docs/LIMITATIONS.md](LIMITATIONS.md) | 出ないもの、その理由、凍結中のコマンド一覧 |
| [docs/ABI.md](ABI.md) | 実装詳細と落とし穴。**フォークする人以外は読まなくて構いません** |

---

## 免責

- SHARP 非公式です。SHARP およびその関連会社とは一切関係がありません。
- **自己責任で使ってください。**本体の保証は失われる可能性があります。
- 取り出したストリームの取り扱いは、各自の責任と各国・各地域の法令に従ってください。

## ライセンス

MIT License — [LICENSE](../LICENSE) を参照してください。
