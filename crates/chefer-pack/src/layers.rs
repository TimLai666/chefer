//! 層 blob 的串流重壓：解壓（gzip / zstd / 未壓縮）→ 同步計算 diff_id → zstd 重壓。
//!
//! 全程串流，不把整層讀進記憶體（層可能數 GB）。

use std::io::{BufWriter, Cursor, Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use fs_err as fs;
use sha2::{Digest, Sha256};

/// 以 magic bytes 偵測的壓縮格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compression {
    Gzip,
    Zstd,
    Plain,
}

/// gzip: 1f 8b；zstd: 28 b5 2f fd；其他視為未壓縮 tar。
pub(crate) fn detect_compression(magic: &[u8]) -> Compression {
    if magic.len() >= 2 && magic[0] == 0x1f && magic[1] == 0x8b {
        Compression::Gzip
    } else if magic.len() >= 4 && magic[..4] == [0x28, 0xb5, 0x2f, 0xfd] {
        Compression::Zstd
    } else {
        Compression::Plain
    }
}

/// 重壓完成的層資訊。
pub(crate) struct RepackedLayer {
    /// 未壓縮 layer tar 的 sha256（`sha256:<64hex>`）。
    pub diff_id: String,
    /// 寫入 layers 目錄的檔名（`{idx:04}-{diffid12}.tar.zst`）。
    pub file_name: String,
    /// zstd 壓縮後檔案大小（bytes）。
    pub size: u64,
}

/// 將單一層 blob 串流解壓、計算 diff_id、zstd 重壓寫入 `layers_dir`。
///
/// 先寫到暫存檔名，算出 diff_id 後再改名為正式檔名
/// （`chefer_bundle::layout::layer_file_name`）。
pub(crate) fn repack_layer(
    blob: &Path,
    layers_dir: &Path,
    idx: usize,
    zstd_level: i32,
) -> Result<RepackedLayer> {
    let mut input =
        fs::File::open(blob).with_context(|| format!("開啟層 blob 失敗：{}", blob.display()))?;

    // 讀 magic bytes 偵測壓縮格式，之後把已讀的位元組接回串流前端。
    let mut magic = [0u8; 4];
    let mut filled = 0usize;
    while filled < magic.len() {
        let n = input.read(&mut magic[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    let head = Cursor::new(magic[..filled].to_vec());
    let chained = head.chain(input);

    let mut decoded: Box<dyn Read> = match detect_compression(&magic[..filled]) {
        Compression::Gzip => Box::new(GzDecoder::new(chained)),
        Compression::Zstd => {
            Box::new(zstd::stream::read::Decoder::new(chained).context("初始化 zstd 解壓器失敗")?)
        }
        Compression::Plain => Box::new(chained),
    };

    // 先寫暫存檔（檔名需要 diff_id，必須讀完整層才知道）。
    let tmp_path = layers_dir.join(format!(".tmp-{idx:04}.tar.zst"));
    let out = fs::File::create(&tmp_path)
        .with_context(|| format!("建立層輸出檔失敗：{}", tmp_path.display()))?;
    let mut encoder = zstd::stream::write::Encoder::new(BufWriter::new(out), zstd_level)
        .context("初始化 zstd 壓縮器失敗")?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = decoded
            .read(&mut buf)
            .with_context(|| format!("解壓層 blob 失敗：{}", blob.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        encoder.write_all(&buf[..n])?;
    }
    let mut writer = encoder.finish().context("結束 zstd 壓縮失敗")?;
    writer.flush()?;
    drop(writer);

    let diff_id = format!("sha256:{}", to_hex(&hasher.finalize()));
    let file_name = chefer_bundle::layout::layer_file_name(idx, &diff_id);
    let final_path = layers_dir.join(&file_name);
    if final_path.exists() {
        fs::remove_file(&final_path)?;
    }
    fs::rename(&tmp_path, &final_path)?;
    let size = fs::metadata(&final_path)?.len();

    Ok(RepackedLayer {
        diff_id,
        file_name,
        size,
    })
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_by_magic() {
        assert_eq!(
            detect_compression(&[0x1f, 0x8b, 0x08, 0x00]),
            Compression::Gzip
        );
        assert_eq!(
            detect_compression(&[0x28, 0xb5, 0x2f, 0xfd]),
            Compression::Zstd
        );
        assert_eq!(detect_compression(b"ustar"), Compression::Plain);
        assert_eq!(detect_compression(&[]), Compression::Plain);
        assert_eq!(detect_compression(&[0x1f]), Compression::Plain);
    }

    #[test]
    fn repack_plain_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let data = b"hello chefer layer".repeat(1000);
        let blob = tmp.path().join("layer.tar");
        std::fs::write(&blob, &data).unwrap();

        let layers_dir = tmp.path().join("layers");
        std::fs::create_dir_all(&layers_dir).unwrap();
        let r = repack_layer(&blob, &layers_dir, 0, 3).unwrap();

        let mut hasher = Sha256::new();
        hasher.update(&data);
        let expected = format!("sha256:{}", to_hex(&hasher.finalize()));
        assert_eq!(r.diff_id, expected);

        // zstd 解回後內容一致
        let out = layers_dir.join(&r.file_name);
        let decoded = zstd::stream::decode_all(std::fs::File::open(&out).unwrap()).unwrap();
        assert_eq!(decoded, data);
        assert_eq!(r.size, std::fs::metadata(&out).unwrap().len());
    }

    #[test]
    fn repack_gzip_input() {
        use flate2::Compression as GzLevel;
        use flate2::write::GzEncoder;

        let tmp = tempfile::tempdir().unwrap();
        let data = b"gzip layer content".repeat(500);
        let mut enc = GzEncoder::new(Vec::new(), GzLevel::default());
        enc.write_all(&data).unwrap();
        let gz = enc.finish().unwrap();

        let blob = tmp.path().join("layer.tar.gz");
        std::fs::write(&blob, &gz).unwrap();
        let layers_dir = tmp.path().join("layers");
        std::fs::create_dir_all(&layers_dir).unwrap();

        let r = repack_layer(&blob, &layers_dir, 1, 3).unwrap();
        // diff_id 必須是「未壓縮內容」的 sha256
        let mut hasher = Sha256::new();
        hasher.update(&data);
        assert_eq!(r.diff_id, format!("sha256:{}", to_hex(&hasher.finalize())));

        let decoded =
            zstd::stream::decode_all(std::fs::File::open(layers_dir.join(&r.file_name)).unwrap())
                .unwrap();
        assert_eq!(decoded, data);
    }
}
