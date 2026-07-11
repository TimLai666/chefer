@CLAUDE.md

## Follow-ups

- **vz-smoke.sh 的 pkill 治標可升級為斷言**：helper stdin-EOF 自我了結（vz.rs + vz-helper/main.swift）合併後，`scripts/vz-smoke.sh` 收尾的 `pkill -f chefer-vz-helper`（目前在主 checkout 未 commit 的變更中）可改成「等 ~10s 斷言無 chefer-vz-helper 殘留」，讓實機一鍵驗證直接覆蓋這個修復。
- **runtime 單發 SIGINT 下 helper 收攤有 ~5s 延遲**：runtime 的 ctrlc handler 等 5 秒才 `exit(130)`，stdin EOF 要到行程死亡才發生（實測 SIGINT→helper 收攤約 6s；SIGTERM/SIGKILL 即時）。若要即時收攤，可讓 vz 後端把 helper stdin 寫端交給訊號處理路徑、收訊號時主動關閉。屬優化，非正確性問題。
- **Windows whp-helper 疑有同款孤兒化問題**：`crates/vmm-backend/src/whp.rs` spawn `chefer-whp-helper` 時無 Job Object、無 stdin liveness——對 runtime 單殺（`taskkill /PID`，非 console Ctrl+C）時 helper/VM 推測會殘留（程式碼層面確認無任何防護；未在實機重現）。建議用 Job Object（`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`）或比照 vz 的 stdin-EOF 做法。
