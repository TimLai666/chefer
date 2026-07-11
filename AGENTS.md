@CLAUDE.md

## Follow-ups

- **whp 防孤兒修復待實機驗證**：Job Object（KILL_ON_JOB_CLOSE）+ helper stdin-EOF 自我了結（`crates/vmm-backend/src/whp.rs` + `crates/whp-helper/src/main.rs`）已完成、機制各有 CI 單元測試，但「單殺 runtime 後 helper/VM 不殘留」的端到端行為需實體 Windows（WHP）驗證：以 `CHEFER_BACKEND=whp` 跑一個 bundle → `taskkill /F /PID <runtime pid>`（TerminateProcess，孤兒化的可靠重現法；原始問題本身也尚未在實機重現過）→ 確認 `chefer-whp-helper` 行程數秒內消失、無殘留。通過後刪除本項。vz 分支（`claude/inspiring-chaum-208d84`）AGENTS.md 的「Windows whp-helper 疑有同款孤兒化問題」follow-up 由本修復解決，兩邊合併時一併刪除該項。
