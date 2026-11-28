//! Recognizes legacy CLI adapter configuration; LSP no longer starts these commands.
pub(crate) fn is_adapter(name: &str) -> bool {
    matches!(name, "biome" | "swiftlint")
}
