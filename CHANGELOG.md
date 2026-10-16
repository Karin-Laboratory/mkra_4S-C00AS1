# Changelog

## BS2K experimental preview — 2026-10-17

- 本家mainの履歴とMIT表記を保持した正式Forkとして公開用整理。
- 2026-10-16最終開発版のBS2K受信・ライブCAS・BS4K統合・取得済み4Kサービス確認を追加。
- 6局設定、EPGStation連携、RAM再投入、失敗を含む日本語資料を追加。
- 私的履歴・ダンプ・鍵・放送録画・実IPを公開対象から除外。
- 配布用bundleをローカルbuildからhash manifest付きで生成。番組streamの終了制御は公開プロキシへ統合し、合成試験を追加。
- 公開作業では既存の受信機・EPGStationを変更しない。元の開発履歴を保持してrollback先とする。
- 残課題: HDMI復帰、一部暗号化PID、長時間安定性、完全電源断復旧、番組時刻追従。
