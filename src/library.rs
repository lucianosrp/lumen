use std::collections::HashSet;

use image::{DynamicImage, RgbaImage, imageops::FilterType};
use serde::{Deserialize, Serialize};

/// One photo or video, as much as the grid needs to know about it.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
pub struct Asset {
    pub id: String,
    /// ISO-8601 capture time, e.g. "2024-03-15T10:22:01.000Z".
    pub taken: String,
    pub ratio: f32,
    pub video: bool,
    /// Base64 thumbhash, used as the instant placeholder.
    pub thumbhash: Option<String>,
    /// Live Photo: the id of its motion clip (itself a hidden video asset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_video: Option<String>,
    /// Video length in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u32>,
    /// Older disk caches predate this field.
    #[serde(default)]
    pub favorite: bool,
}

impl Asset {
    pub fn year_month(&self) -> Option<(u32, u32)> {
        let y = self.taken.get(0..4)?.parse().ok()?;
        let m = self.taken.get(5..7)?.parse().ok()?;
        Some((y, m))
    }
}

/// All thumbhashes decoded into one mipmapped texture, `cell`×`cell` pixels per asset.
pub struct ThumbhashAtlas {
    pub cell: u32,
    pub cols: u32,
    pub width: u32,
    pub height: u32,
    /// RGBA8 sRGB, one entry per mip level.
    pub levels: Vec<Vec<u8>>,
}

pub const ATLAS_WIDTH: u32 = 8192;

/// Oldest first, like Apple Photos (newest sits at the bottom of the grid).
pub struct Library {
    pub assets: Vec<Asset>,
    pub thumbhashes: ThumbhashAtlas,
    pub videos: usize,
}

impl Library {
    pub fn new(mut assets: Vec<Asset>) -> Self {
        assets.sort_by(|a, b| a.taken.cmp(&b.taken));
        let thumbhashes = build_thumbhash_atlas(&assets);
        let videos = assets.iter().filter(|a| a.video).count();
        Self { assets, thumbhashes, videos }
    }

    pub fn len(&self) -> usize {
        self.assets.len()
    }

    /// A copy without `remove` and with `add` merged in by date, reusing the decoded
    /// placeholders, so a local edit (trash, archive, undo…) costs milliseconds rather than a
    /// rebuild. Also returns where each old index went.
    pub fn edit(&self, remove: &HashSet<String>, add: &[Asset]) -> (Library, Vec<Option<usize>>) {
        enum From<'a> {
            Old(usize),
            New(&'a Asset),
        }
        let have: HashSet<&str> = self.assets.iter().map(|a| a.id.as_str()).collect();
        let mut order: Vec<From> = (0..self.assets.len()).filter(|&i| !remove.contains(&self.assets[i].id)).map(From::Old).collect();
        let mut seen = HashSet::new();
        order.extend(add.iter().filter(|a| !have.contains(a.id.as_str()) && seen.insert(a.id.as_str())).map(From::New));
        fn taken<'a>(lib: &'a Library, f: &From<'a>) -> &'a str {
            match f {
                From::Old(i) => &lib.assets[*i].taken,
                From::New(a) => &a.taken,
            }
        }
        // Stable, so equal timestamps keep their relative order.
        order.sort_by(|a, b| taken(self, a).cmp(taken(self, b)));
        let th = &self.thumbhashes;
        if cell_size(order.len()) != th.cell {
            let assets = order.iter().map(|f| match f { From::Old(i) => self.assets[*i].clone(), From::New(a) => (*a).clone() }).collect();
            let lib = Library::new(assets);
            let at: std::collections::HashMap<&str, usize> = lib.assets.iter().enumerate().map(|(i, a)| (a.id.as_str(), i)).collect();
            let map = self.assets.iter().map(|a| at.get(a.id.as_str()).copied()).collect();
            return (lib, map);
        }

        let (cell, cols) = (th.cell, th.cols);
        let rows = (order.len().max(1) as u32).div_ceil(cols);
        let mut levels: Vec<Vec<u8>> = (0..th.levels.len())
            .map(|l| vec![0u8; (((th.width >> l).max(1)) * ((rows * cell) >> l).max(1) * 4) as usize])
            .collect();
        let mut map = vec![None; self.assets.len()];
        let mut assets = Vec::with_capacity(order.len());
        for (j, f) in order.iter().enumerate() {
            let fresh = match f {
                From::Old(i) => {
                    map[*i] = Some(j);
                    assets.push(self.assets[*i].clone());
                    None
                }
                From::New(a) => {
                    assets.push((*a).clone());
                    Some(cell_mips(thumbhash_cell(a, cell), cell, th.levels.len()))
                }
            };
            for (l, dst) in levels.iter_mut().enumerate() {
                let (c, w) = ((cell >> l).max(1), (th.width >> l).max(1));
                let at = |k: u32| (((k / cols) * c * w + (k % cols) * c) * 4) as usize;
                let d = at(j as u32);
                for row in 0..c {
                    let o = (row * w * 4) as usize;
                    let len = (c * 4) as usize;
                    match (f, &fresh) {
                        (From::Old(i), _) => {
                            let s = at(*i as u32);
                            dst[d + o..d + o + len].copy_from_slice(&th.levels[l][s + o..s + o + len]);
                        }
                        (_, Some(m)) => {
                            let s = (row * c * 4) as usize;
                            dst[d + o..d + o + len].copy_from_slice(&m[l][s..s + len]);
                        }
                        _ => {}
                    }
                }
            }
        }
        let videos = assets.iter().filter(|a| a.video).count();
        let thumbhashes = ThumbhashAtlas { cell, cols, width: th.width, height: rows * cell, levels };
        (Library { assets, thumbhashes, videos }, map)
    }
}

/// Box-filtered mips of one cell, matching what the whole-atlas filter produces for it.
fn cell_mips(base: Vec<u8>, cell: u32, count: usize) -> Vec<Vec<u8>> {
    let mut out = vec![base];
    let mut c = cell;
    while out.len() < count {
        let prev = out.last().unwrap();
        let n = (c / 2).max(1);
        let mut next = vec![0u8; (n * n * 4) as usize];
        for y in 0..n {
            for x in 0..n {
                for k in 0..4 {
                    let at = |xx: u32, yy: u32| prev[((yy.min(c - 1) * c + xx.min(c - 1)) * 4 + k) as usize] as u32;
                    let sum = at(2 * x, 2 * y) + at(2 * x + 1, 2 * y) + at(2 * x, 2 * y + 1) + at(2 * x + 1, 2 * y + 1);
                    next[((y * n + x) * 4 + k) as usize] = (sum / 4) as u8;
                }
            }
        }
        out.push(next);
        c = n;
    }
    out
}

/// One asset's placeholder, `cell`×`cell` RGBA8 (dark grey when it has no thumbhash).
fn thumbhash_cell(a: &Asset, cell: u32) -> Vec<u8> {
    use base64::Engine;
    a.thumbhash
        .as_deref()
        .and_then(|h| base64::engine::general_purpose::STANDARD.decode(h).ok())
        .and_then(|bytes| thumbhash::thumb_hash_to_rgba(&bytes).ok())
        .and_then(|(w, h, rgba)| RgbaImage::from_raw(w as u32, h as u32, rgba))
        .map(|img| DynamicImage::ImageRgba8(img).resize_to_fill(cell, cell, FilterType::Triangle).to_rgba8().into_raw())
        .unwrap_or_else(|| [38u8, 38, 40, 255].repeat((cell * cell) as usize))
}

fn cell_size(n: usize) -> u32 {
    // 16px cells hold up to 262k assets in an 8192² texture; shrink beyond that.
    if n <= 262_144 { 16 } else if n <= 1_048_576 { 8 } else { 4 }
}

fn build_thumbhash_atlas(assets: &[Asset]) -> ThumbhashAtlas {
    let n = assets.len().max(1) as u32;
    let cell = cell_size(n as usize);
    let cols = ATLAS_WIDTH / cell;
    let rows = n.div_ceil(cols);
    let (width, height) = (ATLAS_WIDTH, rows * cell);
    let mut base = vec![0u8; (width * height * 4) as usize];

    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let chunk = assets.len().div_ceil(threads).max(1);
    let cells: Vec<Vec<(usize, Vec<u8>)>> = std::thread::scope(|s| {
        let handles: Vec<_> = assets
            .chunks(chunk)
            .enumerate()
            .map(|(ci, part)| {
                s.spawn(move || {
                    part.iter()
                        .enumerate()
                        .map(|(j, a)| (ci * chunk + j, thumbhash_cell(a, cell)))
                        .collect()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    for (i, px) in cells.into_iter().flatten() {
        let (cx, cy) = ((i as u32 % cols) * cell, (i as u32 / cols) * cell);
        for row in 0..cell {
            let dst = (((cy + row) * width + cx) * 4) as usize;
            let src = (row * cell * 4) as usize;
            base[dst..dst + (cell * 4) as usize].copy_from_slice(&px[src..src + (cell * 4) as usize]);
        }
    }

    // Cells are power-of-two aligned, so a plain 2×2 box filter never mixes neighbours.
    let mut levels = vec![base];
    let (mut w, mut h) = (width, height);
    for _ in 0..cell.trailing_zeros() {
        let prev = levels.last().unwrap();
        let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
        let mut next = vec![0u8; (nw * nh * 4) as usize];
        for y in 0..nh {
            for x in 0..nw {
                for c in 0..4 {
                    let at = |xx: u32, yy: u32| prev[((yy.min(h - 1) * w + xx.min(w - 1)) * 4 + c) as usize] as u32;
                    let sum = at(2 * x, 2 * y) + at(2 * x + 1, 2 * y) + at(2 * x, 2 * y + 1) + at(2 * x + 1, 2 * y + 1);
                    next[((y * nw + x) * 4 + c) as usize] = (sum / 4) as u8;
                }
            }
        }
        levels.push(next);
        (w, h) = (nw, nh);
    }

    ThumbhashAtlas { cell, cols, width, height, levels }
}

/// A synthetic library so the grid can be tried without a server.
pub fn demo_assets(n: usize) -> Vec<Asset> {
    use base64::Engine;
    let start = 1_420_070_400i64; // 2015-01-01
    let span = 11 * 365 * 86_400i64;
    (0..n)
        .map(|i| {
            let t = start + (span as f64 * (i as f64 / n as f64).powf(0.8)) as i64;
            let img = demo_image(i as u64, 32);
            let hash = thumbhash::rgba_to_thumb_hash(32, 32, img.as_raw());
            Asset {
                id: format!("demo-{i}"),
                taken: iso_date(t),
                ratio: 1.0,
                video: i % 23 == 0,
                thumbhash: Some(base64::engine::general_purpose::STANDARD.encode(hash)),
                live_video: None,
                duration_ms: (i % 23 == 0).then_some(3_000 + (i as u32 * 7_919) % 600_000),
                favorite: i % 17 == 0,
            }
        })
        .collect()
}

/// A little procedural "landscape": sky gradient, a sun and a hill, tinted by season.
pub fn demo_image(seed: u64, size: u32) -> RgbaImage {
    let mut r = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    let mut rnd = move || {
        r ^= r << 13;
        r ^= r >> 7;
        r ^= r << 17;
        (r % 10_000) as f32 / 10_000.0
    };
    let hue = rnd();
    let sky_top = hsv(hue, 0.55, 0.95);
    let sky_bot = hsv((hue + 0.08) % 1.0, 0.35, 1.0);
    let ground = hsv((hue + 0.35) % 1.0, 0.6, 0.45 + rnd() * 0.3);
    let (sx, sy, sr) = (rnd(), 0.2 + rnd() * 0.3, 0.06 + rnd() * 0.08);
    let (hill_a, hill_f, hill_y) = (0.05 + rnd() * 0.1, 2.0 + rnd() * 6.0, 0.55 + rnd() * 0.25);
    RgbaImage::from_fn(size, size, |x, y| {
        let (u, v) = (x as f32 / size as f32, y as f32 / size as f32);
        let horizon = hill_y + hill_a * (u * hill_f + hue * 9.0).sin();
        let c = if v > horizon {
            ground.map(|g| g * (1.1 - (v - horizon)))
        } else if ((u - sx).powi(2) + (v - sy).powi(2)).sqrt() < sr {
            [1.0, 0.96, 0.85]
        } else {
            let k = v / horizon;
            [0, 1, 2].map(|i| sky_top[i] * (1.0 - k) + sky_bot[i] * k)
        };
        image::Rgba([c[0], c[1], c[2], 1.0].map(|f| (f.clamp(0.0, 1.0) * 255.0) as u8))
    })
}

fn hsv(h: f32, s: f32, v: f32) -> [f32; 3] {
    let f = |n: f32| {
        let k = (n + h * 6.0) % 6.0;
        v - v * s * k.min(4.0 - k).clamp(0.0, 1.0)
    };
    [f(5.0), f(3.0), f(1.0)]
}

fn iso_date(unix: i64) -> String {
    // Civil-from-days (Howard Hinnant), good enough for labels.
    let days = unix.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let secs = unix.rem_euclid(86_400);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z", secs / 3600, secs / 60 % 60, secs % 60)
}
