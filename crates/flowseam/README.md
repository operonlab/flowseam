# Flowseam（GUI + CLI）

一個執行檔 `flowseam`：視窗設定邊界與快捷鍵、配對、啟停 worker。

## Build / Run

```
cargo build -p flowseam
cargo run -p flowseam            # 等同 cargo run -p flowseam -- gui
cargo test -p flowseam
```

子命令：`gui`（預設）、`worker --config <path>`（由 GUI 自動呼叫，一般不手動跑）、`pair`（終端配對）。

## 第一次要做的授權（macOS）

系統設定 → 隱私權與安全性，替「實際執行 Flowseam 的程式」打開：

1. **輔助使用**（Accessibility）
2. **輸入監控**（Input Monitoring）

從終端機跑時，被授權的通常是那個終端機 App（Terminal / iTerm / cmux），不是 Flowseam 本身。
授權後要重開該 App。視窗底部狀態列會寫缺哪一項；改完按「重新檢查權限」。

## 配對

1. 兩台都先在 GUI 的「邊界」新增 peer，填名稱與對方 `host:port`（預設埠 24800）。
2. 對方顯示 6 位配對碼；本機在「配對」選該 peer、輸入碼、按「配對」。
3. 成功後該列會顯示指紋前 8 碼，再按「存檔」。未配對的 peer 無法存檔（指紋必填）。

## 邊界怎麼測

1. 設好 peer 的 edge（例：mini 在 right）、dwell_ms、chord，按「存檔」。
2. 按 Start；狀態列應顯示 `running`。
3. 把游標推到右邊緣停留超過 dwell_ms，應切到對方；或按 chord 手動切換。
4. 換邊測：同一邊不能有兩台，GUI 會列出所有錯誤且不寫檔。
5. 死亡測試：另開終端 `kill -9` worker 子程序，狀態列應顯示 `died: ... 沒有 CLEAN`，游標應被還原。

設定檔：`~/.config/flowseam/config.toml`；身分：`~/.config/flowseam/identity/`。舊名 `~/.config/kmshare/` 的設定與身分會在第一次啟動時整個搬過來，不必重新配對。

## 目前的 stub（見 src/stubs.rs）

- worker：尚未接 `flowseam-mac::worker::run`，只示範啟停，**不會真的擷取輸入**。
- 配對：尚未接 net2 的 `pair_as_client`，按下去會回報「尚未接上」。
- chord 檢查：僅語法檢查，待後端 parser。
- 死亡後的游標還原：GUI 目前只判定 died，尚未呼叫 `supervise` + `real_cursor_os()`。
