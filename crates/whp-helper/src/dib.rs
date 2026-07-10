//! 剪貼簿圖片的 CF_DIB ↔ PNG 轉換（純邏輯，無 Win32 API）。
//!
//! CF_DIB 是 Windows 傳統的剪貼簿圖片格式（小畫家/剪取工具/PowerShell SetImage、Office
//! 皆用）：一段 **packed DIB**（BITMAPINFOHEADER + 選用遮罩/調色盤 + pixel array，**無**
//! BITMAPFILEHEADER）。現代 app（瀏覽器等）另提供註冊的 `"PNG"` 剪貼簿格式。為與兩者
//! 互通，圖片在 host↔guest 線協定上一律用 PNG：host 讀時把 CF_DIB 轉成 PNG、寫時同時放
//! `"PNG"` 與 CF_DIB。此模組只做位元組層轉換 + `png` codec，可跨平台單元測試。

fn u32_le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn i32_le(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn u16_le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;

/// 把 packed CF_DIB 轉成 PNG bytes。僅支援常見情形（24/32-bit truecolor、BI_RGB 或
/// BI_BITFIELDS、BGRA byte 序）；其他格式回 `None`（該次圖片不同步，非致命）。
pub fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let hdr_size = u32_le(dib, 0) as usize;
    if hdr_size < 40 || hdr_size > dib.len() {
        return None;
    }
    let width_raw = i32_le(dib, 4);
    let height_raw = i32_le(dib, 8);
    let bit_count = u16_le(dib, 14);
    let compression = u32_le(dib, 16);
    let clr_used = u32_le(dib, 32) as usize;
    if width_raw <= 0 || height_raw == 0 {
        return None;
    }
    if bit_count != 24 && bit_count != 32 {
        return None;
    }
    if compression != BI_RGB && compression != BI_BITFIELDS {
        return None;
    }
    let top_down = height_raw < 0;
    let width = width_raw as usize;
    let height = height_raw.unsigned_abs() as usize;

    // pixel array 起點 = header + (BI_BITFIELDS 的 3 個遮罩 = 12 bytes) + 選用調色盤。
    let mut off = hdr_size;
    if compression == BI_BITFIELDS {
        off = off.checked_add(12)?;
    }
    off = off.checked_add(clr_used.checked_mul(4)?)?;

    let bytes_per_px = (bit_count / 8) as usize;
    // DIB 每列對齊 4 bytes。
    let stride = width
        .checked_mul(bytes_per_px)?
        .div_ceil(4)
        .checked_mul(4)?;
    let needed = off.checked_add(stride.checked_mul(height)?)?;
    if dib.len() < needed {
        return None;
    }

    let mut rgba = vec![0u8; width.checked_mul(height)?.checked_mul(4)?];
    for y in 0..height {
        // DIB row：top_down → 檔中第 y 列；否則 bottom-up → 檔中第 height-1-y 列。
        let src_row = if top_down { y } else { height - 1 - y };
        let row_start = off + src_row * stride;
        let row = &dib[row_start..row_start + width * bytes_per_px];
        for x in 0..width {
            let p = &row[x * bytes_per_px..x * bytes_per_px + bytes_per_px];
            // DIB 為 BGR(A) 序。
            let (b, g, r) = (p[0], p[1], p[2]);
            // alpha：BI_BITFIELDS 32-bit 才採第 4 byte；BI_RGB 的 alpha 未定義 → 不透明。
            let a = if bytes_per_px == 4 && compression == BI_BITFIELDS {
                p[3]
            } else {
                255
            };
            let d = &mut rgba[(y * width + x) * 4..];
            d[0] = r;
            d[1] = g;
            d[2] = b;
            d[3] = a;
        }
    }
    encode_png(width as u32, height as u32, &rgba)
}

/// 把 PNG 解成 32-bit bottom-up BGRA 的 packed CF_DIB（BI_RGB）。
pub fn png_to_dib(png: &[u8]) -> Option<Vec<u8>> {
    let (width, height, rgba) = decode_png(png)?;
    let w = width as usize;
    let h = height as usize;
    let stride = w.checked_mul(4)?; // 32-bit → 已對齊 4
    let mut out = Vec::with_capacity(40 + stride * h);
    // BITMAPINFOHEADER（BI_RGB、32bpp、bottom-up）。
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&(h as i32).to_le_bytes()); // biHeight（正 = bottom-up）
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&BI_RGB.to_le_bytes()); // biCompression
    out.extend_from_slice(&((stride * h) as u32).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
    // pixel array：bottom-up、BGRA。
    for y in (0..h).rev() {
        let row = &rgba[y * stride..y * stride + stride];
        for x in 0..w {
            let p = &row[x * 4..x * 4 + 4];
            out.push(p[2]); // B
            out.push(p[1]); // G
            out.push(p[0]); // R
            out.push(p[3]); // A
        }
    }
    Some(out)
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    if rgba.len() != (width as usize) * (height as usize) * 4 {
        return None;
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(out)
}

/// 解 PNG → (width, height, RGBA8 top-down)。展開調色盤/低位深、16-bit 降 8-bit，
/// 各色型正規化成 RGBA。
fn decode_png(png_bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    // png 0.18 起 Decoder 要求 Read + Seek（支援 APNG/streaming）；&[u8] 只有 Read，
    // 以 Cursor 補上 Seek。
    let mut dec = png::Decoder::new(std::io::Cursor::new(png_bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().ok()?;
    // png 0.18：output_buffer_size 回 Option（尺寸溢位時 None）。
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let data = &buf[..info.buffer_size()];
    let w = info.width;
    let h = info.height;
    let px = (w as usize).checked_mul(h as usize)?;
    let mut rgba = vec![0u8; px.checked_mul(4)?];
    match info.color_type {
        png::ColorType::Rgba => {
            if data.len() < px * 4 {
                return None;
            }
            rgba.copy_from_slice(&data[..px * 4]);
        }
        png::ColorType::Rgb => {
            if data.len() < px * 3 {
                return None;
            }
            for i in 0..px {
                rgba[i * 4] = data[i * 3];
                rgba[i * 4 + 1] = data[i * 3 + 1];
                rgba[i * 4 + 2] = data[i * 3 + 2];
                rgba[i * 4 + 3] = 255;
            }
        }
        png::ColorType::GrayscaleAlpha => {
            if data.len() < px * 2 {
                return None;
            }
            for i in 0..px {
                let g = data[i * 2];
                rgba[i * 4] = g;
                rgba[i * 4 + 1] = g;
                rgba[i * 4 + 2] = g;
                rgba[i * 4 + 3] = data[i * 2 + 1];
            }
        }
        png::ColorType::Grayscale => {
            if data.len() < px {
                return None;
            }
            for i in 0..px {
                let g = data[i];
                rgba[i * 4] = g;
                rgba[i * 4 + 1] = g;
                rgba[i * 4 + 2] = g;
                rgba[i * 4 + 3] = 255;
            }
        }
        // Indexed 應已被 EXPAND 展開為 Rgb。
        _ => return None,
    }
    Some((w, h, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_dib_roundtrip_opaque() {
        // 2x2 不透明 RGBA（alpha=255；CF_DIB BI_RGB 不帶 alpha，故用不透明才可精確往返）。
        let rgba: Vec<u8> = vec![
            255, 0, 0, 255, // (0,0) red
            0, 255, 0, 255, // (1,0) green
            0, 0, 255, 255, // (0,1) blue
            255, 255, 0, 255, // (1,1) yellow
        ];
        let png = encode_png(2, 2, &rgba).unwrap();
        let dib = png_to_dib(&png).unwrap();
        let png2 = dib_to_png(&dib).unwrap();
        let (w, h, rgba2) = decode_png(&png2).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(rgba2, rgba);
    }

    #[test]
    fn dib24_bottom_up_orientation_and_bgr() {
        // width=1, height=2, 24-bit bottom-up：檔中第 0 列=最底列、第 1 列=最頂列。
        // 每列 3 bytes → 對齊 4 → stride 4（尾補 1 byte）。
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
        dib.extend_from_slice(&1i32.to_le_bytes()); // width
        dib.extend_from_slice(&2i32.to_le_bytes()); // height（正 → bottom-up）
        dib.extend_from_slice(&1u16.to_le_bytes()); // planes
        dib.extend_from_slice(&24u16.to_le_bytes()); // bitcount
        dib.extend_from_slice(&BI_RGB.to_le_bytes()); // compression
        dib.extend_from_slice(&0u32.to_le_bytes()); // sizeImage
        dib.extend_from_slice(&0i32.to_le_bytes());
        dib.extend_from_slice(&0i32.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        dib.extend_from_slice(&0u32.to_le_bytes());
        // 檔第 0 列（bottom）= 藍：BGR = (255,0,0) + 1 padding。
        dib.extend_from_slice(&[255, 0, 0, 0]);
        // 檔第 1 列（top）= 紅：BGR = (0,0,255) + 1 padding。
        dib.extend_from_slice(&[0, 0, 255, 0]);

        let png = dib_to_png(&dib).unwrap();
        let (w, h, rgba) = decode_png(&png).unwrap();
        assert_eq!((w, h), (1, 2));
        // top-down RGBA：第 0 列（top）= 紅。
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255]);
        // 第 1 列（bottom）= 藍。
        assert_eq!(&rgba[4..8], &[0, 0, 255, 255]);
    }

    #[test]
    fn png_to_dib_header_shape() {
        let rgba = vec![10, 20, 30, 255, 40, 50, 60, 255]; // 2x1
        let png = encode_png(2, 1, &rgba).unwrap();
        let dib = png_to_dib(&png).unwrap();
        assert_eq!(u32_le(&dib, 0), 40); // biSize
        assert_eq!(i32_le(&dib, 4), 2); // width
        assert_eq!(i32_le(&dib, 8), 1); // height（bottom-up）
        assert_eq!(u16_le(&dib, 14), 32); // bitcount
        assert_eq!(u32_le(&dib, 16), BI_RGB);
        // 第一顆（bottom-up 唯一列，x=0）BGRA = (30,20,10,255)。
        assert_eq!(&dib[40..44], &[30, 20, 10, 255]);
    }

    #[test]
    fn dib_rejects_unsupported() {
        // 太短。
        assert!(dib_to_png(&[0u8; 10]).is_none());
        // 8-bit（調色盤）不支援。
        let mut dib = vec![0u8; 40];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&1i32.to_le_bytes());
        dib[8..12].copy_from_slice(&1i32.to_le_bytes());
        dib[14..16].copy_from_slice(&8u16.to_le_bytes());
        assert!(dib_to_png(&dib).is_none());
    }
}
