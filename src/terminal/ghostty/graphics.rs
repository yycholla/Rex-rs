//! Bounded owned Kitty pixels and placements; every native handle stays here.
use std::{collections::HashMap, sync::Arc, time::Instant};

use anyhow::{Context, Result, ensure};
use libghostty_vt::{
    Terminal,
    kitty::graphics::{Graphics, ImageFormat},
    render::{ImageIteration, ImageIterator, ImageLayer as NativeLayer, ImageProtocol, Snapshot},
};

use super::GhosttyTerminal;
use crate::core::{
    GridSize,
    frame::{ImageLayer, ImagePlacement, TerminalImage},
};

pub(super) const IMAGE_BYTES: usize = 16 * 1024 * 1024;
const IMAGE_DIMENSION: u32 = 4096;
const PLACEMENTS: usize = 4096;

type Pixels = HashMap<u64, Arc<TerminalImage>>;

pub(super) struct GraphicsState {
    iterator: ImageIterator<'static>,
    generation: Option<(u64, [u32; 2])>,
    pixels: Pixels,
    pub images: Arc<Vec<ImagePlacement>>,
    pub warning: Option<String>,
    epoch: Instant,
    next_tick: Option<u64>,
    pub needs_tick: bool,
}

impl GraphicsState {
    pub fn new() -> Result<Self> {
        Ok(Self {
            iterator: ImageIterator::new()?,
            generation: None,
            pixels: HashMap::new(),
            images: Arc::default(),
            warning: None,
            epoch: Instant::now(),
            next_tick: None,
            needs_tick: true,
        })
    }

    pub fn refresh(
        &mut self,
        terminal: &Terminal<'static, 'static>,
        snapshot: &Snapshot<'static, '_>,
        size: GridSize,
    ) {
        if let Err(error) = self.refresh_inner(terminal, snapshot, size) {
            self.images = Arc::default();
            self.pixels.clear();
            self.generation = None;
            self.warning = Some(format!("Terminal graphics omitted: {error}"));
        }
    }

    fn refresh_inner(
        &mut self,
        terminal: &Terminal<'static, 'static>,
        snapshot: &Snapshot<'static, '_>,
        size: GridSize,
    ) -> Result<()> {
        let cell_size = [
            terminal.width_px()? / u32::from(size.columns()),
            terminal.height_px()? / u32::from(size.rows()),
        ];
        let generation = (snapshot.image_placements_generation()?, cell_size);
        if self.generation == Some(generation) {
            return Ok(());
        }
        let graphics = terminal.kitty_graphics()?;
        let mut images = Vec::new();
        let mut pixels = HashMap::new();
        let mut remaining = IMAGE_BYTES;
        let mut warning = None;
        let mut iterator = self.iterator.update(snapshot)?;
        let mut count = 0;
        while let Some(placement) = iterator.next() {
            if count == PLACEMENTS {
                warning =
                    Some("Terminal graphics omitted: more than 4096 visible placements".into());
                break;
            }
            count += 1;
            match owned_placement(
                placement,
                &graphics,
                &self.pixels,
                &mut pixels,
                &mut remaining,
                cell_size,
            ) {
                Ok(placement) => images.push(placement),
                Err(error) => {
                    warning.get_or_insert_with(|| format!("Terminal graphics omitted: {error}"));
                }
            }
        }
        images.sort_by_key(|placement| (placement.layer, placement.z));
        // Generation changes may conservatively report an unchanged list.
        if self.images.as_ref() != &images {
            self.images = Arc::new(images);
        }
        self.pixels = pixels;
        self.warning = warning;
        self.generation = Some(generation);
        Ok(())
    }
}

fn owned_placement(
    native: &ImageIteration<'_, '_>,
    graphics: &Graphics<'_>,
    old: &Pixels,
    next: &mut Pixels,
    remaining: &mut usize,
    cell_size: [u32; 2],
) -> Result<ImagePlacement> {
    ensure!(
        native.protocol()? == ImageProtocol::Kitty,
        "unsupported image protocol"
    );
    ensure!(
        cell_size.iter().all(|size| *size > 0),
        "image cell size is unavailable"
    );
    let id = native.image_id()?;
    let generation = native.image_generation()?;
    let image = if let Some(image) = next.get(&generation) {
        Arc::clone(image)
    } else {
        let image = if let Some(image) = old.get(&generation) {
            Arc::clone(image)
        } else {
            let source = graphics.image(id).context("image pixels unavailable")?;
            ensure!(
                source.generation()? == generation,
                "image generation changed"
            );
            let width = source.width()?;
            let height = source.height()?;
            let length = rgba_length(width, height)?;
            ensure!(length <= *remaining, "visible images exceed 16 MiB");
            let rgba = rgba_pixels(
                source.data()?.context("image pixels pending")?,
                source.format()?,
                length,
            )?;
            Arc::new(TerminalImage {
                id,
                generation,
                width,
                height,
                rgba: Arc::new(rgba),
            })
        };
        ensure!(
            image.rgba.len() <= *remaining,
            "visible images exceed 16 MiB"
        );
        *remaining -= image.rgba.len();
        next.insert(generation, Arc::clone(&image));
        image
    };
    let source = [
        native.source_x()?,
        native.source_y()?,
        native.source_width()?,
        native.source_height()?,
    ];
    ensure!(
        source[2] > 0
            && source[3] > 0
            && source[0]
                .checked_add(source[2])
                .is_some_and(|end| end <= image.width)
            && source[1]
                .checked_add(source[3])
                .is_some_and(|end| end <= image.height),
        "image source rectangle is invalid"
    );
    let pixel_size = [native.pixel_width()?, native.pixel_height()?];
    ensure!(
        pixel_size.iter().all(|size| *size > 0),
        "image placement has zero size"
    );
    let layer = match native.layer()? {
        NativeLayer::BelowBackground => ImageLayer::BelowBackground,
        NativeLayer::BelowText => ImageLayer::BelowText,
        NativeLayer::AboveText => ImageLayer::AboveText,
    };
    Ok(ImagePlacement {
        image,
        layer,
        z: native.z()?,
        column: native.viewport_col()?,
        row: native.viewport_row()?,
        cell_offset: [native.cell_offset_x()?, native.cell_offset_y()?],
        pixel_size,
        source,
        cell_size,
    })
}

fn rgba_length(width: u32, height: u32) -> Result<usize> {
    ensure!(
        width > 0 && height > 0 && width <= IMAGE_DIMENSION && height <= IMAGE_DIMENSION,
        "image dimensions exceed 4096 pixels or are zero"
    );
    let length = usize::try_from(width)?
        .checked_mul(usize::try_from(height)?)
        .and_then(|pixels| pixels.checked_mul(4))
        .context("image size overflow")?;
    ensure!(length <= IMAGE_BYTES, "image exceeds 16 MiB");
    Ok(length)
}

fn rgba_pixels(data: &[u8], format: ImageFormat, length: usize) -> Result<Vec<u8>> {
    let channels = match format {
        ImageFormat::Rgb => 3,
        ImageFormat::Rgba => 4,
        ImageFormat::Gray => 1,
        ImageFormat::GrayAlpha => 2,
        _ => anyhow::bail!("unsupported decoded image format"),
    };
    ensure!(
        data.len() == length / 4 * channels,
        "decoded image byte count is invalid"
    );
    if channels == 4 {
        return Ok(data.to_vec());
    }
    let mut output = Vec::with_capacity(length);
    for pixel in data.chunks_exact(channels) {
        let rgba = match channels {
            3 => [pixel[0], pixel[1], pixel[2], 255],
            2 => [pixel[0], pixel[0], pixel[0], pixel[1]],
            _ => [pixel[0], pixel[0], pixel[0], 255],
        };
        output.extend_from_slice(&rgba);
    }
    Ok(output)
}

impl GhosttyTerminal {
    /// Advance native image animation only when a visible stream needs a tick.
    /// Returns whether frame extraction should run; static images need no timer.
    ///
    /// # Errors
    /// Reports failure advancing the native animation clock.
    pub fn graphics_tick(&mut self) -> Result<bool> {
        let now = u64::try_from(self.graphics.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        if !self.graphics.needs_tick && self.graphics.next_tick.is_none_or(|next| now < next) {
            return Ok(false);
        }
        self.graphics.needs_tick = false;
        self.graphics.next_tick = self
            .terminal
            .kitty_graphics_animation_tick(now)?
            .map(|next| next.max(now.saturating_add(16)));
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::ghostty::TerminalConfig;

    fn terminal() -> Result<GhosttyTerminal> {
        let size = GridSize::new(20, 6)?;
        let mut terminal = GhosttyTerminal::new(TerminalConfig {
            size,
            ..TerminalConfig::default()
        })?;
        terminal.resize(size, 8, 16)?;
        Ok(terminal)
    }

    #[test]
    fn image_changes_preserve_text_rows_and_share_unchanged_pixels() -> Result<()> {
        let mut terminal = terminal()?;
        terminal.feed(b"text\x1b_Ga=T,f=32,i=7,s=1,v=1,c=2,r=2,q=2;/wAA/w==\x1b\\");
        let red = terminal.snapshot_shared()?;
        assert_eq!(red.images.len(), 1);
        let image = &red.images[0];
        assert_eq!(image.image.rgba.as_ref(), &[255, 0, 0, 255]);
        assert_eq!(image.cell_size, [8, 16]);
        assert_eq!(image.pixel_size, [16, 32]);
        assert_eq!(image.source, [0, 0, 1, 1]);
        assert!(Arc::ptr_eq(&red, &terminal.snapshot_shared()?));
        terminal.feed(b"\x1b_Ga=t,f=32,i=7,s=1,v=1,q=2;AAD//w==\x1b\\");
        assert_eq!(terminal.snapshot_shared()?.images.len(), 0);
        // Transmitting a replacement retires old placements. Place the new
        // pixels explicitly without moving the cursor or changing text rows.
        terminal.feed(b"\x1b_Ga=p,i=7,c=2,r=2,C=1,q=2\x1b\\");
        let blue = terminal.snapshot_shared()?;
        assert!(!Arc::ptr_eq(&red, &blue));
        assert_eq!(blue.images[0].image.rgba.as_ref(), &[0, 0, 255, 255]);
        assert_ne!(blue.images[0].image.generation, image.image.generation);
        assert!(
            red.rows
                .iter()
                .zip(&blue.rows)
                .all(|(a, b)| Arc::ptr_eq(a, b))
        );
        terminal.feed(b"\x1b_Ga=d,d=I,i=7,q=2\x1b\\");
        assert!(terminal.snapshot_shared()?.images.is_empty());
        Ok(())
    }

    #[test]
    fn installed_rex_api7_raw_pixel_snapshot_restores_exact_image() -> Result<()> {
        let bytes = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/graphics-rex-api7.bin"
        ));
        let mut terminal = GhosttyTerminal::from_snapshot(bytes)?;
        terminal.resize(GridSize::new(80, 24)?, 8, 16)?;
        let frame = terminal.snapshot_shared()?;
        assert_eq!(frame.images.len(), 1);
        let placement = &frame.images[0];
        assert_eq!(placement.image.id, 4242);
        assert_eq!(placement.image.rgba.as_ref(), &[0, 0, 0, 255]);
        assert_eq!((placement.column, placement.row), (0, 0));
        assert_eq!(placement.pixel_size, [16, 32]);
        assert_eq!(placement.source, [0, 0, 1, 1]);
        assert!(
            frame
                .text_lines()
                .iter()
                .any(|line| line.contains("image-ready"))
        );
        terminal.feed(b"\x1b_Ga=q,f=24,i=99,s=1,v=1;AAAA\x1b\\");
        assert_eq!(terminal.take_pending_output(), Vec::<u8>::new());
        Ok(())
    }

    #[test]
    fn snapshot_restores_images_and_replica_never_answers_graphics_queries() -> Result<()> {
        let mut source = terminal()?;
        source.feed(b"\x1b_Ga=T,f=32,i=7,s=1,v=1,c=2,r=2,q=2;/wAA/w==\x1b\\");
        let expected = source.snapshot_shared()?;
        let bytes = source.snapshot_bytes()?;
        let mut restored = GhosttyTerminal::from_snapshot(&bytes)?;
        let actual = restored.snapshot_shared()?;
        assert_eq!(actual.images.len(), 1);
        assert_eq!(actual.images[0].image.rgba, expected.images[0].image.rgba);
        assert_eq!(actual.images[0].pixel_size, expected.images[0].pixel_size);
        restored.feed(b"\x1b_Ga=q,f=32,i=9,s=1,v=1;/wAA/w==\x1b\\");
        assert_eq!(restored.take_pending_output(), Vec::<u8>::new());
        Ok(())
    }

    #[test]
    fn oversized_graphics_are_omitted_without_losing_text() -> Result<()> {
        let mut terminal = terminal()?;
        let payload = "AAAA".repeat(4097);
        terminal.feed(
            format!("\x1b_Ga=T,f=24,i=9,s=4097,v=1,c=2,r=1,q=2;{payload}\x1b\\readable").as_bytes(),
        );
        let frame = terminal.snapshot_shared()?;
        assert!(frame.images.is_empty());
        assert!(
            frame
                .graphics_warning
                .as_deref()
                .is_some_and(|warning| warning.contains("4096"))
        );
        assert!(
            frame
                .text_lines()
                .iter()
                .any(|line| line.contains("readable"))
        );
        assert!(rgba_length(4096, 4096).is_err());
        assert!(rgba_length(0, 1).is_err());
        assert_eq!(
            rgba_pixels(&[12, 34], ImageFormat::GrayAlpha, 4)?,
            [12, 12, 12, 34]
        );
        assert!(rgba_pixels(&[12, 34], ImageFormat::Rgb, 4).is_err());
        Ok(())
    }

    #[test]
    fn image_crops_and_z_order_use_native_placements() -> Result<()> {
        let mut terminal = terminal()?;
        terminal.feed(
            b"\x1b_Ga=T,f=32,i=1,s=2,v=1,x=1,y=0,w=1,h=1,c=2,r=1,z=-1,q=2;/wAA/wD/AP8=\x1b\\",
        );
        terminal.feed(b"\x1b_Ga=T,f=32,i=2,s=1,v=1,c=1,r=1,z=3,q=2;/wAA/w==\x1b\\");
        let frame = terminal.snapshot_shared()?;
        assert_eq!(frame.images.len(), 2);
        assert_eq!(frame.images[0].source, [1, 0, 1, 1]);
        assert_eq!(frame.images[0].layer, ImageLayer::BelowText);
        assert_eq!(frame.images[1].layer, ImageLayer::AboveText);
        assert_eq!(frame.images[1].z, 3);
        terminal.feed(b"\x1b[?1049h");
        assert!(terminal.snapshot_shared()?.images.is_empty());
        terminal.feed(b"\x1b[?1049l");
        assert_eq!(terminal.snapshot_shared()?.images.len(), 2);
        Ok(())
    }

    #[test]
    fn native_animation_changes_pixels_without_rebuilding_text() -> Result<()> {
        let mut terminal = terminal()?;
        terminal.feed(
            b"\x1b_Ga=T,f=32,i=2,s=1,v=1,q=2;/wAA/w==\x1b\\\
            \x1b_Ga=f,f=32,i=2,s=1,v=1,z=40,q=2;AAD//w==\x1b\\\
            \x1b_Ga=a,i=2,r=1,z=30,q=2\x1b\\\
            \x1b_Ga=a,i=2,s=3,q=2\x1b\\",
        );
        assert!(terminal.graphics_tick()?);
        let red = terminal.snapshot_shared()?;
        assert_eq!(red.images[0].image.rgba.as_ref(), &[255, 0, 0, 255]);
        let elapsed = terminal.graphics.epoch.elapsed();
        terminal.graphics.epoch = Instant::now()
            .checked_sub(elapsed + std::time::Duration::from_millis(35))
            .context("test clock")?;
        assert!(terminal.graphics_tick()?);
        let blue = terminal.snapshot_shared()?;
        assert_eq!(blue.images[0].image.rgba.as_ref(), &[0, 0, 255, 255]);
        assert!(
            red.rows
                .iter()
                .zip(&blue.rows)
                .all(|(a, b)| Arc::ptr_eq(a, b))
        );
        Ok(())
    }

    #[test]
    fn builtin_png_decodes_without_a_runtime_callback() -> Result<()> {
        let mut terminal = terminal()?;
        terminal.feed(b"\x1b_Ga=T,f=100,i=11,c=1,r=1,q=2;iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==\x1b\\");
        let frame = terminal.snapshot_shared()?;
        assert_eq!(frame.images.len(), 1);
        assert_eq!(frame.images[0].image.rgba.as_ref(), &[255, 0, 0, 255]);
        Ok(())
    }
}
