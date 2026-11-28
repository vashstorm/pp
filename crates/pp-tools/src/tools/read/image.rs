use super::Read;
use super::ReadContext;
use super::ReadResult;
use super::selector::Target;
use anyhow::Result;
pub use pp_workspace::image::decode_image;
pub(crate) use pp_workspace::image::is_image;
#[cfg(test)]
use pp_workspace::image::{
    MAX_IMAGE_INPUT_BYTES as MAX_INPUT_BYTES, MAX_IMAGE_SIDE, VISION_MAX_IMAGE_SIDE,
};

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
        let decoded = decode_image(&target.source, &bytes)?;
        Ok(ReadResult {
            value: decoded.value,
            images: decoded.images,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::read::{WebCache, run};
    use pp_config::config::EditMode;
    use pp_config::config::ToolLimits;
    use pp_core::cancellation::CancellationToken;
    use std::io::Cursor;
    use std::path::Path;

    fn read_image(bytes: Vec<u8>) -> Result<ReadResult> {
        ImageReader.read(
            ImageInput {
                target: Target::parse("fixture", Path::new("."))?,
                bytes,
            },
            &mut ReadContext {
                snapshots: None,
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
                &pp_config::skills::Skills::default().shared(),
                &mut WebCache::default(),
                &mut |_, _| unreachable!(),
                &CancellationToken::new()
            )
            .is_err()
        );
    }

    #[test]
    fn oversized_side_is_scaled_into_the_provider_limit() {
        use base64::Engine;
        for (width, height) in [
            (120, MAX_IMAGE_SIDE + 1000),
            (MAX_IMAGE_SIDE + 1000, 120),
            (VISION_MAX_IMAGE_SIDE + 600, 900),
        ] {
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(width, height)
                .write_to(&mut bytes, image::ImageFormat::Png)
                .unwrap();
            let result = read_image(bytes.into_inner()).unwrap();
            let sent_width = u32::try_from(result.value["width"].as_u64().unwrap()).unwrap();
            let sent_height = u32::try_from(result.value["height"].as_u64().unwrap()).unwrap();
            assert_eq!(sent_width.max(sent_height), VISION_MAX_IMAGE_SIDE);
            assert!(matches!(
                result.value["mime_type"].as_str(),
                Some("image/png" | "image/jpeg")
            ));
            let scale = f64::from(VISION_MAX_IMAGE_SIDE) / f64::from(width.max(height));
            assert!((f64::from(sent_width) - f64::from(width) * scale).abs() <= 1.0);
            assert!((f64::from(sent_height) - f64::from(height) * scale).abs() <= 1.0);
            let content = result.value["content"].as_str().unwrap();
            assert!(content.contains(&format!("Dimensions: {sent_width} x {sent_height}")));
            assert!(content.contains(&format!("Resized from: {width} x {height}")));
            let data = result.images[0].data_url.split_once(',').unwrap().1;
            let decoded = image::load_from_memory(
                &base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                (decoded.width(), decoded.height()),
                (sent_width, sent_height)
            );
        }
        // Below the vision limit the original bytes are kept without re-encoding.
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(3, VISION_MAX_IMAGE_SIDE)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let source = bytes.into_inner();
        let result = read_image(source.clone()).unwrap();
        assert!(
            !result.value["content"]
                .as_str()
                .unwrap()
                .contains("Resized from")
        );
        let data = result.images[0].data_url.split_once(',').unwrap().1;
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap(),
            source
        );
    }
}
