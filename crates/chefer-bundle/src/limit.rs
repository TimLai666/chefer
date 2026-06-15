//! 解壓位元組預算限制器：包住「解壓後」的 reader，累計已讀 bytes，
//! 超過上限即回 `io::Error`。用於防解壓炸彈（zstd 對零的壓縮比可達上千倍，
//! 幾 MB 的惡意 payload 可膨脹成數 GB）。
//!
//! 關鍵：tar 在迭代過程中遇到 GNU 長檔名（'L'）/pax 擴充標頭時，會在把
//! entry 交給呼叫端之前就 `read_to_end` 把「宣告大小」的內容讀進記憶體；
//! 對單一 entry 的事後大小檢查擋不到這種「迭代器內部」的無上限讀取。
//! 把限制器套在 tar 之前（zstd 解碼器之後），即可同時擋下：
//! 1) 長檔名/pax 標頭撐爆 read_to_end 的 OOM；
//! 2) 把目標 entry 排在多個超大 entry 之後、迭代時逐一讀取-丟棄的 CPU/記憶體 DoS；
//! 3) 任何未被單一 entry 上限覆蓋的解壓放大。

use std::io::{self, Read};

/// 解壓炸彈偵測的壓縮比上限：解壓後輸出超過「壓縮輸入 × 此倍數」即視為炸彈。
/// 真實容器資料的 zstd 壓縮比一般 ≤ 5；取 200 作為極寬鬆的安全邊界。
pub const BOMB_RATIO: u64 = 200;
/// 比率上限的下限地板：即使壓縮輸入很小，至少允許解到此大小
/// （涵蓋小 bundle 的合法內容）。
pub const BOMB_FLOOR_BYTES: u64 = 512 * 1024 * 1024;

/// 由壓縮 payload 長度推算合理的解壓輸出上限（防炸彈用）。
pub fn bomb_limit_for(compressed_len: u64) -> u64 {
    compressed_len
        .saturating_mul(BOMB_RATIO)
        .max(BOMB_FLOOR_BYTES)
}

/// 包住一個 reader，限制總共可讀出的 bytes；超過即回 `InvalidData` 錯誤。
/// 「剛好讀到上限後底層 EOF」不會誤報（只有確實還有更多資料時才報錯）。
pub struct LimitedReader<R> {
    inner: R,
    remaining: u64,
    limit: u64,
}

impl<R: Read> LimitedReader<R> {
    pub fn new(inner: R, limit: u64) -> Self {
        LimitedReader {
            inner,
            remaining: limit,
            limit,
        }
    }
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            // 已達上限：探測底層是否還有資料。EOF → 正常結束；有資料 → 超限報錯。
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "decompressed content exceeds the limit of {} bytes (possibly a malicious decompression bomb or a corrupted file)",
                        self.limit
                    ),
                )),
            };
        }
        let cap = buf.len().min(self.remaining as usize);
        let n = self.inner.read(&mut buf[..cap])?;
        self.remaining -= n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_within_limit() {
        let data = [1u8; 100];
        let mut r = LimitedReader::new(&data[..], 200);
        let mut out = Vec::new();
        assert_eq!(r.read_to_end(&mut out).unwrap(), 100);
        assert_eq!(out.len(), 100);
    }

    #[test]
    fn errors_past_limit() {
        let data = vec![1u8; 1000];
        let mut r = LimitedReader::new(&data[..], 256);
        let mut out = Vec::new();
        let err = r.read_to_end(&mut out).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(out.len() <= 256);
    }

    #[test]
    fn exact_limit_then_eof_is_ok() {
        let data = vec![1u8; 256];
        let mut r = LimitedReader::new(&data[..], 256);
        let mut out = Vec::new();
        // 剛好讀完 256 bytes 後底層 EOF，不應誤報超限
        assert_eq!(r.read_to_end(&mut out).unwrap(), 256);
        assert_eq!(out.len(), 256);
    }

    #[test]
    fn bomb_limit_uses_ratio_above_floor() {
        // 大 payload：用比率
        assert_eq!(
            bomb_limit_for(100 * 1024 * 1024),
            100 * 1024 * 1024 * BOMB_RATIO
        );
        // 小 payload：用地板
        assert_eq!(bomb_limit_for(1024), BOMB_FLOOR_BYTES);
    }
}
