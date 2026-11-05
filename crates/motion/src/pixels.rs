//! Pixels in memory, and the one resize everything here uses: an exact area average in linear light, the arithmetic upstream runs so
//! that fine detail neither aliases nor darkens.

use std::sync::OnceLock;

/// RGBA, row-major, not premultiplied.
#[derive(Clone)]
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

fn srgb_to_linear_table() -> &'static [f32; 256] {
    static TABLE: OnceLock<[f32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [0.0f32; 256];
        for (value, slot) in table.iter_mut().enumerate() {
            let channel = value as f64 / 255.0;
            *slot = (if channel <= 0.04045 { channel / 12.92 } else { ((channel + 0.055) / 1.055).powf(2.4) }) as f32;
        }
        table
    })
}

const LINEAR_STEPS: usize = 16_383;

fn linear_to_srgb_table() -> &'static [u8] {
    static TABLE: OnceLock<Vec<u8>> = OnceLock::new();
    TABLE.get_or_init(|| {
        (0..=LINEAR_STEPS)
            .map(|step| {
                let linear = step as f64 / LINEAR_STEPS as f64;
                let channel = if linear <= 0.0031308 { linear * 12.92 } else { 1.055 * linear.powf(1.0 / 2.4) - 0.055 };
                (channel.clamp(0.0, 1.0) * 255.0).round() as u8
            })
            .collect()
    })
}

/// For each output cell along one axis, the source cells it covers and how much of each. Weights of one cell sum to 1.
fn coverage(source: u32, target: u32) -> Vec<Vec<(u32, f32)>> {
    let scale = source as f64 / target as f64;
    (0..target)
        .map(|cell| {
            let start = cell as f64 * scale;
            let end = (((cell + 1) as f64) * scale).min(source as f64);
            let from = start.floor() as u32;
            let to = end.ceil().min(source as f64) as u32;
            (from..to).map(|i| (i, (((i + 1) as f64).min(end) - (i as f64).max(start)) as f32 / scale as f32)).collect()
        })
        .collect()
}

/// Shrinks `source` by averaging exactly the source area under each output pixel, on linear-light, alpha-premultiplied
/// values: a fine black-and-white pattern becomes the grey it looks like, and a transparent pixel's hidden colour does
/// not bleed into its neighbours.
pub fn area_resize(source: &Pixels, width: u32, height: u32) -> Result<Pixels, String> {
    if width > source.width || height > source.height {
        return Err("areaResize only shrinks".to_owned());
    }
    let to_linear = srgb_to_linear_table();
    let to_srgb = linear_to_srgb_table();
    let columns = coverage(source.width, width);
    let rows = coverage(source.height, height);
    let (w, sw) = (width as usize, source.width as usize);
    let mut output = vec![0u8; w * height as usize * 4];
    let mut accumulator = vec![0.0f32; w * 4];
    let mut linear_row = vec![0.0f32; sw * 4];
    let mut filtered_row = vec![0.0f32; w * 4];
    let mut filtered_index: i64 = -1;
    for (y, row) in rows.iter().enumerate() {
        accumulator.fill(0.0);
        for &(source_row, row_weight) in row {
            if filtered_index != source_row as i64 {
                let offset = source_row as usize * sw * 4;
                for x in 0..sw {
                    let pixel = &source.data[offset + x * 4..offset + x * 4 + 4];
                    let alpha = pixel[3] as f32 / 255.0;
                    linear_row[x * 4] = to_linear[pixel[0] as usize] * alpha;
                    linear_row[x * 4 + 1] = to_linear[pixel[1] as usize] * alpha;
                    linear_row[x * 4 + 2] = to_linear[pixel[2] as usize] * alpha;
                    linear_row[x * 4 + 3] = alpha;
                }
                for (cell, column) in columns.iter().enumerate() {
                    let mut sum = [0.0f32; 4];
                    for &(at, weight) in column {
                        for (c, s) in sum.iter_mut().enumerate() {
                            *s += linear_row[at as usize * 4 + c] * weight;
                        }
                    }
                    filtered_row[cell * 4..cell * 4 + 4].copy_from_slice(&sum);
                }
                filtered_index = source_row as i64;
            }
            for (a, f) in accumulator.iter_mut().zip(filtered_row.iter()) {
                *a += f * row_weight;
            }
        }
        let offset = y * w * 4;
        for x in 0..w {
            let alpha = accumulator[x * 4 + 3];
            if alpha <= 0.0 {
                continue;
            }
            let k = LINEAR_STEPS as f32 / alpha;
            for c in 0..3 {
                output[offset + x * 4 + c] = to_srgb[((accumulator[x * 4 + c] * k).round() as usize).min(LINEAR_STEPS)];
            }
            output[offset + x * 4 + 3] = (alpha.min(1.0) * 255.0).round() as u8;
        }
    }
    Ok(Pixels { width, height, data: output })
}
