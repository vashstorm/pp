use super::{MAX_INPUT_BYTES, Read, ReadContext, ReadResult, ToolImage, selector::Target};
use ::image::{ImageFormat, ImageReader as Decoder, Limits};
use anyhow::{Result, ensure};
use base64::Engine;
use serde_json::json;
use std::io::Cursor;

pub(super) struct ImageReader;
pub(super) struct ImageInput {
    pub(super) target: Target,
    pub(super) bytes: Vec<u8>,
}

impl Read for ImageReader {
    type Input = ImageInput;

    fn read(
        &self,
        ImageInput { target, bytes }: Self::Input,
        _context: &mut ReadContext<'_, '_>,
    ) -> Result<ReadResult> {
        target.validate_image()?;
        let source = &target.source;
        let bytes = bytes.as_slice();
        ensure!(bytes.len() <= MAX_INPUT_BYTES, "image exceeds 20 MiB limit");
        let format = ::image::guess_format(bytes)
            .map_err(|_| anyhow::anyhow!("unsupported image format"))?;
        ensure!(
            matches!(
                format,
                ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif
            ),
            "unsupported image format"
        );
        let mut reader = Decoder::with_format(Cursor::new(bytes), format);
        let mut limits = Limits::default();
        limits.max_alloc = Some(256 * 1024 * 1024);
        reader.limits(limits.clone());
        let (width, height) = Decoder::with_format(Cursor::new(bytes), format)
            .into_dimensions()
            .map_err(|_| anyhow::anyhow!("invalid image dimensions"))?;
        ensure!(
            u64::from(width) * u64::from(height) <= 32_000_000,
            "image exceeds 32000000 pixel limit"
        );
        let decoded = reader
            .decode()
            .map_err(|_| anyhow::anyhow!("image decoding failed"))?;
        let mut converted = Cursor::new(Vec::new());
        let (data, mime) = if format == ImageFormat::Gif {
            decoded
                .write_to(&mut converted, ImageFormat::Png)
                .map_err(|_| anyhow::anyhow!("image conversion failed"))?;
            (converted.get_ref().as_slice(), "image/png")
        } else {
            (bytes, format.to_mime_type())
        };
        ensure!(
            data.len() <= MAX_INPUT_BYTES,
            "converted image exceeds 20 MiB limit"
        );
        let data_url = format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(data)
        );
        let content = format!(
            "Image: {source}\nFormat: {mime}\nDimensions: {width} x {height}\nBytes: {}{}",
            bytes.len(),
            if format == ImageFormat::Gif {
                "\nGIF: first frame"
            } else {
                ""
            }
        );
        Ok(ReadResult {
            value: json!({"ok":true,"kind":"image","content":content,"mime_type":mime,"width":width,"height":height,"bytes":bytes.len()}),
            images: vec![ToolImage { data_url }],
        })
    }
}

pub(super) fn is_image(bytes: &[u8]) -> bool {
    ::image::guess_format(bytes).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::read::{WebCache, run};
    use crate::{
        cancellation::CancellationToken,
        config::{EditMode, ToolLimits},
    };
    use std::path::Path;

    fn read_image(bytes: Vec<u8>) -> Result<ReadResult> {
        ImageReader.read(
            ImageInput {
                target: Target::parse("fixture", Path::new("."))?,
                bytes,
            },
            &mut ReadContext {
                cwd: Path::new("."),
                limits: ToolLimits::default(),
                mode: EditMode::Replace,
                cache: &mut WebCache::default(),
                fetch: &mut |_, _| unreachable!(),
                cancellation: &CancellationToken::new(),
            },
        )
    }

    #[test]
    fn read_images_validate_and_keep_payload_out_of_display() {
        use base64::Engine;
        for format in [
            image::ImageFormat::Png,
            image::ImageFormat::Jpeg,
            image::ImageFormat::WebP,
            image::ImageFormat::Gif,
        ] {
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(2, 3)
                .write_to(&mut bytes, format)
                .unwrap();
            let result = read_image(bytes.into_inner()).unwrap();
            assert_eq!(result.value["width"], 2);
            assert_eq!(result.value["height"], 3);
            assert_eq!(result.images.len(), 1);
            assert!(!result.value.to_string().contains("base64"));
            let data = result.images[0].data_url.split_once(',').unwrap().1;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap();
            assert!(image::load_from_memory(&decoded).is_ok());
            if format == image::ImageFormat::Gif {
                assert_eq!(result.value["mime_type"], "image/png");
            }
        }
        assert!(read_image(b"not an image".to_vec()).is_err());
        assert!(read_image(vec![0; MAX_INPUT_BYTES + 1]).is_err());
        assert!(read_image(b"GIF89a\xff\xff\xff\xff\x00\x00\x00".to_vec()).is_err());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        image::DynamicImage::new_rgb8(1, 1).save(&path).unwrap();
        assert!(
            run(
                "image.png:1",
                dir.path(),
                ToolLimits::default(),
                EditMode::Replace,
                &mut WebCache::default(),
                &mut |_, _| unreachable!(),
                &CancellationToken::new()
            )
            .is_err()
        );
    }
}
