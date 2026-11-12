pub mod discovery;
pub mod identity;
pub mod pairing;
pub mod tls;
pub mod transport;

pub fn config_dir(override_dir: Option<&std::path::Path>) -> std::path::PathBuf {
    override_dir
        .map(std::path::Path::to_path_buf)
        .or_else(|| dirs::config_dir().map(|d| d.join("mirrorwire")))
        .unwrap_or_else(|| std::path::PathBuf::from(".mirrorwire"))
}
