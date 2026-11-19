# GProTray

Windows 11向けの超軽量・Rust製 Logicool G PRO Wireless（HID++ 2.0）タスクトレイ設定ツールです。  
Logicool G HUB を常駐させずに、タスクトレイから最小限のメモリ消費（アイドル時 Working Set 約 9 MB / Private Bytes 約 1.7 MB）でマウス設定を管理できます。

## 主な機能

- **トレイ常駐・イベント駆動監視**: ポーリングを行わず、`WM_DEVICECHANGE`・`WM_POWERBROADCAST`・HID++ 通知のみで接続/スリープ復帰を追従
- **DPI / ポーリングレート変更**: `0x2201` / `0x8060` およびオンボードモード時セクタ更新（読み戻し検証・スリープ復帰時の選択DPI自動復元対応）
- **オンボードプロファイル管理 (`0x8100`)**: スロット 1〜5 の有効化/無効化・プロファイル切替
- **ボタン・プロファイル設定画面**: ネイティブ Win32 ダイアログで、プロファイル名・DPI 1〜5・デフォルトDPI・レート・ボタン 1〜8（標準HIDキー `F1〜F24` / `Page Up` 等 + 修飾キー組合せ）を編集可能
- **簡易RGB設定 (`0x8070` / セクタLED)**: 消灯・カラーサイクル・固定色・ブリージング
- **Windows起動時自動実行**: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 登録（管理者権限不要）
- **共存設計**: Windows標準HIDドライバのまま `FILE_SHARE_READ | FILE_SHARE_WRITE` および固有 `SW_ID = 0x0E` を使用し、`WirelessBattery` 等の他ツールと競合しない設計

## ビルド・診断コマンド

```powershell
cargo test --offline
cargo build --release --offline
```

- 読み取り専用プローブ: `.\target\release\gpro-tray.exe --probe`
- CLI設定変更: `--set-dpi <DPI>` / `--set-rate <HZ>`

## ライセンス・出典

- 本体ライセンス: [MIT License](LICENSE)
- 参照した MIT ライセンス OSS の帰属表記: [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)
