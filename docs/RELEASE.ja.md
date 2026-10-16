BS2K/BS4K統合版の実験的プレビューです。otti83さんのmkraを基にしたForkです。

ARMv7 little-endian / musleabihfの静的バイナリを公開ソースからGitHub Actionsでビルドしています。
ライセンス・追加局表付きの`mkra-armv7.tar.gz`を推奨します。単体`mkra`を取得する場合も同じReleaseのLICENSEとthird-party NOTICEを保持してください。
`mkra`と`mkra.sha256`を同じディレクトリへ取得し、`sha256sum -c mkra.sha256`で照合してください。

本体のRAM上で起動する例:

```sh
/tmp/mt/mkra serve --remux4k --tune --bs2k --channel-type BS4K --port 40772 \
  --isdbs-channels /tmp/mt/bs-additional-main.tsv
```

追加局表はリポジトリの`config/bs-additional-main.tsv`から取得してください。既に本体の管理手段が使え、純正サービスが稼働している環境が必要です。

通常BS6局の主映像音声・120秒ライブ・EPGStation連携・2K後の4K録画切替を開発時に確認しました。Actionsは実機の受信検証ではありません。公開用のbundle manifest化とEPG番組stream終了制御はホスト上の試験で確認しています。

物理チューナー1基、通常BS後のHDMI復帰未解決、BS日テレ一部PID暗号化残存、CS未検証、長時間運用未確認、電源断復旧の通し試験未完了、番組延長追従が残課題です。READMEとdocsの制限を確認してください。

MITライセンス。本家の著作権表示と、MULTI2参照実装のApache-2.0 NOTICEを保持しています。純正ファームウェア・鍵・カード情報・放送録画は同梱していません。
