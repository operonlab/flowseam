<!-- doctype: contract -->
# Flowseam

[English](README.md) · **繁體中文** · [日本語](README.ja.md) · [한국어](README.ko.md)

一組滑鼠與鍵盤，同時操作你的 Mac 與 PC。把游標推出一個螢幕的邊緣，就會接續出現在下一台電腦上；你輸入的內容跟著游標走，剪貼簿和你拖曳的檔案也一樣。

https://github.com/user-attachments/assets/be5499eb-c39a-4722-b87a-0203b3a30c64

<sub>示範影片的旁白與音樂由 ElevenLabs（elevenlabs.io）生成。</sub>

> **狀態：早期版本（0.1）。** 已在兩台 Mac 與一台 Windows PC 上每天使用。仍有不少粗糙之處，請讓每台機器都使用相同版本。

## 功能

| | Mac ↔ Mac | Mac → Windows | Windows → Mac |
|---|---|---|---|
| 滑鼠與鍵盤跨越螢幕邊緣 | 支援 | 支援 | 不支援：Windows PC 只能被控制，不能控制別台 |
| 剪貼簿文字 | 支援 | 支援 | 支援 |
| 跨邊緣拖曳檔案 | 支援，放開在哪裡就放在哪裡：資料夾、桌面或某個 App | 支援，儲存到「下載」資料夾 | 尚未支援 |
| 音量與媒體鍵 | 支援 | 支援 | 不支援 |

- **切換。** 讓游標在共用的邊緣停留片刻（預設 200 毫秒），或按下快捷鍵（預設 ⌃⌥⌘S）。按住滑鼠按鍵時，游標會留在原本的螢幕上，因此拖移視窗或選取文字時不會誤滑到下一台電腦；拖曳檔案是唯一的例外。
- **奪回控制權。** 在 Mac 上按 ⌃⌥⌘Esc，一定能把滑鼠與鍵盤拿回這台 Mac。
- **在 Windows 上使用 Mac 快捷鍵。** 從 Mac 操作 Windows PC 時，可以轉換快捷鍵：⌘C 複製、⌘Tab 切換 App、單按 ⌥ 開啟「開始」功能表（裝置與邊界 › 按鍵對應）。
- **Mac 之間最快的連線。** 兩台 Mac 之間有多條網路相連時，Flowseam 會使用最快的一條：Thunderbolt 或乙太網路優先於 Wi‑Fi。
- **以隱私為設計原則。** 機器之間只需用 6 位數配對碼配對一次。之後的每個連線都使用雙向 TLS，並綁定你當初配對的那台機器的憑證。資料不會經過任何伺服器：Flowseam 只透過你的區域網路，或你自己架設的 SSH 通道通訊。它沒有任何遙測，檢查更新也只會開啟 Releases 頁面。
- **不擋你的路。** 選單列圖示（Mac）或系統匣圖示（Windows）會顯示共用是否開啟，以及哪些機器已連線；關閉視窗後 Flowseam 仍會持續執行。如果背景程序當掉或停住，會自動重新啟動。登入時自動開啟是一項設定，預設為關閉。
- **四種語言。** English、繁體中文、日本語與 한국어。App 會跟隨系統語言；也可以在 設定 › 進階 › 語言 中改用其他語言。

## 畫面

<p>
  <img src="docs/screenshots/zh-TW/mac-overview.png" width="49%" alt="Mac 上的總覽：這台 Mac 在中間，MacBook Pro 在左側，Windows PC 在右側">
  <img src="docs/screenshots/zh-TW/win-overview.png" width="49%" alt="Windows PC 上的總覽：Mac mini 在左側，MacBook Pro 在上方">
</p>

## 安裝

### macOS 13 或更新版本（Apple silicon 與 Intel）

```sh
brew install --cask operonlab/tap/flowseam
```

或從 [Releases](https://github.com/operonlab/flowseam/releases) 下載 `Flowseam-<version>-macos.zip`。

先開啟 Flowseam 一次。macOS 會要求**輔助使用**與**輸入監控**權限（系統設定 › 隱私權與安全性），請兩者都允許。每個版本都使用同一張憑證簽署，因此更新後權限會保留。

Flowseam 是以自己的憑證簽署，沒有經過 Apple 公證。Homebrew cask 會幫你移除下載時的隔離標記。如果你是用 zip 安裝，macOS 會拒絕第一次開啟：請打開 系統設定 › 隱私權與安全性，然後選擇「**強制打開**」。

更新請執行 `brew upgrade --cask flowseam`。

### Windows 10 或 11

目前還沒有安裝程式，請用 Rust（stable，MSVC 工具鏈）自行建置：

```powershell
git clone https://github.com/operonlab/flowseam
cd flowseam
cargo build --release -p flowseam
.\target\release\flowseam.exe
```

Flowseam 會開啟設定視窗，並在系統匣放上一個圖示。出現提示時，請讓它通過 Windows 防火牆：Mac 會透過 TCP 連接埠 24800 連到它。

## 設定步驟

1. **加入另一台電腦**：在每台機器的**裝置與邊界**中，填入名稱、位址（主機名稱或 IP），以及它所在的螢幕邊緣。

   <img src="docs/screenshots/zh-TW/mac-edges.png" width="600" alt="裝置與邊界：每台電腦的位址、所在的螢幕邊緣與按鍵對應">
2. **配對。** 由一台顯示配對碼、另一台輸入。在顯示的那台（Windows PC，或兩台 Mac 配對時的任一台）先關閉共用（配對會用到同一個連接埠），開啟**配對**，選擇另一台電腦，再選擇**顯示配對碼**。在另一台 Mac 上開啟**配對**，選擇剛才那台電腦，輸入配對碼並確認，再按**儲存**。配對只需進行一次；之後兩台機器會自動互相信任。

   <img src="docs/screenshots/zh-TW/mac-pairing.png" width="600" alt="配對：選擇一台電腦，輸入它的配對碼，或顯示這台的配對碼">
3. **開啟共用**：從視窗或選單列圖示開啟，然後把游標推過邊緣。

## 已知限制

- Windows PC 可以被控制，但不能控制另一台電腦，也還不能把檔案從 Windows 拖出去。
- 拖曳到 Windows PC 的檔案會儲存到「下載」資料夾，而不是你放開的位置。
- 剪貼簿只傳送文字：不含圖片、格式化文字或檔案。
- 不支援 Linux。
- Windows 沒有安裝程式，也沒有簽署過的版本，請從原始碼建置。
- 機器是依你輸入的位址找到的；目前還沒有在區域網路上自動探索的功能。

## 疑難排解

- **記錄檔。** macOS：`~/Library/Logs/Flowseam/worker.log`。Windows：`%LOCALAPPDATA%\Flowseam\worker.log`。上一次執行的記錄會保留為 `worker.log.1`。
- **設定與配對資料**放在 `~/.config/flowseam`（Windows 上是 `%USERPROFILE%\.config\flowseam`）。
- **游標過不去。** 請確認共用已開啟、兩台機器都顯示對方已連線，而且你推向的邊緣就是在裝置與邊界中設定的那一邊。
- **Mac 上出現「缺少權限」。** 到 系統設定 › 隱私權與安全性 › 輔助使用 與 輸入監控，把 Flowseam 開啟，然後結束並重新開啟它。

## 從原始碼建置

需要 Rust stable。`cargo test --workspace` 可在 macOS 與 Windows 上執行。`scripts/make-app.sh <version>` 會建置並簽署同時支援 Apple silicon 與 Intel 的 macOS App；需要有程式碼簽署身分，以及 `cargo-about`（`cargo install cargo-about --locked --features cli`）。

設計筆記在 [docs/](docs/)。變更紀錄列在 [CHANGELOG.md](CHANGELOG.md)。

## 授權

[MIT](LICENSE)。Flowseam 所使用的開源函式庫，其授權條款隨 App 一併提供，位於 `Flowseam.app/Contents/Resources/THIRD-PARTY-NOTICES.html`。
