# 統合mkraのRAM自動再投入

外部Linuxサーバのsystemd timerから本体の状態を検査し、mkraが消えたときだけ`/tmp/mt/`へ投入する。純正フラッシュ・ブート設定には書かない。公開作業ではこの仕組みを実機へ配備し直していない。

## ローカルでbundleを作る

```sh
./tools/build_unified.sh
python3 tools/mkra-watch/make_bundle.py --output dist/mkra-watch
```

`dist/mkra-watch/`にはARM実行ファイル、5局の追加設定、watcher、telnet helper、ハッシュmanifestだけが入る。buildは純正バイナリを必要としない。既存bundleを上書きしない。manifestのないbundle、バイナリや局表の改変は照合で停止する。

公開版ではビルド済みの特定バイナリのハッシュをソースに固定せず、各bundle作成時にサイズ・SHA-256・転送用MD5を確定する。ソースビルドの違いを反映できる。SHA-256は整合性確認であり署名認証の代わりではない。

## 外部サーバへの配置例

bundleを`/usr/local/libexec/4s-mkra-watch/`にコピーする。実行ユーザー`bs4k-epg`と以下を管理者が準備する。

- `/var/lib/4s-mkra-watch/`（watcherが書ける状態保存先）
- `/var/lib/bs4k-epg/scan.lock`（EPG巡回と共通のlockファイル）
- EPGプロキシとEPGStationの稼働、scanner service名の整合

同梱service/timerは**配置例**。`TUNER_HOST`と`MKRA_API`の文書用アドレスを実環境へ置換し、実行ユーザーとパスも確認する。EPGStationが別URLなら`EPGSTATION_URL`、proxyは`MKRA_PROXY`を指定できる。

```sh
# bundleのディレクトリで。実機へ接続しない自己試験
python3 watch.py --self-test
# 外部サーバにserviceを登録する前に状態と記録を確認
python3 watch.py --dry-run
```

`--dry-run`は接続・安全確認を行うが、RAM転送やサーバ起動をしない。

動作を確認してからsystemd unitを登録する。標準timerは45秒＋最大10秒の揺らぎ。既存環境のunitと衝突しないようにする。

## 保護条件

- APIが正常なら、録画中でも転送・停止・再起動をしない。
- 自分のmkraプロセスが残るのにAPIが異常なら停止せず延期する。
- 純正tuner/card/abs/adsb、driver、IPC、起動後45秒、RAM空き、boot IDを確認。
- 録画中・scanner稼働中は延期し、EPG scannerと同じflockを保持。
- サーバが存在しない場合は、間近の予約のためにendpointを戻すことがある。稼働録画は常に保護する。
- stageへ転送→hash検証→boot再確認→rename→起動。固定された自分の`/tmp/mt/`配下だけを扱う。
- TCPの`tcp_tw_recycle`を修正する処理は、検証済みの一時sysctlコマンドに限定する。恒久設定ファイルは変更しない。
- 接続失敗は15秒から最大300秒までbackoffする。

単にHDMI decoder threadがないことを起動失敗としない。通常BS動作時の黒画面とサーバ正常性は別に確認する。

## 検証の範囲

開発時に、自分のidleサーバとRAMファイルを除去した後の再投入・hash照合・配信・重複防止を確認した。コンセントOFF/ON後のすべての起動段階と録画までを連続して確認する通し試験は未完了。

公開用のmanifest化はローカルのbundle整合性確認と既存watcherテストで検証する。実機で再試験したとの主張はしない。
