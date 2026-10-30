//! 索引色 PNG 编码（256 色调色板）。
//!
//! 报告类长图（1080×2400+，大量抗锯齿文字）用真彩 PNG 要 600KB 上下，
//! QQ 富媒体接口回源拉取会超时（实测 590KB → `40034003`）。
//! 这类图的实际用色很少（底色 + 几档文字灰 + 强调色 + 一个 icon），
//! 量化到 256 色后体积约为真彩的 1/3（~180KB），肉眼几乎无差别。
//!
//! `image` crate 的 PNG 编码器只写真彩，所以这里直接用 `png` crate 写 PLTE。

use image::RgbImage;
use std::collections::HashMap;

/// 把 RGB 长图量化成 256 色调色板 PNG；失败时返回 None（调用方回退真彩编码）。
pub fn encode_indexed_png(img: &RgbImage) -> Option<Vec<u8>> {
    let (w, h) = (img.width(), img.height());
    let raw = img.as_raw();
    if w == 0 || h == 0 || raw.len() < (w as usize * h as usize) * 3 {
        return None;
    }

    // 1) 直方图：每通道取高 6 bit（18 bit key，必须用 u32 —— 拿 u16 装会把 R 通道溢出，
    //    实测后果是浅色背景被量化成青色）
    let mut hist: HashMap<u32, u32> = HashMap::with_capacity(1 << 16);
    for px in raw.chunks_exact(3) {
        *hist.entry(bucket(px)).or_insert(0) += 1;
    }

    // 2) 取出现最多的 255 个桶当调色板（用桶中心色，减少整体偏色）
    let mut entries: Vec<(u32, u32)> = hist.into_iter().collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1));
    entries.truncate(255);
    if entries.is_empty() {
        return None;
    }
    let palette: Vec<[u8; 3]> = entries
        .iter()
        .map(|(k, _)| {
            [
                (((k >> 12) & 0x3f) as u8) << 2 | 2,
                (((k >> 6) & 0x3f) as u8) << 2 | 2,
                ((k & 0x3f) as u8) << 2 | 2,
            ]
        })
        .collect();

    // 3) 逐像素映射到最近的调色板色（同桶只算一次，缓存住）
    let mut cache: HashMap<u32, u8> = HashMap::with_capacity(entries.len() * 4);
    let mut idx = Vec::with_capacity((w * h) as usize);
    for px in raw.chunks_exact(3) {
        let k = bucket(px);
        let i = match cache.get(&k) {
            Some(v) => *v,
            None => {
                let mut best = 0usize;
                let mut best_d = u32::MAX;
                for (i, p) in palette.iter().enumerate() {
                    let dr = px[0] as i32 - p[0] as i32;
                    let dg = px[1] as i32 - p[1] as i32;
                    let db = px[2] as i32 - p[2] as i32;
                    let d = (dr * dr + dg * dg + db * db) as u32;
                    if d < best_d {
                        best_d = d;
                        best = i;
                        if d == 0 {
                            break;
                        }
                    }
                }
                let i = best as u8;
                cache.insert(k, i);
                i
            }
        };
        idx.push(i);
    }

    // 4) 写索引色 PNG
    let mut out: Vec<u8> = Vec::new();
    {
        let mut enc = png::Encoder::new(std::io::Cursor::new(&mut out), w, h);
        enc.set_color(png::ColorType::Indexed);
        enc.set_depth(png::BitDepth::Eight);
        let mut plte: Vec<u8> = Vec::with_capacity(palette.len() * 3);
        for p in &palette {
            plte.extend_from_slice(p);
        }
        enc.set_palette(plte);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(&idx).ok()?;
        writer.finish().ok()?;
    }
    if out.len() > 8 {
        Some(out)
    } else {
        None
    }
}

#[inline]
fn bucket(px: &[u8]) -> u32 {
    ((px[0] as u32 >> 2) << 12) | ((px[1] as u32 >> 2) << 6) | (px[2] as u32 >> 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 索引色编码_能解码且色彩接近() {
        let mut img = RgbImage::new(320, 200);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([
                ((x * 255 / 320) as u8),
                ((y * 255 / 200) as u8),
                if (x / 16 + y / 16) % 2 == 0 { 240 } else { 30 },
            ]);
        }
        let bytes = encode_indexed_png(&img).expect("编码");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let decoded = image::load_from_memory(&bytes).expect("解码").to_rgb8();
        assert_eq!(decoded.dimensions(), (320, 200));
        // 渐变图（6.4 万种颜色）挤进 255 色：单点误差可能较大，但平均误差必须很小
        let mut worst = 0i32;
        let mut sum = 0i64;
        let mut n = 0i64;
        for (a, b) in img.pixels().zip(decoded.pixels()) {
            for c in 0..3 {
                let d = (a[c] as i32 - b[c] as i32).abs();
                worst = worst.max(d);
                sum += d as i64;
                n += 1;
            }
        }
        let mean = sum as f64 / n as f64;
        println!("渐变图量化：平均误差 {mean:.2}，最大误差 {worst}");
        assert!(mean < 6.0, "平均误差 {mean} 偏大");
        assert!(worst <= 48, "最大通道偏差 {worst}");
    }

    /// 真实排版图（少量色 + 抗锯齿灰阶）：量化后应与原图几乎一致
    #[test]
    fn 索引色编码_扁平设计图近似无损() {
        let mut img = RgbImage::from_pixel(400, 300, image::Rgb([252, 253, 255]));
        for (x, y, p) in img.enumerate_pixels_mut() {
            if x > 20 && x < 380 && y > 20 && y < 120 {
                *p = image::Rgb([246, 249, 253]);
            }
            if (x / 3 + y / 3) % 7 == 0 && x > 30 && y > 140 {
                // 模拟抗锯齿文字灰阶
                let v = 60 + ((x * 3 + y) % 40) as u8;
                *p = image::Rgb([v, v, v]);
            }
            if x % 97 == 0 {
                *p = image::Rgb([47, 111, 237]);
            }
        }
        let bytes = encode_indexed_png(&img).expect("编码");
        let decoded = image::load_from_memory(&bytes).expect("解码").to_rgb8();
        let mut worst = 0i32;
        for (a, b) in img.pixels().zip(decoded.pixels()) {
            for c in 0..3 {
                worst = worst.max((a[c] as i32 - b[c] as i32).abs());
            }
        }
        assert!(worst <= 8, "最大通道偏差 {worst}");
    }

    #[test]
    fn 索引色编码_空图返回None() {
        let img = RgbImage::new(0, 0);
        assert!(encode_indexed_png(&img).is_none());
    }
}
