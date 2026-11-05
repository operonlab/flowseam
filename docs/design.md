# Flowseam 架構設計（草案 v0）

> 專案原名 kmshare，2026-11 改名。線上協定與憑證裡的 `kmshare.local`、配對標籤仍沿用舊名（改了等於所有裝置重新配對）。

狀態：設計草案，未實作。依據為 5 路調研（視為待驗證主張）與本檔第 9 節列出的源碼抽查。
範圍：MVP = 鍵盤、滑鼠、游標邊界切換、文字剪貼簿；TLS 配對；Windows 只定義 trait 邊界。

---

## 0. 源碼抽查紀錄（第 2 級：file:line 直讀）

已直接讀取、確認與調研一致：

- deskflow `src/lib/deskflow/ProtocolTypes.cpp:17-22`：`CINN%2i%2i%4i%2i`、`COUT`、`CCLP%1i%4i`，座標為 `%2i`（16-bit）。
- deskflow `src/lib/platform/OSXComputer.mm:667-688`：`hideCursor()` 無 `m_cursorHidden` 守衛，直接 `CGDisplayHideCursor` 並 `CGAssociate(true)`；另以私有 CGS API 設 `SetsCursorInBackground`。
- deskflow `OSXComputer.mm:819-830`：`leave()` 無條件 `hideCursor()`；primary 端 `avoidHesitatingCursor()` 後 `CGAssociate(false)` 凍結游標。primary 分支內無 warp 到中心。
- lan-mouse `input-capture/src/macos.rs:105-118`：`update_bounds()` 以 `min`/`max` 累加，不會縮小。
- lan-mouse `input-capture/src/macos.rs:121-133`：`start_capture` 的 `edge_offset = 1.0`，游標落在邊界內 1 px。
- lan-mouse `input-capture/src/macos.rs:457-504`：`TapDisabledByTimeout` 原地 `CGEventTapEnable`；`TapDisabledByUserInput` 清狀態、顯示游標、通知上層（與調研描述一致）。

未重讀（沿用報告，標「報告主張」）：Deskflow 的 TLS 驗證、剪貼簿 chunk、Issue #10225/#10231 內容、Windows 文件條目。

---

## 1. 架構與 crate 切分

```
            ┌────────────────────── flowseam（單一 binary，CLI daemon）──────────────────────┐
            │  supervisor 程序（只負責游標還原，見 §5）                                     │
            │      └─ worker 程序：組裝下列 crate，跑 event loop                            │
            └──────────────────────────────────────────────────────────────────────────────┘
                 │                         │                          │
        ┌────────▼────────┐       ┌────────▼────────┐        ┌────────▼────────┐
        │ flowseam-core           │◄──────│ flowseam-net            │        │ flowseam-mac / flowseam-win    │
        │ 純 Rust，無 OS   │       │ rustls、framing  │        │ InputBackend 實作 │
        │ 協定型別／codec  │       │ 配對 pin DB      │        │ （flowseam-win 本階段只  │
        │ 切換狀態機       │       │ heartbeat        │        │  是 stub）        │
        │ 幾何／顯示拓撲   │       └──────────────────┘        └────────▲────────┘
        │ 按鍵集合追蹤     │                                              │
        │ InputBackend trait│◄────────────── 實作 ──────────────────────────┘
        │ 游標狀態機       │
        └──────────────────┘
```

依賴方向：`flowseam-core` 不依賴任何其他 crate；`flowseam-net` → `flowseam-core`；`flowseam-mac`/`flowseam-win` → `flowseam-core`；binary 同時組裝四者。`flowseam-core` 的測試不需要任何 OS 權限。

建議依賴（待確認）：`rustls`、`rcgen`（自簽）、`sha2`（指紋）、`postcard` + `serde`（序列化，見 §8-1）、`core-graphics`、`objc2`（macOS）。

---

## 2. 協定訊息

設計原則：版本協商必須是第一則訊息；座標用 `i32` 邏輯像素（不用 deskflow 的 16-bit）；每個控制權段落有單調 `seq`，接收端丟棄舊 `seq` 的訊息。

Framing：`u32` big-endian 長度前綴 + body；單框上限 4 MiB，超過即斷線並 Fault。

| 訊息 | 欄位 | 方向 | 說明 |
|---|---|---|---|
| `Hello` | `ver: u16, node: [u8;16], name: String(≤64B), displays: Vec<Rect>(≤16)` | 雙向 | 第一則。`Rect` 為本機邏輯座標 `i32 x,y,w,h` |
| `HelloAck` | `ver: u16, accepted: bool, reason: Option<String>` | 回應 | 版本不符或指紋未配對即 `accepted=false` 並斷線 |
| `Enter` | `seq: u32, edge: Edge, frac: u16, mods: u8, pressed: Vec<u16>` | 控制端→受控端 | `frac` 為沿邊界的位置比例（0..=65535）；`pressed` 為控制端目前按住的 HID 鍵，供受控端同步 |
| `Leave` | `seq: u32, edge: Edge, frac: u16` | 受控端→控制端 | 游標從受控端回到控制端的位置（以受控端的邊界＋比例表示） |
| `MotionRel` | `seq: u32, dx: i32, dy: i32` | 控制端→受控端 | 邏輯像素相對位移；小數累積在發送端 |
| `Button` | `seq: u32, button: u8, down: bool` | 控制端→受控端 | |
| `Wheel` | `seq: u32, dx: i32, dy: i32` | 控制端→受控端 | 發送端已套用自然捲動設定；受控端不再反轉（避免雙重反轉） |
| `Key` | `seq: u32, hid: u16, down: bool, repeat: bool` | 控制端→受控端 | `hid` 為 USB HID Usage（page 0x07） |
| `ModSync` | `seq: u32, mods: u8, pressed: Vec<u16>` | 控制端→受控端 | 重同步（Enter 之後、網路恢復之後） |
| `Release` | `seq: u32, reason: Reason` | 雙向 | `Reason` = `Edge` / `Emergency` / `Timeout` / `Shutdown` / `TopologyChange` / `UserEscape`；受控端收到即全數放開按鍵 |
| `ClipboardText` | `gen: u64, text: String(≤1 MiB)` | 雙向 | 只傳 UTF-8 文字；超過上限拒絕並 Fault，不截斷 |
| `Ping` / `Pong` | `t: u64` | 雙向 | 間隔 1 s；連續 3 s 無 Pong 視為死線 |
| `Fault` | `code: u16, msg: String` | 雙向 | 協定錯誤、拒絕原因，必顯示給使用者 |

`ModifierSet`（u8 bitmask）：Shift、Ctrl、Alt/Option、Cmd/Win、CapsLock（toggle 狀態，不是按住狀態）。

鍵碼空間：USB HID Usage（page 0x07），修飾鍵為 0xE0–0xE7。各端在本地把 HID 映射到 CGKeyCode 或 Windows VK/scancode。

---

## 3. 邊界切換幾何

**拓撲歸各端自己所有**：每端只知道自己的 `displays`（由 `displays()` 取得）。Wire 上只交換 `Rect` 與邊界比例，不交換絕對座標對映。

**出口判定**（控制端，本機座標系）：
- 對 `Right` 出口，游標 `p` 滿足「存在顯示器 D 包含 `p`」且「沒有任何顯示器包含 `(p.x+1, p.y)`」。內部接縫不算出口。`Left/Top/Bottom` 同理。
- 只有面向對端的那條出口會觸發切換；其餘邊界一律夾住（clamp）。
- 出口的「沿邊位置」：`frac = (p.y - top) / (bottom - top)`，`top/bottom` 為該出口所屬連續邊界段的範圍（多螢幕時以包含 `p` 的那段為準）。

**入口落點**（受控端）：
- 依 `Enter.edge` 找對端的同向外邊界段；`y = top + frac × (bottom - top)`，四捨五入。
- 落點 x 不放在邊界上，而是內縮 `ENTRY_INSET = 24 px`（避免立刻又出口）。
- 若落點不在任何顯示器內，夾到最近顯示器的內縮矩形。

**回切**（受控端 → 控制端）：
- 受控端的虛擬游標 = 入口落點 + 累積 `MotionRel`，夾在受控端的 union 內。
- 只有面向控制端那條邊可觸發 `Leave`；觸發時回傳受控端的 `(edge, frac)`，控制端入口落點內縮 24 px。
- 同一方向的重複切回：入口後需要游標往內移動至少 `ARM_DIST = 4 px` 才解除出口武裝，過濾觸控板抖動。這直接避開 lan-mouse 的 1 px re-entry 幾何。

**角落**：
- 角落定義為兩條邊界都在 `CORNER_R = 16 px` 範圍內。角落內一律不觸發切換，直到游標離開角落區域。
- 角落若只有一個方向有鄰居，依該方向的規則判定；兩個方向都沒有鄰居則純夾住。

**多螢幕與缺口**：
- 對端有多個顯示器時，入口使用包含 `y` 的最近外邊界段。
- 本端 L 形或高度不一致時，以 `frac` 比例映射；超出對端段範圍就夾住，不外推。

**拓撲變更**：
- 每次 `displays` 變更（接電源、解析度變更、螢幕拔除）都以 `CGDisplay::active_displays` 整體重建，不做 min/max 累加。
- 控制權在遠端時發生拓撲變更：立即 `Release(TopologyChange)`，把游標還給本機，再重新發 `Hello`。不嘗試在途中修正座標。

---

## 4. InputBackend trait（只列簽章）

```rust
pub struct Point { pub x: i32, pub y: i32 }
pub struct Rect { pub x: i32, pub y: i32, pub w: i32, pub h: i32 }
pub enum Edge { Left, Right, Top, Bottom }

#[derive(Debug)]
pub enum Preflight { Ok, Missing(Permission), Unknown(String) }
// Permission: Accessibility | InputMonitoring | PostEvent（名稱依平台）

pub enum Disposition { Pass, Swallow }

pub trait CaptureSink: Send {
    /// 必須在微秒級返回（只做 channel send）；不得阻塞、不得呼叫 OS API。
    fn on_event(&mut self, ev: CapturedEvent) -> Disposition;
    fn on_tap_fault(&mut self, f: CaptureFault);
}

pub trait InputCapture {
    fn preflight(&self) -> Preflight;
    fn displays(&self) -> Result<Vec<Rect>, BackendError>;
    fn set_display_change_sink(&mut self, cb: Box<dyn Fn() + Send>) -> Result<(), BackendError>;
    /// 安裝 tap（或 hook）。回傳 Unknown 時不得當作成功。
    fn start(&mut self, sink: Box<dyn CaptureSink>) -> Result<(), BackendError>;
    fn stop(&mut self) -> Result<(), BackendError>;
    /// grabbed=true：吞掉本機輸入、隱藏游標、凍結游標；false 反之。冪等。
    fn set_grabbed(&mut self, grabbed: bool) -> Result<(), BackendError>;
    fn cursor_position(&self) -> Result<Point, BackendError>;
    fn warp_cursor(&mut self, p: Point) -> Result<(), BackendError>;
    fn secure_input_active(&self) -> Result<Option<bool>, BackendError>; // None = 查不到
}

pub trait InputEmitter {
    fn preflight(&self) -> Preflight;
    fn move_rel(&mut self, dx: i32, dy: i32) -> Result<EmitOutcome, BackendError>;
    fn move_abs(&mut self, p: Point) -> Result<EmitOutcome, BackendError>;
    fn button(&mut self, button: u8, down: bool) -> Result<EmitOutcome, BackendError>;
    fn wheel(&mut self, dx: i32, dy: i32) -> Result<EmitOutcome, BackendError>;
    fn key(&mut self, hid: u16, down: bool, repeat: bool) -> Result<EmitOutcome, BackendError>;
    fn set_modifiers(&mut self, mods: ModifierSet) -> Result<EmitOutcome, BackendError>;
    /// 放開所有本端注入過、尚未放開的按鍵，並清空狀態。
    fn release_all(&mut self) -> Result<(), BackendError>;
    /// 冪等的游標顯示（後端內部記錄狀態，不做計數累加）。
    fn set_cursor_visible(&mut self, visible: bool) -> Result<(), BackendError>;
}

/// 三態：OS API 無法區分時，回傳 Unknown 而非 Ok。
pub enum EmitOutcome { Delivered, Unknown(String) /* 例如 Windows UIPI 可能擋 */ }

pub trait Clipboard {
    fn change_count(&self) -> Result<u64, BackendError>;
    fn get_text(&self) -> Result<Option<String>, BackendError>;
    fn set_text(&mut self, s: &str) -> Result<(), BackendError>;
}

pub trait InputBackend: InputCapture + InputEmitter + Clipboard {}
```

`BackendError` 必須區分：`PermissionMissing` / `TapCreateFailed` / `SecureInput` / `Os { code, api }` / `Unsupported`。不得只回傳字串。

Capture 與 Inject 分開實作的理由：只擔任接收端的機器不需要 tap，權限面也較小，可獨立授權。`InputBackend` 是組合便利，不是強制。

---

## 5. 游標隱藏／還原與崩潰安全

**flowseam-core 的游標狀態機**：`Visible ⇄ Hidden`。只在狀態轉換時呼叫後端一次；後端 `set_cursor_visible` 必須冪等（自己記錄是否已隱藏，不依賴 OS 的計數器）。

macOS 轉換：
- `Visible → Hidden`：`CGDisplayHideCursor`（僅一次）→ `CGAssociateMouseAndMouseCursorPosition(false)` → warp 到螢幕中心（避開 Dock 與選單列，這是 deskflow #10225 的對策方向，不沿用 deskflow 離開時不 warp 的行為）。
- `Hidden → Visible`：`CGAssociate(true)` → `CGDisplayShowCursor`（僅一次）。
- 不使用 deskflow 的私有 CGS API `SetsCursorInBackground`，除非 spike 證明必要。

**崩潰安全（多層）**：
1. **狀態檔**：進入 Hidden 前寫入 `~/Library/Application Support/flowseam/grab.lock`（內容：pid、狀態、時間）；還原後刪除。
2. **Supervisor 程序**：binary 啟動時 fork worker 並監看。worker 異常結束（含 SIGKILL）→ supervisor 直接呼叫 `CGDisplayShowCursor` 與 `CGAssociate(true)`，清 lock 檔。supervisor 不需要輔助使用權限，只做還原。
3. **Worker 內**：SIGINT/SIGTERM/SIGHUP handler 只設 atomic flag；主迴圈讀到後在主執行緒還原。panic hook 與 Drop guard 同樣還原（signal handler 內不呼叫 CG API）。
4. **啟動時自檢**：發現 lock 檔且 pid 已死 → 先還原、再啟動。

**未驗證（關鍵）**：調研查不到 WindowServer 在程序死亡時是否自動重設隱藏計數與 association。設計不依賴此假設；supervisor 是必要條件。
**Slice 0 驗收**：隱藏游標 → `kill -9` worker → supervisor 還原 → 游標可見且可移動，錄下結果（第 4 級證據）。

**邊界偽影**：離開時 warp 到中心，因此游標不會停在 Dock／選單列上。若 warp 後游標仍隱藏失敗，以 `CGDisplayIsCursorHidden`（待驗 API 存在性）檢查，失敗即 Fault。

---

## 6. 修飾鍵同步與卡鍵防護

- 控制端維護 `pressed: BitSet(256)`（HID 鍵）與 `ModifierSet`。Enter 時以 `pressed` 與 `mods` 全量送出（`Enter` 欄位），受控端 `release_all()` 後依此重建。
- 受控端的修飾鍵以真實 keycode + `FlagsChanged` 注入（lan-mouse 的做法，`input-emulation/src/macos.rs:228-251` 報告主張，未重讀）；裸 `CGEvent` 預設 keycode 0 等於送出 `A`，禁止。
- 修飾鍵**不進入 repeat 機制**；repeat 由受控端 OS 產生，`Key.repeat` 只做提示，不由發送端定時重送（避免與 OS repeat 疊加）。
- CapsLock 為 toggle 狀態：只在 `ModSync` 與 `Enter` 同步狀態，不轉送 CapsLock 的 down/up。
- 放開觸發點：`Release`（任何原因）、心跳死線、連線中斷、緊急鍵、程序結束，受控端一律 `release_all()`；控制端在失去控制權時也對自己的實體鍵狀態做一次 `CGEventSourceKeyState` 比對重同步。
- 控制端在控制期間仍吞掉本機鍵盤事件，因此「本機實體放開」由 tap 看見並轉送 `Key{down:false}`；若 tap 失效則由 `Pong` 死線觸發全放開，不靜默保留。

---

## 7. 失敗行為（fail loud，不靜默）

| 情境 | 偵測 | 行為 | 可見症狀 |
|---|---|---|---|
| Accessibility／輸入監控缺失 | `preflight()` = Missing | 拒絕啟動 capture，列出缺少的權限與面板位置 | 啟動即報錯，不進入 grab |
| tap 建立失敗（回傳 NULL） | `start()` 錯誤 | 同上，`TapCreateFailed` | 同上 |
| tap 逾時被系統停用 | `TapDisabledByTimeout` | 原地 re-enable，保留狀態；**斷路器**：10 s 內 > 3 次即拆除 tap 並 Fault | 日誌＋狀態列紅燈 |
| tap 被使用者輸入停用（secure input／權限撤銷） | `TapDisabledByUserInput` | 清狀態、還原游標、Release 對端、Fault | 立即回到本機 |
| Secure Input 啟用 | `secure_input_active() == Some(true)` | 不進入 grab，Fault 說明「有程式啟用 Secure Input」 | 不會被鎖在遠端 |
| Secure Input 查詢不到 | `None` | 視為「查不到」，**不**當成未啟用；保留 grab 但在 UI 標示 | 不靜默 |
| 指紋不符 | TLS verifier | 中止，印出雙方指紋 | 手動配對流程 |
| 協定版本不符 | `HelloAck` | 拒絕，印出雙方版本 | 同上 |
| Ping 死線（3 s） | 心跳計時器 | 受控端 release_all、控制端還原游標並退回本機 | 游標回到本機 |
| 注入回傳 `Unknown`（例：Windows UIPI） | `EmitOutcome::Unknown` | 不重試，計入統計並 Fault（「可能被目標視窗權限擋下」） | 明確提示，不假裝成功 |
| 剪貼簿超過 1 MiB | 發送端檢查 | 拒絕該次同步，Fault | 不截斷 |
| 拓撲變更於遠端控制中 | `display_change` | Release(TopologyChange)，重建拓撲 | 游標回本機 |
| 緊急鍵 | 雙端各自的本地 tap／hook | 見 §6；**不依賴網路** | 立即回本機 |

原則：沒有明文 fallback；沒有靜默重新隱藏游標；斷路器跳脫後不自動重試，等使用者重新啟動。

---

## 8. 要你決定（只列需要 owner 拍板的）

1. **序列化格式**：`postcard`（推薦：一個小依賴、schema 由 Rust enum 決定、version 欄位明確）vs 手寫固定寬度（零依賴、但要自己維護對齊）。推薦 postcard。
2. **角色模型**：固定（一台 Mac 永遠是控制端）vs 對稱（兩端皆可控制）。推薦 MVP 固定，但 wire 協定已允許之後切換；對稱會引入「誰擁有控制權」的競爭問題。
3. **配對 UX**：指紋人工比對（deskflow 式）vs 短配對碼（兩端顯示 6 位數碼、輸入確認）。推薦短配對碼＋指紋 pin 存檔（比對一次，之後自動）。
4. **緊急鍵**：推薦 `Ctrl+Option+Cmd+Esc`（待驗：與 macOS 內建熱鍵無衝突；Windows 對應 `Ctrl+Alt+Win+Esc`）。
5. **開發簽章身分（最可能第一週就咬人）**：調研報告（社群彙整，未在本機驗證）指出 ad-hoc 簽章的 TCC 以 cdhash 辨識，每次重建都要重新授權。推薦從第一天就用穩定身分（Developer ID，或 keychain 內固定的 code-signing 憑證）。否則違反「不要每次重新授權」的前提。
6. **Win 平台 Cmd/Ctrl 對映**：HID 傳輸後由接收端解讀。推薦 Windows 端把 Cmd 對映到 Ctrl（可設定）。
7. **埠號**：推薦 `24807`（避開 deskflow 預設 24800）。
8. **剪貼簿**：推薦雙向、只傳文字、上限 1 MiB。
9. **角落行為**：推薦 16 px 死區、無鄰居則純夾住（§3）。

---

## 9. 待驗證主張（facts 清單）

見 StructuredOutput 的 facts 欄位。驗證方法包含 slice 0 的 kill -9 實驗、幾何單元測試（flowseam-core，無 OS 權限）、以及在真機上的 tap 停用／睡眠喚醒實驗。

## 10. 未解的查不到項目

- WindowServer 在程序死亡時是否還原游標（§5）。
- `TlsEnabled` 預設值未在 deskflow `Settings.cpp` 中找到，因此 deskflow 預設是否加密未定案（本設計不採用明文，不影響）。
- Secure Input 的偵測 API（`IsSecureEventInputEnabled` 之類）存在性與行為未驗。
- 睡眠喚醒後 tap 是否需重建（報告為單一專案做法，未經 Apple 官方說明）。
- 顯示器關閉後 tap 失效（deskflow PR #10297 報告主張，macOS 27.0.1 修正）未重現。
- Windows 側：secure desktop（UAC）下 hook／注入行為、剪貼簿 API 均未驗。
