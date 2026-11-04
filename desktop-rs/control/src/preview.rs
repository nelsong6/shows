use serde::{Deserialize, Serialize};

pub const WIDTH: usize = 160;
pub const HEIGHT: usize = 90;
pub const INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoPreview {
    pub rows: Vec<String>,
    pub aspect: f64,
    pub capture_ms: f64,
}

impl VideoPreview {
    pub fn validate(&self) -> Result<(), String> {
        if self.rows.len() != HEIGHT
            || self.rows.iter().any(|r| {
                r.len() != WIDTH || !r.bytes().all(|b| b == b' ' || b".:-=+*#%@".contains(&b))
            })
            || !self.aspect.is_finite()
            || !(0.1..=10.0).contains(&self.aspect)
            || !self.capture_ms.is_finite()
            || self.capture_ms < 0.0
        {
            return Err("Invalid video preview from target".into());
        }
        Ok(())
    }

    /// Convert a top-down RGBA sample without changing its orientation.
    pub fn from_rgba(pixels: &[u8], aspect: f64, capture_ms: f64) -> Result<Self, String> {
        if pixels.len() != WIDTH * HEIGHT * 4 {
            return Err("Invalid video sample size".into());
        }
        let ramp = b" .:-=+*#%@";
        let rows = pixels
            .as_chunks::<{ WIDTH * 4 }>().0.iter()
            .map(|row| {
                row.as_chunks::<4>().0.iter()
                    .map(|p| {
                        let light =
                            (54 * p[0] as usize + 183 * p[1] as usize + 19 * p[2] as usize) / 256;
                        ramp[light * (ramp.len() - 1) / 255] as char
                    })
                    .collect()
            })
            .collect();
        let preview = Self {
            rows,
            aspect,
            capture_ms,
        };
        preview.validate()?;
        Ok(preview)
    }

    pub fn fit(&self, columns: u16, lines: u16) -> Vec<String> {
        if columns == 0 || lines == 0 {
            return vec![];
        }
        // Terminal cells are approximately twice as tall as they are wide.
        let width = usize::from(columns)
            .min((f64::from(lines) * self.aspect * 2.0).round().max(1.0) as usize);
        let height =
            usize::from(lines).min((width as f64 / self.aspect / 2.0).round().max(1.0) as usize);
        (0..height)
            .map(|y| {
                let row = self.rows[y * HEIGHT / height].as_bytes();
                (0..width).map(|x| row[x * WIDTH / width] as char).collect()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_top_down_surface_orientation_and_terminal_aspect() {
        let mut pixels = vec![0; WIDTH * HEIGHT * 4];
        // Unequal corner luminances expose vertical and horizontal mirroring.
        for (x, y, light) in [
            (0, 0, 255),
            (WIDTH - 1, 0, 170),
            (0, HEIGHT - 1, 85),
        ] {
            let offset = (y * WIDTH + x) * 4;
            pixels[offset..offset + 4].fill(light);
        }
        let sample = VideoPreview::from_rgba(&pixels, 16.0 / 9.0, 1.0).unwrap();
        assert_eq!(sample.rows[0].as_bytes()[0], b'@');
        assert_eq!(sample.rows[0].as_bytes()[WIDTH - 1], b'*');
        assert_eq!(sample.rows[HEIGHT - 1].as_bytes()[0], b'-');
        assert_eq!(sample.rows[HEIGHT - 1].as_bytes()[WIDTH - 1], b' ');
        assert_eq!(sample.fit(WIDTH as u16, HEIGHT as u16)[0].as_bytes()[0], b'@');
        let fitted = sample.fit(80, 30);
        assert_eq!((fitted[0].len(), fitted.len()), (80, 23));
        assert!(sample.fit(0, 1).is_empty());
        assert_eq!(sample.fit(1, 1).len(), 1);
    }

    #[test]
    fn rejects_malformed_pixels_and_terminal_escape_sequences() {
        assert!(VideoPreview::from_rgba(&[], 1.0, 0.0).is_err());
        let mut sample = VideoPreview::from_rgba(&vec![0; WIDTH * HEIGHT * 4], 1.0, 0.0).unwrap();
        sample.rows[0] = "\x1b".repeat(WIDTH);
        assert!(sample.validate().is_err());
        sample.rows[0] = " ".repeat(WIDTH);
        sample.aspect = f64::NAN;
        assert!(sample.validate().is_err());
    }
}
