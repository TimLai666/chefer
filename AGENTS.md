@CLAUDE.md

## Follow-ups

- **vz-smoke.sh 的 pkill 治標可升級為斷言**：helper stdin-EOF 自我了結（vz.rs + vz-helper/main.swift）合併後，`scripts/vz-smoke.sh` 收尾的 `pkill -f chefer-vz-helper`（目前在主 checkout 未 commit 的變更中）可改成「等 ~10s 斷言無 chefer-vz-helper 殘留」，讓實機一鍵驗證直接覆蓋這個修復。
- **runtime 單發 SIGINT 下 helper 收攤有 ~5s 延遲**：runtime 的 ctrlc handler 等 5 秒才 `exit(130)`，stdin EOF 要到行程死亡才發生（實測 SIGINT→helper 收攤約 6s；SIGTERM/SIGKILL 即時）。若要即時收攤，可讓 vz 後端把 helper stdin 寫端交給訊號處理路徑、收訊號時主動關閉。屬優化，非正確性問題。
- **whp 防孤兒修復待實機驗證**：Job Object（KILL_ON_JOB_CLOSE）+ helper stdin-EOF 自我了結（`crates/vmm-backend/src/whp.rs` + `crates/whp-helper/src/main.rs`）已完成、機制各有 CI 單元測試，但「單殺 runtime 後 helper/VM 不殘留」的端到端行為需實體 Windows（WHP）驗證：以 `CHEFER_BACKEND=whp` 跑一個 bundle → `taskkill /F /PID <runtime pid>`（TerminateProcess，孤兒化的可靠重現法；原始問題本身也尚未在實機重現過）→ 確認 `chefer-whp-helper` 行程數秒內消失、無殘留。通過後刪除本項。
