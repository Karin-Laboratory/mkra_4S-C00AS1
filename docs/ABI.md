# 実装詳細と落とし穴 — XCode6 ABI / 選局 IPC

**フォークする人、他機種へ移植する人以外は読む必要がありません。**
使うだけなら [SETUP.md](SETUP.md)、仕組みの概要は [HOW_IT_WORKS.md](HOW_IT_WORKS.md) で足ります。

外部プログラムからこの箱の 4K 放送を MPEG-TS として取り出し、選局まで行うために
必要な全手順です。参照実装はこのリポジトリの `src/`（Rust、依存クレート無し）。

証拠タグ: `[S]` 静的解析（バイナリのアドレス付き） / `[M]` 実機実測 / `[D]` 設計判断

> **§5 と §9 だけは、使うだけの人にも関係します。**
> ここに書いてあることを守らないと、本体が「受信できません」表示から復帰しなくなります。

---

## 0. 要約

やることは 2 つだけである。

1. **製品が動かしている MMT デマルチプレクサの ES リングに、自前の REMUX を繋ぐ。**
   REMUX は XCode6 のハードウェアで、MMT 由来の ES から MPEG-TS を組み立てる。
   製品は録画時にしかこれを作らないが、同じものをもう一つ作れば録画なしで TS が得られる。
2. **選局は製品自身のチャンネル変更メッセージを送る。**
   RF だけ動かしても製品の MMT フィルタが追随しないため、必ず製品に選局させる。

どちらも製品を止めない。実測で 60 秒連続 28.9 Mbps・CC エラー 0、
`ffprobe` で `hevc Main10 / 3840x2160 / BT.2020 / HLG + aac_latm` を確認済み。[M]

```
RF ─ SC1501A(ISDB-S3) ─ TSI0 ─ TLV(0x24) ─ MMT_DEMUX(0x25) ┬─ video ES (20 MiB) ─ VIDEO_DECODER
                                                            ├─ audio ES (2 MiB)  ─ AUDIO_DECODER
                                                            └─ MMT-SI section rings ─ abs
                                                                 ▲
                                          自前 REMUX(0x0E) ──────┘ 入力を2本もらう
                                                 └→ TSOUT(LINEAR_RING) → host read → 192B TTS → 188B TS
```

---

## 1. 環境

| 項目 | 値 |
|---|---|
| SoC | ViXS XCode6（`Machine: Xcode6`、ARM Cortex-A9 ×2、Linux 3.10.24） |
| ドライバ | `/dev/vixs/xcodedrv`（`xcdrv.ko` 3.0.0.0.72.00）、ARC700 ファーム `xc6800_arc700.bin` 3.48.0.0.72.00 |
| RF | Sony CXD2868ER（`/dev/i2c-0`、7-bit `0x60`） |
| 復調 | SC1501A（`/dev/i2c-0`、page 7-bit `0x10..0x18`）、ISDB-S3 |
| 製品プロセス | `tuner` / `adsb`（MMT・録画）/ `abs`（デコード graph・UI 連携） |
| メモリ | 666 MB。XCode6 の host アパーチャは物理 `0x1F000000..0x29000000`（160 MB） |
| 作業領域 | `/tmp/mt` のみ。tmpfs で再起動により消える |

**書いてはいけないもの**: eMMC、MTD、NAND、OTP、`/dev/spidev0.0`（ブート SPI）。
CPLD は非搭載（`[TUNER]:Arib[1]Sky[0]cpld[0]silicon[0]`）なので selector 探索は無意味。[M]

---

## 2. ioctl ABI

`fd = open("/dev/vixs/xcodedrv", O_RDWR)`。呼び出しは `ioctl(fd, cmd, &req)` で、
**`req` は入出力兼用のワード配列**である。`cmd` の下位 16 bit だけがハンドラを選び、
上位 16 bit はサブセレクタ（`0x2A` `0x2B` `0x35` `0x36` のみが読む）。[S]

### 2.1 使う ioctl 一覧

| cmd | 意味 | req 長 | 備考 |
|---|---|---:|---|
| `0x28` | mini device 生成 | 24 B | `[me_type, max_in, max_out, max_ctl, handle(OUT), 0]` |
| `0x29` | mini device 破棄 | 4 B | 値渡し（handle） |
| `0x32` | buffer 生成 | 36 B | §2.2 |
| `0x34` | buffer 破棄 | 4 B | 値渡し |
| `0x37` | reader 生成 | 8 B | `[buffer, reader(OUT)]` |
| `0x38` | reader 破棄 | 4 B | **先に `0x3F` を呼ぶこと**（§5.3） |
| `0x3B` | host read | 40 B | `[reader, wait, timeout, addr, size, addr1, size1, desc, …]` OUT 付き |
| `0x3C` | commit read | 16 B | `[reader, addr, size, desc]` |
| `0x3F` | cancel get | 8 B | `[reader, 1]`。`libvixs:xcode_buffer_cancel_get_event @0x86c34` [S] |
| `0x48` | pipeline 時刻取得 | 12 B | `[pipeline, lo(OUT), hi(OUT)]` |
| `0x7A` | 物理→ユーザ空間 map | 12 B | `[size, phys, vaddr(OUT)]` |
| `0x7B` | unmap | 8 B | `[size, vaddr]`。**引数順を間違えるとプロセスの address space が飛ぶ** |
| `0x00010035` | buffer set_ready | 4 B | 値渡し |
| `0x00030035` | buffer force_reset | 8 B | `[buffer, 1]` |
| `0x00010036` | buffer GET_STATUS | 48 B | §2.3 |
| `0x00020036` | buffer GET_READER | `8+4n` B | **`access_ok` 無しの生 memcpy**。バッファを小さく渡すと oops |
| `0x0001002B` | device get_status | 8 B | `[device, status(OUT)]` |
| `0x0001002A` | device run | 12 B | `[device, wait, timeout_ms]` |
| `0x0003002A` | device stop | 12 B | 同上 |
| `0x0007002A` | connect_input | 16 B | `[device, bind=0, buffer, connect(OUT)]` |
| `0x0008002A` | connect_output | 16 B | 同上 |
| `0x0009002A` | disconnect | 16 B | `[device, connect, wait, timeout_ms]` |
| `0x000C002A` | set_pipeline | 8 B | `[device, pipeline]` |

`0x2A` のサブセレクタは **`device_class * 100 + index`**（`me_type` ではない）。[S]
REMUX は class 11。

| コード | 操作 | req 長 | 引数 |
|---|---|---:|---|
| `0x044C002A` | conf_remux | 28 B | `[dev, me_format, bf_feature, bitrate, stream_id, pmt_interval, ext]` |
| `0x044D002A` | add_program | 44 B | `[dev, prog, pcr_pid, pmt_pid, pcr_interval, flag, ats_lo, ats_hi, 0,0,0]` |
| `0x044E002A` | remove_program | 8 B | `[dev, prog]` |
| `0x044F002A` | add_video | 28 B | `[dev, connect, prog, pid, stream_id, ext_stream_type, pts_interval]` |
| `0x0450002A` | add_audio | 28 B | 同上 |
| `0x0452002A` | add_psi | 20 B | `[dev, connect, pid, psi_type, interval]` |
| `0x0454002A` | remove_stream | 12 B | `[dev, connect, flag]` |
| `0x0456002A` | config_video | 36 B | `[dev, connect, 0, max_bps, 0,0,0,0, target_bps]` |
| `0x0459002A` | set_copy_permission | 8 B | `[dev, ccc]` |

各コードは `libvixs.so` の該当ラッパー内の `movw r1, #imm` を実測したもの。[S]

### 2.2 `0x32` buffer 生成（36 B）

```
[0] me_type        1=CHUNK 3=RING 4=LINEAR_RING
[1] me_media_type  1=SYSTEM 2=DATA 3=VIDEO_ES 6=AUDIO_ES 8=AUDIO_PCM
[2] m_media_subtype
[3] m_max_reader   ★ 生成時固定の消費者クォータ。あとから増やせない
[4] m_alignment
[5] m_size         RING はユニット数、LINEAR_RING はバイト数
[6] m_unit_size
[7] m_bf_flags     bit0/bit1。★ bit1 が立っていると host アパーチャ内に確保される
[8] m_buffer(OUT)
```

`m_max_reader` は引数 5 番目である（4 番目ではない）。ここを間違えると
クォータを `m_media_subtype` に書き込むことになる。[S]

`m_bf_flags` bit1 の効果は実測で確認できる。製品の video ES は flags=1 で
`0x1e02af00`（アパーチャ外＝host から読めない）、audio ES と TSOUT は flags=3 で
`0x28xxxxxx`（アパーチャ内＝`0x7A` で map できる）。[M]

### 2.3 `0x00010036` GET_STATUS（48 B）

```
[0] m_buffer  [1] me_type   [2] m_mem_size  [3] m_low_threshold
[4] m_high_th [5] m_level   [6] me_media_type [7] ms_media_subtype
[8] m_writer  [9] m_num_readers  [10],[11] 未文書（ES では累積カウンタが入る）
```

**クォータ（cap）は返らない。**空きがあるかは繋いでみるまで分からない。
`-1035 XCE_TOO_MANY` が「枠が満杯」である。[M]

### 2.4 主なエラーコード

`-1003` NO_SUPPORT / `-1021` 範囲外 / `-1022` 無効ハンドル / `-1023` 無効パラメータ /
`-1034` reader 使用中（cancel していない）/ `-1035` クォータ超過 / `-1037` 状態不正 /
`-1038` タイムアウト / `-1039` キャンセル / `-1040` 使用中 / `-1042` 存在しない

---

## 3. デバイスとバッファの発見（すべて read-only）

ハンドルを総当たりしてはいけない。ドライバは自分が作った全オブジェクトの表を持つ。[S]

```
gps_XCode（/proc/kallsyms）→ ctx
  ctx+0x4DBD8 count / +0x4DBDC ptr   devices   stride 12  cap 100
  ctx+0x4DBE0 / +0x4DBE4            buffers   stride 12  cap 200
  ctx+0x4DBE8 / +0x4DBEC            pipelines stride  4  cap 32
  ctx+0x4DBF0 / +0x4DBF4            ports     stride 20  cap 300
```

前提: `kptr_restrict=0`、`CONFIG_DEVKMEM=y`（この機種は両方満たす）。
表は lowmem にあるので `/dev/kmem` でそのまま読める。[M]

- device 行 = `[handle, me_type, pipeline]`
- port 行 = `[port, type, a, b, extra]`。`type==1` は「device a が buffer b に書く」、
  `type==0` は「device b が buffer a を読む」、`type==0x0b` は host 接続。

**4K のライブ ES を特定する条件**（`me_type 0x25` = MMT_DEMUX が書くもの）:

| 対象 | 判定 |
|---|---|
| video ES | `me_media_type==3 && subtype==0 && m_mem_size==0x1400000`（20 MiB） |
| audio ES | `me_media_type==6 && subtype==0 && m_mem_size==0x200000`（2 MiB） |
| pipeline | MMT_DEMUX の device 行 3 列目 |

これらは `adsb` の FDC が作る（`videoSetFilter @0x5c178` / `audioSetFilter @0x4a9e4`）。
生成時の `m_max_reader` は **video 4 / audio 5** で、通常は video 1 / audio 2 しか
使っていない。**空きがあるので外部の消費者を足せる。**[S][M]

---

## 4. 自前 REMUX を組む手順

製品 `adsb:remux_xcif_open_remux @0x421f8` ほかの録画用レシピから、録画部分を除いたもの。
順序も製品どおりにする。

```c
/* 1. REMUX 本体 */
ioctl(0x28, [0x0E, 13, 2, 1, &remux, 0]);              /* adsb:0x42260 */

/* 2. 全体設定。m_stream_id が生成される PAT の TSID になる */
ioctl(0x044C002A, [remux, 3, 0x10, 0, TSID, 100, 0]);  /* adsb:0x42294 */

/* 3. 出力リング。flags=3 で host アパーチャに確保させる */
ioctl(0x32, [4, 1, 0, 1, 0x1000, 0x468000, 1, 3, &tsout]);   /* adsb:0x422e2 */

/* 4. 出力接続（TSOUT だけ。TSINFO は繋がない → §5.1） */
ioctl(0x0008002A, [remux, 0, tsout, &conn_out]);

/* 5. 製品のライブ ES を入力にもらう。ここが -1035 ならクォータ満杯 */
ioctl(0x0007002A, [remux, 0, video_es, &conn_v]);
ioctl(0x0007002A, [remux, 0, audio_es, &conn_a]);

/* 6. 基準時刻。製品は選局時の参照時刻を使うが pipeline 時刻で足りる */
ioctl(0x48, [pipeline, &lo, &hi]);

/* 7. プログラムとストリームの宣言 */
ioctl(0x044D002A, [remux, SID, 0x1001, 0x0100, 100, 1, lo, hi, 0,0,0]);
ioctl(0x0456002A, [remux, conn_v, 0, 0x02A4C200, 0,0,0,0, 0x029F6300]);
ioctl(0x044F002A, [remux, conn_v, SID, 0x1011, 0xE0, 0x24, 0]);   /* HEVC */
ioctl(0x0450002A, [remux, conn_a, SID, 0x1100, 0xC0, 0x11, 0]);   /* AAC-LATM */

/* 8. 起動 */
ioctl(0x0459002A, [remux, 0]);          /* copy permission */
ioctl(0x000C002A, [remux, pipeline]);   /* 製品のライブ pipeline に載せる */
ioctl(0x00010035, tsout);               /* set_ready */
ioctl(0x0001002A, [remux, 1, 2000]);    /* run */

/* 9. 読み出し */
reader = ioctl(0x37, [tsout, &reader]);
loop {
    ioctl(0x3B, [reader, 1, 500, &addr, &size, &addr1, &size1, &desc]);
    /* addr の扱いは §5.2 */
    ioctl(0x3C, [reader, addr, size, desc]);
}

/* 10. 後始末（製品と同じ順序） */
ioctl(0x0003002A, [remux, 1, 2000]);                 /* stop */
ioctl(0x0454002A, [remux, conn_v, 1]);               /* remove_stream */
ioctl(0x0454002A, [remux, conn_a, 1]);
ioctl(0x044E002A, [remux, SID]);                     /* remove_program */
ioctl(0x0009002A, [remux, conn_v, 1, 2000]);         /* disconnect ×3 */
ioctl(0x0009002A, [remux, conn_a, 1, 2000]);
ioctl(0x0009002A, [remux, conn_out, 1, 2000]);
ioctl(0x3F, [reader, 1]);  ioctl(0x38, reader);      /* cancel してから破棄 */
ioctl(0x34, tsout);
ioctl(0x29, remux);
```

**この 3 つを省くと出力が 1 byte も出ない**（実測）: `0x48` の時刻を `add_program` の
ATS 基準に渡すこと、`config_video`、`set_copy_permission`。[M]

`remove_stream` が `-1042` を返すことがあるが、stop 後に呼ぶ製品と同じ順序であり、
graph は毎回きれいに戻る。[M]

---

## 5. 落とし穴（ここが本番）

### 5.1 TSINFO を繋いではいけない

REMUX は `m_max_output=2` で作るが、**出力は TSOUT 1 本だけ繋げば動く**。
製品は ATS 索引用に TSINFO も繋ぐが、外部から繋ぐと**ドレインの有無に関わらず
数秒で REMUX が入力を取らなくなる**。device status は `0x2`(running) のままで、
`run` を撃ち直しても復帰しない。[M]

| 条件 | 結果 |
|---|---|
| TSINFO 接続・未ドレイン | 約 2 秒（7–9 MB）で停止 |
| TSINFO 接続・ドレインあり | 2.5–10 秒（8–36 MB）で停止、停止時刻は非決定的 |
| **TSINFO 未接続** | **60 秒 217 MB まで停止なし** |

### 5.2 `0x3B` の返すアドレスは物理とは限らない

host メモリに確保されたバッファでは、**既に自プロセスへ map 済みのポインタ**が返る。
製品 `abs:remuxif_task @0x2b89fe` も返り値をそのまま参照している。[S]
これを物理として `0x7A` で map し直すと、**無関係な物理ページを読んで
「それらしいゴミ」が取れる**（ARM のコードやゼロ領域が混ざった 0x80 埋めが出る）。[M]

対処: `/proc/self/maps` に含まれるアドレスなら直接読み、含まれなければ `0x7A` で map する。

### 5.3 `0x38` の前に `0x3F` を呼ぶ

`get_read` で待っている reader は破棄できず `-1034` を返す。その後 buffer 破棄も
`-1040 XCE_IN_USE` になり、**毎回 4.6 MB ずつリークする**（電源を切るまで戻らない）。
製品も host connection を閉じる前に `0x3F` を呼んでいる（`abs:0x2b84b8`）。[S][M]

### 5.4 止まった tap は製品の復旧まで塞ぐ

tap が消費を止めると、ES リングが滞留 → デコーダ枯渇（`HDMI UFLOW`/`SPDF UFLOW`）
→ `adsb` が ES バッファを張り替えようとして **我々の接続が残っているため
`XC_SBUF_DESTROY` が `XCE_IN_USE(-1040)` で失敗** → `ADSB Tuning Error` →
画面が「受信できません」になる。シリアルログで因果を確認済み。[M]

対処: **ライブ video ES の `m_level` を監視し、滞留したら即 disconnect する。**
定常は 9〜14、事故時は 0x65 まで上がっていた。閾値 48 で十分な余裕がある。[D]
通常の gap は 150〜390 ms で自然回復するので、時間だけで判断してはいけない。

### 5.5 `create_reader` を製品バッファに撃たない

`0x37` は成功しても `0x38` で外せないことがあり、枠が 1 つ永久に減る。
**device 接続（`0x0007002A`）は `disconnect` で正常に外れる**ので、
製品のバッファに触るときは必ず device 接続を使う。[M]

### 5.6 アパーチャ外は読めない

TLV リング（`0x1df.....`、4 MiB ×4）と video ES（20 MiB）は
host アパーチャ `0x1F000000..0x29000000` の外にある。**生の TLV を吸って
PC 側で MMT→TS 変換する経路は取れない。**[M] だから REMUX を使う。

---

## 6. 出力フォーマット

- **192 byte TTS**（`[4 byte 到着時刻][188 byte TS]`）。位相 4、同期率 1.0000。[M]
  `me_format=3` の結果である。188 にしたければ 4 byte を捨てる。
- **REMUX が PAT/PMT も生成する。** TSID は `conf_remux` の `m_stream_id`、
  program number は `add_program` の値。`add_psi` でセクションリングを繋ぐ必要はない。[M]
- 生成される PMT: `PCR 0x1001 / stream_type 0x24 (HEVC) PID 0x1011 /
  stream_type 0x11 (AAC-LATM) PID 0x1100`。
- 実測レート 27〜29 Mbps、60 fps、CC エラー 0、`ffprobe` で
  `hevc Main10 3840x2160 yuv420p10le BT.2020 arib-std-b67(HLG)` + `aac_latm`。[M]

**実サービスを名乗らせること。** `m_stream_id` に実 TLV ストリーム ID、
`add_program` に実サービス ID を渡すと、Mirakurun 系クライアントが
サービス単位でフィルタできる TS になる。[D]

---

## 7. 選局（製品 IPC）

### 7.1 なぜ RF 直叩きでは駄目か

製品の MMT フィルタは選局中サービスのアセット向けに構成されている。RF だけ別
TLV ストリームへ動かすと ES リングが空になる。**製品自身に選局させれば全段を作り直す。**[S][M]

```
eAdsbCommProcChangeChannel → msgAdsbCommRequestTumChangeChannel
  → eAdsbTumFSDTVTuningStart → fdc_chmCancelSelectChannel → fdc_chmDisConnectTsUnits
  → TUNER_ADBS1 OFF → RfStandby → OfdmStandby → FemDisconnectDevice ×3
  → fdc_chmConnectTsUnits(source:100 destination:0) → fdc_chmSelectChannel
  → psfc_fed_AdbsSetTuningStart(tuner=1, freq=<IF kHz>, streamid=<TSID>,
                                inputPortNo=100, msgqid=896)
  → FemChangeDevice → SetTsid/SetFreq/SeqStart → @@@lock
  → [REMUX] NetworkID/ServiceID → MPT/CCC → ES 再構成 → デコード再開
```

### 7.2 キューの解決

`/tmp/bdh_out_msgh.lst` が `qid-msqid` の対応表。**msqid は起動ごとに変わる**ので
毎回読むこと。ADSB 通信キューは `qid = 0x36F`(879)。[M]

### 7.3 メッセージ形式（SysV `msgsnd`）

```c
struct wire {                 /* msgsnd に渡す長さは 92（mtype を除く） */
    long mtype;      /* +0x00  常に 1 */
    u32  _unused[2]; /* +0x04  製品は未初期化のまま送る */
    u32  self_ptr;   /* +0x0c  受信側は使わない。0 でよい */
    u32  _inner;     /* +0x10 */
    u32  msg_id;     /* +0x14 */
    u32  len;        /* +0x18  payload の有効長 */
    u32  src;        /* +0x1c */
    u8   payload[64];/* +0x20 */
};
```

この配置は `/proc/<tid>/syscall` で製品タスクの `msgrcv` 引数を読み、
`/proc/<pid>/mem` から受信バッファを採取して**実物と一致することを確認済み**。[M]

### 7.4 チャンネル変更要求

```
msg_id = 0x6004    宛先 qid = 0x36F    len = 0x28（10 word）

payload = [ 0, OID, 9, 0, 0x0b, SID, RF_kHz, TSID, 0, BID ]
             │   │  │  │    │    │     │      │    │   └ リモコンキー番号
             │   │  │  │    │    │     │      │    └ NotDecode（0）
             │   │  │  │    │    │     │      └ TLV ストリーム ID
             │   │  │  │    │    │     └ RF 周波数 kHz（IF ではない）
             │   │  │  │    │    └ サービス ID
             │   │  │  │    └ network id（advanced BS は 0x0b）
             │   │  │  └ Tune（0）
             │   │  └ Dev（9）
             │   └ 操作連番。受信側がエコーするだけ
             └ 常に 0
```

根拠: 送信側 `abs:vEtmMain+0x308 @0x459a64` が 10 word を組み
`eSysSendMessage(0x36F, 0x6004, body, 0x28, -1)` を `0x459af2` で発行する [S]。
受信側はこの body をそのままログへ出すので、リモコンで 10 波すべて切り替えた
シリアルログと突き合わせて確認した [M]。

### 7.5 全 10 波の対応表 [M]

| SID | 局 | RF kHz | TSID | BID |
|---:|---|---:|---|---:|
| 101 | NHK BSP4K | 12,034,360 | 0xB110 | 1 |
| 102 | NHK BS8K | 11,976,820 | 0xB0E0 | 1 |
| 141 | BS日テレ4K | 11,842,560 | 0xB072 | 2 |
| 151 | BS朝日4K | 11,842,560 | 0xB070 | 3 |
| 161 | BS-TBS4K | 12,034,360 | 0xB111 | 4 |
| 171 | BSテレ東4K | 11,842,560 | 0xB071 | 5 |
| 181 | BSフジ4K | 12,034,360 | 0xB112 | 6 |
| 211 | ショップチャンネル | 11,919,280 | 0xB0B0 | 9 |
| 221 | 4K QVC | 11,919,280 | 0xB0B2 | 0xa |
| 231 | （名称なし） | 11,919,280 | 0xB2B1 | 0xb |

IF は `RF − 10,678,000` kHz（LNB LO）。製品の FED メッセージは IF を使うが、
このチャンネル変更要求は **RF** を渡す。

### 7.6 成功判定

応答キューを名乗れないので、送信結果では判定できない。
**製品自身の SI が新サービスになったことを見る**（約 4 秒）。§8 参照。

### 7.7 順序の制約

**選局の前にライブ ES の tap を必ず外す。**残っていると製品側の ES バッファ破棄が
`XCE_IN_USE` で失敗し、§5.4 の停止に至る。選局 → SI 確認 → tap 再構築の順にする。[D]

---

## 7.8 スタンバイと復帰

スタンバイでは **video/audio ES リングが解放される**ので、その状態では配信できない。
残るのは TLV(0x24) と MMT_DEMUX(0x25) だけで、製品はスタンバイ中も選局して EPG を集める。[M]

| | 復号中 | スタンバイ |
|---|---:|---:|
| graph devices | 22 | 11 |
| ARM CPU busy | 54.4% | 51.8% |

**4K 復号の ARM 負荷は 2.6 ポイントしかない**（復号は XCode6 のハードウェア）。[M]

**起床は同じ 0x6004 メッセージで行える。** `Tune = 3`（製品自身も電源オン復帰でこの値を使う）
を送ると、3 秒で復号チェーンと ES リングが再生成され、そのまま配信できる。[M]

```
before : devices 11 / buffers 9
mkra tune4k --sid 141 --tune-mode 3
t+3s   : devices 22 / buffers 24 → 直後の採取で 23.3 MB / 6 秒 / 同期率 1.0000
```

**実用的な省エネは「駐機」で行う。** 電波の来ていないトランスポンダへ選局すると、
復号器は餓死して idle になる（devices は 22 のままだが ES にデータが来ない）。
状態に依存せず必ず効き、実サービスへ選局し直せば 4 秒で戻る。[M]
`mkra serve --idle-sleep <sec>` がこれを自動化する（`--park-freq` / `--park-tsid`）。
止まるのは復号だけで RF は動き続ける点に注意。

**製品スタンバイ相当の就寝は未確立。** `NotDecode = 1` は「製品の電源状態が standby のとき」しか効かず、
通常の稼働状態では受理されるが無視される。[M] 製品の電源状態は `abs` 内の UIM/INI
状態機械が持っており未解読である。

実用上は「リモコンでスタンバイに入れ、配信要求で mkra が起こす」で足りる。
`mkra serve --remux4k --tune` は ES リングが無ければ自動で起床メッセージを送る。

## 8. 選局中サービスの特定

`adsb` が `/tmp/adsb` に書く MMT-SI をそのまま読む。ioctl も不要。[M]

```
AdsbSiMhSdtActual.dat   MH-SDT actual（選局中の TLV ストリーム）
  9f f0 59 | b0 72 | c3 00 00 | 00 0b | ff | 00 8d | ...
  ^tid      ^TLV stream id     ^network id      ^service id
   (+0)      (+3,+4)            (+8,+9)          (+11,+12)

AdsbSiMpt.dat           選局中パッケージの MPT のみ
  20 | 54 | 03 76 | fc | 02 | 00 8d
  ^tid ^ver ^len    ^mode ^idlen ^MMT_package_id = service id
```

両者が一致したときだけ採用する。不一致は選局遷移中とみなす。[D]
セクションは CRC-32/MPEG-2 が 0 になることで検証できる（MH-SDT は long form）。

番組表も同じディレクトリから作れる（`AdsbSiMhEitPF.dat` / `MhEitSch*.dat`）。
実測 297 番組、日本語タイトル・`2160p`・`BT.2100(HLG)` 付きで取れる。[M]

---

## 9. 安全規則（この機種で壊した／壊しかけた実例に基づく）

1. ハンドルを総当たりしない。ドライバの graph から取る。総当たりは kernel oops の実績あり。
2. `0x00020036` GET_READER は `access_ok` 無しの生 memcpy。ベンダの
   `xcode_buffer_get_readers` ラッパーはスタック 16 byte しか確保しないので、
   `m_num >= 2` で呼ぶとスタックを壊す。自前でリクエストを組むこと。
3. 未検証の物理アドレスを `0x7A` で map しない（アパーチャ外はバスが応答しない）。
4. 製品の device / buffer を破棄しない。触るのは自分が作ったものだけ。
5. 実機に置くファイルは `/tmp/mt` だけ。
6. 一度の実機試験で判定する仮説は 1 つ。値の総当たりをしない。
7. 動作中の `tuner` と I2C を競合させない（共有セマフォ shm key `0x3400103B`）。
8. 復旧は再起動より先にリモコン選局／待機→電源オンを試す。RF は無関係に生きていることが多い。

---

## 10. 検証方法と実測値

| 確認したいこと | 方法 | 実測 |
|---|---|---|
| TS が壊れていないか | 188 byte 同期率、PID ごとの CC エラー | 60 秒 217 MB で CC エラー 0 |
| 取りこぼしが無いか | PCR の時間幅 ÷ 実時間 | 20.405 s / 20 s = 1.00 |
| 実際に復号できるか | `ffprobe` / `ffmpeg -frames:v 1` | 3840×2160 の実画像を取得 |
| 製品を壊していないか | graph の device/buffer/port 数、ES の readers | 実行前後で完全一致 |
| クライアントが追いつけているか | サーバ側のクライアント別ドロップ率 | 有線 Mac で 0.000% |
| 選局できたか | `/tmp/adsb/AdsbSiMhSdtActual.dat` | 要求から約 4 秒で切替 |

参考: `mkra` の対応コマンド（実機の `/tmp/mt/mkra`）

```sh
mkra graph --sniff                 # ドライバの live graph を読む（read-only）
mkra remux4k --discover-only       # ライブ ES と pipeline を報告して終了
mkra remux4k --out x.ts --seconds 20
mkra tune4k --sid 141 [--dry-run]  # 製品に選局させる
mkra serve --remux4k --tune --channel-type BS4K --port 40772 --epg-dir /tmp/adsb
```

---

## 11. 他機種へ持っていくときの注意

- **XCode6 の ioctl 番号とリクエスト長はドライバ版に依存する。** `xcdrv.ko` は
  ET_REL で関数名が残っているので、各ハンドラが `+0x8b8` に書く即値がリクエスト長である。
  具体的には各ハンドラが request 長を 1 byte の即値として `+0x8b8` へ `strb` するので、
  逆アセンブルして直前の `mov` の即値を読めばよい。
- **製品 IPC のキュー ID・メッセージ ID は機種ごとに違う。** 手順は同じで、
  (1) シリアルコンソールで製品のログを取り、(2) 送信側関数を逆アセンブルして
  `eSysSendMessage(qid, msgId, body, len, …)` を読み、(3) `/proc/<tid>/syscall` +
  `/proc/<pid>/mem` で実物のメッセージと突き合わせる。
- **「動いたように見えて実は違う」を疑うこと。** この機種では
  「TSI が lock して CRC が進む」も「get_read が data を返す」も、
  有効な MPEG-TS の証拠にはならなかった。最終的な証拠は常に
  「復号できる画が出るか」「CC エラーが 0 か」である。
