//! 识别旧 CLI adapter 配置；LSP 不再启动这些命令。
pub(crate) fn is_adapter(name: &str) -> bool {
    matches!(name, "biome" | "swiftlint")
}
