@CLAUDE.md

## Follow-ups

- **vz-smoke.sh 的 pkill 治標可升級為斷言**：helper stdin-EOF 自我了結（vz.rs + vz-helper/main.swift）合併後，`scripts/vz-smoke.sh` 收尾的 `pkill -f chefer-vz-helper`（目前在主 checkout 未 commit 的變更中）可改成「等 ~10s 斷言無 chefer-vz-helper 殘留」，讓實機一鍵驗證直接覆蓋這個修復。
- **runtime 單發 SIGINT 下 helper 收攤有 ~5s 延遲**：runtime 的 ctrlc handler 等 5 秒才 `exit(130)`，stdin EOF 要到行程死亡才發生（實測 SIGINT→helper 收攤約 6s；SIGTERM/SIGKILL 即時）。若要即時收攤，可讓 vz 後端把 helper stdin 寫端交給訊號處理路徑、收訊號時主動關閉。屬優化，非正確性問題。
- **whp 防孤兒修復待實機驗證**：Job Object（KILL_ON_JOB_CLOSE）+ helper stdin-EOF 自我了結（`crates/vmm-backend/src/whp.rs` + `crates/whp-helper/src/main.rs`）已完成、機制各有 CI 單元測試，但「單殺 runtime 後 helper/VM 不殘留」的端到端行為需實體 Windows（WHP）驗證：以 `CHEFER_BACKEND=whp` 跑一個 bundle → `taskkill /F /PID <runtime pid>`（TerminateProcess，孤兒化的可靠重現法；原始問題本身也尚未在實機重現過）→ 確認 `chefer-whp-helper` 行程數秒內消失、無殘留。通過後刪除本項。
- **branch protection 的必要檢查名稱失效，每個 PR 都得 admin bypass**：main 的 required status checks 含一條 `whp-helper (windows compile-check)`（無後綴），但該 CI job 從建立起就是 matrix（`.github/workflows/ci.yml`），實際回報名稱是 `whp-helper (windows compile-check) (x86_64-pc-windows-msvc)` 與 `(aarch64-pc-windows-msvc)`——這條 context 永遠不會被滿足，所有 PR 的 merge state 都是 BLOCKED，只能靠 owner `--admin`／網頁勾「bypass」合併（2026-07-11 PR #126 實測確認）。修法（需 repo 管理權限，Settings → Branches → main 的 required status checks，或 `gh api` PATCH `branches/main/protection/required_status_checks`）：把無後綴那條換成上述兩條帶 target 後綴的。修好後刪除本項。
