pub mod decode;
pub mod encode;
pub mod frame;
mod hw;
pub mod pattern;
pub mod pipeline;
pub mod source;

pub fn init() -> anyhow::Result<()> {
    ffmpeg_next::init()?;
    ffmpeg_next::util::log::set_level(ffmpeg_next::util::log::Level::Error);
    Ok(())
}
