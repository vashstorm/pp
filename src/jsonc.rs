use anyhow::{Result, anyhow};
use jsonc_parser::{ParseOptions, parse_to_serde_value};

fn options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        allow_bare_decimal_point_numbers: false,
        allow_non_finite_numbers: false,
        allow_extended_string_escapes: false,
    }
}

// Parser/serde errors may contain credential values; never expose their text.
pub(crate) fn parse<T: serde::de::DeserializeOwned>(text: &str, kind: &str) -> Result<T> {
    parse_to_serde_value::<T>(text, &options()).map_err(|_| {
        anyhow!(
            "invalid JSONC {kind} config: check syntax, field types, required fields, and unknown fields"
        )
    })
}
