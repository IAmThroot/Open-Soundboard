//! Generates `assets/icon.ico` and `assets/icon.png` from `src/icon_art.rs`.
//!
//! Run (from the repository root) after changing the artwork:
//!
//! ```text
//! rustc -O tools/gen_icon.rs -o /tmp/gen_icon && /tmp/gen_icon assets
//! ```
//!
//! `rustc` alone works because `icon_art.rs` is dependency-free and is
//! `include!`d. The generated files are committed, so normal builds never run
//! this — it is a design tool, not a build step.

#[path = "../src/icon_art.rs"]
mod icon_art;

use icon_art::draw;

/// Icon sizes to embed in the .ico (16px is the Windows taskbar size).
const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

fn main() {
    let out_dir = std::env::args().nth(1).unwrap_or_else(|| "assets".to_owned());
    std::fs::create_dir_all(&out_dir).expect("create assets dir");

    let ico = build_ico(&SIZES);
    let ico_path = format!("{out_dir}/icon.ico");
    std::fs::write(&ico_path, &ico).expect("write icon.ico");
    println!("{ico_path}: {} bytes", ico.len());

    let png = build_png(&draw(256), 256);
    let png_path = format!("{out_dir}/icon.png");
    std::fs::write(&png_path, &png).expect("write icon.png");
    println!("{png_path}: {} bytes", png.len());
}

// --- ICO ------------------------------------------------------------------

/// ICO with one 32-bit BGRA DIB per size (alpha carried in the DIB, AND
/// mask present but empty, as Windows expects).
fn build_ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| dib_image(s)).collect();

    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());

    // Directory entries first, so offsets are known up front.
    let mut offset = 6 + 16 * sizes.len();
    for (index, &size) in sizes.iter().enumerate() {
        let dim = if size >= 256 { 0u8 } else { size as u8 };
        out.push(dim);
        out.push(dim);
        out.push(0); // palette colors (0 = truecolor)
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // color planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(images[index].len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += images[index].len();
    }
    for image in images {
        out.extend_from_slice(&image);
    }
    out
}

/// 32-bit BGRA DIB for one size: BITMAPINFOHEADER + bottom-up XOR bitmap +
/// (empty) 1bpp AND mask.
fn dib_image(size: u32) -> Vec<u8> {
    let w = size as usize;
    let xor_len = w * w * 4;
    // 1bpp mask: each row padded to a 4-byte boundary.
    let mask_row = w.div_ceil(32) * 4;
    let and_len = mask_row * w;

    let mut out = Vec::with_capacity(40 + xor_len + and_len);
    // BITMAPINFOHEADER
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&((w as i32) * 2).to_le_bytes()); // biHeight (XOR + AND)
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&(xor_len as u32).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&0u32.to_le_bytes()); // ppm x
    out.extend_from_slice(&0u32.to_le_bytes()); // ppm y
    out.extend_from_slice(&0u32.to_le_bytes()); // colors used
    out.extend_from_slice(&0u32.to_le_bytes()); // important colors

    let rgba = draw(size);
    // Bottom-up rows, BGRA order.
    for row in (0..w).rev() {
        for x in 0..w {
            let i = (row * w + x) * 4;
            out.push(rgba[i + 2]);
            out.push(rgba[i + 1]);
            out.push(rgba[i]);
            out.push(rgba[i + 3]);
        }
    }
    out.extend(std::iter::repeat_n(0u8, and_len));
    out
}

// --- PNG ------------------------------------------------------------------

/// Minimal RGBA PNG using uncompressed deflate blocks (no zlib dependency).
fn build_png(rgba: &[u8], size: u32) -> Vec<u8> {
    let mut raw = Vec::with_capacity(size as usize * (1 + size as usize * 4));
    for row in 0..size as usize {
        raw.push(0); // filter: none
        let start = row * size as usize * 4;
        raw.extend_from_slice(&rgba[start..start + size as usize * 4]);
    }

    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&size.to_be_bytes());
    ihdr.extend_from_slice(&size.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, no interlace
    write_chunk(&mut out, b"IHDR", &ihdr);
    write_chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    write_chunk(&mut out, b"IEND", &[]);
    out
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

/// zlib stream with stored (uncompressed) deflate blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01]; // deflate, 32k window, no dict
    let mut offset = 0;
    while offset < data.len() || offset == 0 {
        let take = (data.len() - offset).min(65_535);
        let last = offset + take >= data.len();
        out.push(if last { 1 } else { 0 }); // BFINAL + BTYPE=00
        out.extend_from_slice(&(take as u16).to_le_bytes());
        out.extend_from_slice(&(!(take as u16)).to_le_bytes());
        out.extend_from_slice(&data[offset..offset + take]);
        offset += take;
        if last {
            break;
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}