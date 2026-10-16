# Changelog

## 公開資料・配布設定の追加監査 — 2026-10-17

- 4K専用版の説明4資料を統合版へ更新し、元の説明と否定結果はdocs/historyへ保存。
- 原本と再照合し、I-O DATA完全版/3社比較、失敗条件、判断訂正、数値と警告を補完。
- 匿名化検証表、新規EPG導入、非破壊的確認/段階的復旧を追加。
- proxy/4K・2K scanner/timer/共通env/tmpfiles/EPGStation unit雛形を完備。
- 4K scannerがplaceholderのstoredEvents=0で初回収集を止める不具合を修正し、合成回帰試験を追加。両scannerのEPGStation URLを共通envから指定可能にした。
- 実機・既存EPGStation・予約は変更せず、ローカル検証後に公開main/Actionsで確認する。旧公開コミットと開発原本を保持し、強制pushや本家PRは行わない。

## BS2K experimental preview — 2026-10-17

- 本家mainの履歴とMIT表記を保持した正式Forkとして公開用整理。
- 2026-10-16最終開発版のBS2K受信・ライブCAS・BS4K統合・取得済み4Kサービス確認を追加。
- 6局設定、EPGStation連携、RAM再投入、失敗を含む日本語資料を追加。
- 私的履歴・ダンプ・鍵・放送録画・実IPを公開対象から除外。
- 配布用bundleをローカルbuildからhash manifest付きで生成。番組streamの終了制御は公開プロキシへ統合し、合成試験を追加。
- 公開作業では既存の受信機・EPGStationを変更しない。元の開発履歴を保持してrollback先とする。
- 残課題: HDMI復帰、一部暗号化PID、長時間安定性、完全電源断復旧、番組時刻追従。
