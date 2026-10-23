# 4S-C00AS1 / CT141 接続経路

Codexがこのリポジトリを作業するときに使う、確認済みの管理経路。

```text
agent-101-vm
  └─ SSH TCP/22 → CT141: 192.168.1.52 (root)
       ├─ Telnet TCP/2323 → SHARP 4S-C00AS1: 192.168.1.51 (root shell)
       └─ HTTP TCP/40772 → SHARP mkra API (読み取り用GET)
```

CT141への接続例:

```sh
ssh -i "$HOME/.ssh/butlerx_bs4krec_141_ed25519" \
  -o IdentitiesOnly=yes root@192.168.1.52
```

秘密鍵の実体はagent-101-vm内に保持し、このリポジトリへコピー・登録しない。4SへはCT141から接続する。Telnetコマンドはリポジトリの `tools/sharp_telnet.py` にある破壊的操作ガード付きクライアントを使う。4S APIは `http://192.168.1.51:40772`。調査ではGETだけを使い、選局を起こす `/api/services/{id}/stream` は録画・予約安全性を別途確認しない限り呼ばない。

CT141で確認した実機識別情報は `bs4krec-141-ct`。4S側のhostnameは `vixs`。CT141上のwatcherは `4s-mkra-watch.timer` / `4s-mkra-watch.service` で、APIが正常なら機器へ書き込まず、既存mkraプロセスがある状態では停止・置換しない。

接続できない場合に、ファイアウォール制限の回避や別ホストへの横展開を行わない。認証鍵の内容、予約・録画データ、放送ログの生データはこの文書へ記録しない。
