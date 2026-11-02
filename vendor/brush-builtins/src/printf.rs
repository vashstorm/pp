use clap::Parser;
use std::{
    ffi::OsString,
    io::{self, Write},
    ops::ControlFlow,
};
use uucore::format;

use brush_core::{Error, ErrorKind, ExecutionResult, builtins, escape, expansion};

/// Format a string.
#[derive(Parser)]
#[clap(disable_help_flag = true, disable_version_flag = true)]
pub(crate) struct PrintfCommand {
    /// If specified, the output of the command is assigned to this variable.
    #[arg(short = 'v')]
    output_variable: Option<String>,

    /// Format string + arguments to the format string.
    #[arg(trailing_var_arg = true, required = true, allow_hyphen_values = true)]
    format_and_args: Vec<String>,
}

impl builtins::Command for PrintfCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        if let Some(variable_name) = &self.output_variable {
            // Format to a u8 vector.
            let mut result: Vec<u8> = vec![];
            format(self.format_and_args.as_slice(), &mut result)?;

            // Convert to a string.
            let result_str = String::from_utf8(result).map_err(|_| {
                brush_core::ErrorKind::PrintfInvalidUsage("invalid UTF-8 output".into())
            })?;

            // Assign to the selected variable.
            expansion::assign_to_named_parameter(
                context.shell,
                &context.params,
                variable_name,
                result_str,
            )
            .await?;
        } else {
            format(self.format_and_args.as_slice(), context.stdout())?;
            context.stdout().flush()?;
        }

        Ok(ExecutionResult::success())
    }
}

fn format(format_and_args: &[String], writer: impl Write) -> Result<(), brush_core::Error> {
    match format_and_args {
        // Special-case invocation of printf with %q-based format string from bash-completion.
        // It has hard-coded expectation of backslash-style escaping instead of quoting.
        [fmt, arg] if fmt == "%q" => format_special_case_for_percent_q(None, arg, writer),
        [fmt, arg] if fmt == "~%q" => format_special_case_for_percent_q(Some("~"), arg, writer),
        // Handle format string with arguments using uucore
        [fmt, args @ ..] => format_via_uucore(fmt, args.iter(), writer),
        // Handle case with no format string (we shouldn't be able to get here since clap will
        // fail parsing when the format string is missing)
        [] => Err(ErrorKind::PrintfInvalidUsage("missing operand".into()).into()),
    }
}

fn format_special_case_for_percent_q(
    prefix: Option<&str>,
    arg: &str,
    mut writer: impl Write,
) -> Result<(), brush_core::Error> {
    let mut result = escape::quote_if_needed(arg, escape::QuoteMode::BackslashEscape).to_string();

    if let Some(prefix) = prefix {
        result.insert_str(0, prefix);
    }

    write!(writer, "{result}")?;

    Ok(())
}

fn format_via_uucore(
    format_string: &str,
    args: impl Iterator<Item = impl Into<OsString>>,
    mut writer: impl Write,
) -> Result<(), brush_core::Error> {
    // Convert string arguments to FormatArgument::Unparsed
    let format_args: Vec<_> = args
        .map(|s| format::FormatArgument::Unparsed(s.into()))
        .collect();

    // Parse format string once.
    let format_items = parse_format_string(format_string)?;

    // Locate the field widths without going through uucore's private spec
    // representation. Only used to identify fields that the host formatter
    // cannot render itself (see `width_needs_manual_padding`).
    let spec_metadata = scan_format_specs(format_string.as_bytes()).filter(|specs| {
        specs.len()
            == format_items
                .iter()
                .filter(|item| matches!(item, format::FormatItem::Spec(_)))
                .count()
    });

    // Wrap the format arguments.
    let mut format_args_wrapper = format::FormatArguments::new(&format_args);
    let mut argument_cursor = ArgumentCursor::default();

    // Keep going until we've exhausted all format arguments. Also make sure to run at least once
    // even if there's no format arguments.
    while format_args.is_empty() || !format_args_wrapper.is_exhausted() {
        let mut spec_index = 0;
        // Process all format items, in order. We'll bail when we're told to stop.
        for item in &format_items {
            let metadata = match item {
                format::FormatItem::Spec(_) => {
                    let metadata = spec_metadata.as_ref().map(|specs| &specs[spec_index]);
                    spec_index += 1;
                    metadata
                }
                format::FormatItem::Char(_) => None,
            };
            let mut value_index = None;
            if let Some(metadata) = metadata {
                if let Some(NumberSpec::Asterisk(location)) = metadata.width {
                    argument_cursor.read(location);
                }
                if let PrecisionSpec::Asterisk(location) = metadata.precision
                    && resolves_precision(metadata.conversion)
                {
                    argument_cursor.read(location);
                }
                value_index = argument_cursor.read(metadata.value);
            }
            if let Some(metadata) = metadata
                && needs_manual_path(metadata)
            {
                write_wide_spec(
                    &mut writer,
                    &mut format_args_wrapper,
                    metadata,
                    &format_args,
                    value_index,
                )?;
                continue;
            }

            let control_flow = item
                .write(&mut writer, &mut format_args_wrapper)
                .map_err(map_format_error)?;

            if control_flow == ControlFlow::Break(()) {
                break;
            }
        }

        // Start next batch if not exhausted
        if !format_args_wrapper.is_exhausted() {
            format_args_wrapper.start_next_batch();
            argument_cursor.start_next_batch();
        }

        if format_args.is_empty() {
            break;
        }
    }

    Ok(())
}

fn map_format_error(error: format::FormatError) -> brush_core::Error {
    match error {
        // Propagate I/O errors directly so they can be handled appropriately
        format::FormatError::IoError(io_err) => Error::from(io_err),
        // Wrap other format errors
        other => Error::from(ErrorKind::PrintfInvalidUsage(std::format!(
            "printf formatting error: {other}"
        ))),
    }
}

/// Largest field width that the host's `format_args!` accepts. This toolchain
/// stores a dynamic width in 16 bits, so wider fields make the standard
/// formatter panic ("Formatting argument out of range"). uucore's own limit
/// (`MAX_FORMAT_WIDTH`, 1_000_000) is larger than that, which is why valid
/// widths such as `%70000s` reached the panic.
const MAX_HOST_WIDTH: usize = u16::MAX as usize;

/// Mirror of uucore's per-field formatting width limit. Fields that would need
/// more padding than this are rejected by uucore itself.
const MAX_FORMAT_WIDTH: usize = 1_000_000;

#[derive(Clone, Copy)]
enum ArgumentLocation {
    Next,
    Position(usize),
}

#[derive(Clone, Copy)]
enum NumberSpec {
    Fixed(usize),
    Asterisk(ArgumentLocation),
}

#[derive(Clone, Copy)]
enum PrecisionSpec {
    None,
    Fixed(usize),
    Asterisk(ArgumentLocation),
}

#[derive(Default)]
struct FormatFlags {
    minus: bool,
    zero: bool,
    raw: Vec<u8>,
}

struct SpecMetadata {
    value: ArgumentLocation,
    width: Option<NumberSpec>,
    precision: PrecisionSpec,
    flags: FormatFlags,
    length: Vec<u8>,
    conversion: u8,
}

#[derive(Default)]
struct ArgumentCursor {
    next: usize,
    highest: Option<usize>,
    offset: usize,
}

impl ArgumentCursor {
    /// Mirror of uucore's `FormatArguments::next_arg` that only tracks which
    /// absolute argument would be consumed. Used to clone the value argument
    /// for manual rendering without touching uucore's private location type.
    fn read(&mut self, location: ArgumentLocation) -> Option<usize> {
        match location {
            ArgumentLocation::Next => {
                let index = self.next;
                self.next += 1;
                Some(index)
            }
            ArgumentLocation::Position(position) => {
                let index = position.saturating_sub(1).saturating_add(self.offset);
                self.highest = Some(self.highest.map_or(index, |highest| highest.max(index)));
                Some(index)
            }
        }
    }

    fn start_next_batch(&mut self) {
        self.offset = self
            .next
            .max(self.highest.map_or(0, |highest| highest.saturating_add(1)));
        self.next = self.offset;
    }
}

/// Scan a format string and collect the information needed to format fields
/// wider than the host formatter supports. Escapes are skipped conservatively:
/// they can only consume ASCII digits, so they never hide a `%` that starts a
/// directive.
fn scan_format_specs(format: &[u8]) -> Option<Vec<SpecMetadata>> {
    let mut specs = Vec::new();
    let mut index = 0;
    while index < format.len() {
        match format[index] {
            b'\\' => {
                index += 1;
                if index < format.len() {
                    index += 1;
                }
            }
            b'%' => {
                if format.get(index + 1) == Some(&b'%') {
                    index += 2;
                    continue;
                }
                let (spec, next) = parse_spec_metadata(format, index + 1)?;
                specs.push(spec);
                index = next;
            }
            _ => index += 1,
        }
    }
    Some(specs)
}

fn parse_spec_metadata(format: &[u8], mut index: usize) -> Option<(SpecMetadata, usize)> {
    let value = eat_argument_position(format, &mut index)?;

    let mut flags = FormatFlags::default();
    while let Some(&flag) = format.get(index) {
        match flag {
            b'-' => flags.minus = true,
            b'0' => flags.zero = true,
            b'+' | b' ' | b'#' | b'\'' => {}
            _ => break,
        }
        flags.raw.push(flag);
        index += 1;
    }

    let width = eat_asterisk_or_number(format, &mut index);

    let precision = if format.get(index) == Some(&b'.') {
        index += 1;
        match eat_asterisk_or_number(format, &mut index) {
            Some(NumberSpec::Fixed(precision)) => PrecisionSpec::Fixed(precision),
            Some(NumberSpec::Asterisk(location)) => PrecisionSpec::Asterisk(location),
            None => PrecisionSpec::Fixed(0),
        }
    } else {
        PrecisionSpec::None
    };

    let length_start = index;
    loop {
        match format.get(index) {
            Some(b'h') => {
                index += 1;
                if format.get(index) == Some(&b'h') {
                    index += 1;
                }
            }
            Some(b'l') => {
                index += 1;
                if format.get(index) == Some(&b'l') {
                    index += 1;
                }
            }
            Some(b'j' | b'z' | b't' | b'L') => index += 1,
            _ => break,
        }
    }
    let length = format[length_start..index].to_vec();

    let conversion = *format.get(index)?;
    index += 1;
    let known = matches!(
        conversion,
        b'c' | b's'
            | b'b'
            | b'q'
            | b'd'
            | b'i'
            | b'u'
            | b'o'
            | b'x'
            | b'X'
            | b'f'
            | b'F'
            | b'e'
            | b'E'
            | b'g'
            | b'G'
            | b'a'
            | b'A'
    );
    if !known {
        return None;
    }

    Some((
        SpecMetadata {
            value,
            width,
            precision,
            flags,
            length,
            conversion,
        },
        index,
    ))
}

fn eat_argument_position(format: &[u8], index: &mut usize) -> Option<ArgumentLocation> {
    let start = *index;
    let mut end = start;
    while matches!(format.get(end), Some(b'0'..=b'9')) {
        end += 1;
    }
    if end > start && format.get(end) == Some(&b'$') {
        *index = end + 1;
        let position = std::str::from_utf8(&format[start..end])
            .ok()?
            .parse()
            .unwrap_or(usize::MAX);
        if position == 0 {
            return None;
        }
        Some(ArgumentLocation::Position(position))
    } else {
        *index = start;
        Some(ArgumentLocation::Next)
    }
}

fn eat_asterisk_or_number(format: &[u8], index: &mut usize) -> Option<NumberSpec> {
    if format.get(*index) == Some(&b'*') {
        *index += 1;
        Some(NumberSpec::Asterisk(eat_argument_position(format, index)?))
    } else {
        let start = *index;
        let mut end = start;
        while matches!(format.get(end), Some(b'0'..=b'9')) {
            end += 1;
        }
        if end == start {
            return None;
        }
        *index = end;
        Some(NumberSpec::Fixed(
            std::str::from_utf8(&format[start..end])
                .ok()?
                .parse()
                .unwrap_or(usize::MAX),
        ))
    }
}

fn width_needs_manual_padding(spec: &SpecMetadata) -> bool {
    match spec.width {
        Some(NumberSpec::Asterisk(_)) => true,
        Some(NumberSpec::Fixed(width)) => width > MAX_HOST_WIDTH,
        None => false,
    }
}

fn is_numeric(conversion: u8) -> bool {
    matches!(
        conversion,
        b'd' | b'i'
            | b'u'
            | b'o'
            | b'x'
            | b'X'
            | b'f'
            | b'F'
            | b'e'
            | b'E'
            | b'g'
            | b'G'
            | b'a'
            | b'A'
    )
}

fn is_float(conversion: u8) -> bool {
    matches!(
        conversion,
        b'f' | b'F' | b'e' | b'E' | b'g' | b'G' | b'a' | b'A'
    )
}

fn resolves_precision(conversion: u8) -> bool {
    conversion == b's' || is_numeric(conversion)
}

fn precision_needs_manual_path(spec: &SpecMetadata) -> bool {
    // Strings only truncate; uucore never passes their precision to the host
    // formatter, so any precision is safe there. Integers and floats can turn
    // precision into host padding, so large or dynamic values go manual.
    if !is_numeric(spec.conversion) {
        return false;
    }
    match spec.precision {
        PrecisionSpec::None => false,
        PrecisionSpec::Fixed(precision) => precision > MAX_HOST_WIDTH,
        PrecisionSpec::Asterisk(_) => true,
    }
}

fn needs_manual_path(spec: &SpecMetadata) -> bool {
    width_needs_manual_padding(spec) || precision_needs_manual_path(spec)
}

fn selector(location: ArgumentLocation) -> String {
    match location {
        ArgumentLocation::Next => String::new(),
        ArgumentLocation::Position(position) => format!("{position}$"),
    }
}

/// Format a single field whose width or precision the host formatter cannot
/// represent.
///
/// The value is formatted through uucore without a field width, then the
/// padding that uucore would have added is written directly. Precision is
/// clamped to the host limit and the remaining zeros/rounding extension is
/// applied by hand. This keeps conversion semantics in uucore while removing
/// the 16-bit host limitation for every width and precision.
fn write_wide_spec(
    writer: &mut impl Write,
    args: &mut format::FormatArguments<'_>,
    spec: &SpecMetadata,
    args_slice: &[format::FormatArgument],
    value_index: Option<usize>,
) -> Result<(), brush_core::Error> {
    // Resolve asterisk widths and precisions by having uucore itself consume
    // the arguments through a minimal `%d` directive. That keeps argument
    // positions, cursor movement and numeric error reporting identical to the
    // original directive. The rendered integer is the value uucore would use.
    let (width, negative_width) = match spec.width {
        None => (0, false),
        Some(NumberSpec::Fixed(width)) => (width, false),
        Some(NumberSpec::Asterisk(location)) => {
            let value = consume_integer(args, location)?;
            if value < 0 {
                (
                    value
                        .checked_neg()
                        .and_then(|value| usize::try_from(value).ok())
                        .unwrap_or(0),
                    true,
                )
            } else {
                (usize::try_from(value).unwrap_or(0), false)
            }
        }
    };
    let original_precision = match spec.precision {
        PrecisionSpec::None => None,
        PrecisionSpec::Fixed(precision) => Some(precision),
        PrecisionSpec::Asterisk(location) => {
            let value = consume_integer(args, location)?;
            Some(if value < 0 {
                0
            } else {
                usize::try_from(value).unwrap_or(0)
            })
        }
    };

    // Consume the value argument on the shared cursor exactly as the original
    // directive would; the content itself is rendered from a clone so float
    // variants can be rendered again with the full precision.
    consume_value(args, spec.value)?;
    let value = value_index
        .and_then(|index| args_slice.get(index))
        .cloned()
        .unwrap_or_else(|| default_argument(spec.conversion));

    let numeric = is_numeric(spec.conversion);
    let float = is_float(spec.conversion);

    let mut precision_extra = None;
    let content_precision = match original_precision {
        // String precision truncates; clamping would change the result.
        Some(precision) if !numeric => Some(precision),
        Some(precision) if precision > MAX_HOST_WIDTH => {
            if precision > MAX_FORMAT_WIDTH {
                return Err(map_format_error(format::FormatError::InvalidPrecision(
                    precision.to_string(),
                )));
            }
            precision_extra = Some(precision);
            Some(MAX_HOST_WIDTH)
        }
        other => other,
    };

    let render = |precision: Option<usize>| -> Result<Vec<u8>, brush_core::Error> {
        let format_string = rebuild_content_spec(spec, precision);
        let items = parse_format_string(&format_string)?;
        let arguments = [value.clone()];
        let mut arguments = format::FormatArguments::new(&arguments);
        let mut content = Vec::new();
        for item in &items {
            let _ = item
                .write(&mut content, &mut arguments)
                .map_err(map_format_error)?;
        }
        Ok(content)
    };

    let mut content = render(content_precision)?;

    if let Some(original) = precision_extra {
        let extra = original.saturating_sub(MAX_HOST_WIDTH);
        let sign = numeric
            && content
                .first()
                .is_some_and(|byte| matches!(byte, b'-' | b'+' | b' '));
        let sign_len = usize::from(sign);
        let non_finite = float
            && (content[sign_len..].eq_ignore_ascii_case(b"inf")
                || content[sign_len..].eq_ignore_ascii_case(b"nan"));
        if extra > 0 {
            if !float {
                // Integer precision zeros go after the sign and any `0x`
                // prefix and before the digits.
                let prefix = if content.len() >= sign_len + 2
                    && content[sign_len..sign_len + 2].eq_ignore_ascii_case(b"0x")
                {
                    2
                } else {
                    0
                };
                insert_zeros(&mut content, sign_len + prefix, extra);
            } else if !non_finite {
                match spec.conversion {
                    b'f' | b'F' => {
                        // bigdecimal refuses to create more than 1000 integer
                        // padding zeros, so integer-valued `%f` output has no
                        // decimal point even at large precision. Only extend
                        // real fractional output.
                        if content.contains(&b'.') {
                            content.extend(std::iter::repeat_n(b'0', extra));
                        }
                    }
                    b'e' | b'E' => {
                        if float_significand_is_zero(&content, sign_len, spec.conversion) {
                            let exponent =
                                content.iter().position(|byte| matches!(byte, b'e' | b'E'));
                            if let Some(exponent) = exponent {
                                insert_zeros(&mut content, exponent, extra);
                            }
                        } else {
                            content = render(Some(original))?;
                        }
                    }
                    b'g' | b'G' => {
                        if float_significand_is_zero(&content, sign_len, spec.conversion) {
                            if spec.flags.raw.iter().any(|byte| *byte == b'#') {
                                content.extend(std::iter::repeat_n(b'0', extra));
                            }
                        } else {
                            content = render(Some(original))?;
                        }
                    }
                    b'a' | b'A' => {
                        if float_significand_is_zero(&content, sign_len, spec.conversion) {
                            let exponent =
                                content.iter().rposition(|byte| matches!(byte, b'p' | b'P'));
                            if let Some(exponent) = exponent {
                                insert_zeros(&mut content, exponent, extra);
                            }
                        } else {
                            content = render(Some(original))?;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let left = spec.flags.minus || negative_width;
    let sign = if numeric
        && content
            .first()
            .is_some_and(|byte| matches!(byte, b'-' | b'+' | b' '))
    {
        1
    } else {
        0
    };
    // uucore pads non-finite floats with spaces even when the `0` flag is set.
    let non_finite = float
        && (content[sign..].eq_ignore_ascii_case(b"inf")
            || content[sign..].eq_ignore_ascii_case(b"nan"));

    // Reproduce uucore's internal width guard so oversized fields still fail
    // instead of allocating unbounded padding.
    let checked = if numeric {
        width.saturating_sub(sign.min(width))
    } else {
        width.saturating_sub(content.len().min(width))
    };
    if checked > MAX_FORMAT_WIDTH {
        return Err(Error::from(io::Error::new(
            io::ErrorKind::OutOfMemory,
            "formatting width too large",
        )));
    }

    let padding = width.saturating_sub(content.len());
    if padding == 0 {
        writer.write_all(&content)?;
    } else if left {
        writer.write_all(&content)?;
        write_padding(writer, b' ', padding)?;
    } else if numeric && spec.flags.zero && !non_finite && (float || original_precision.is_none()) {
        // Zero padding goes after the sign and, for hexadecimal output, after
        // the `0x` prefix, matching uucore's numeric alignment.
        let prefix =
            if content.len() >= sign + 2 && content[sign..sign + 2].eq_ignore_ascii_case(b"0x") {
                2
            } else {
                0
            };
        writer.write_all(&content[..sign + prefix])?;
        write_padding(writer, b'0', padding)?;
        writer.write_all(&content[sign + prefix..])?;
    } else {
        write_padding(writer, b' ', padding)?;
        writer.write_all(&content)?;
    }

    Ok(())
}

fn float_significand_is_zero(content: &[u8], sign: usize, conversion: u8) -> bool {
    let mut digits = &content[sign..];
    if matches!(conversion, b'a' | b'A') && (digits.starts_with(b"0x") || digits.starts_with(b"0X"))
    {
        digits = &digits[2..];
    }
    let mut saw_digit = false;
    for &byte in digits {
        if matches!(byte, b'e' | b'E' | b'p' | b'P') {
            break;
        }
        match byte {
            b'0' => saw_digit = true,
            b'1'..=b'9' | b'a'..=b'f' | b'A'..=b'F' => return false,
            b'.' | b'+' | b'-' => {}
            _ => return false,
        }
    }
    saw_digit
}

fn insert_zeros(content: &mut Vec<u8>, position: usize, count: usize) {
    if count > 0 && position <= content.len() {
        content.splice(position..position, std::iter::repeat_n(b'0', count));
    }
}

fn consume_integer(
    args: &mut format::FormatArguments<'_>,
    location: ArgumentLocation,
) -> Result<i64, brush_core::Error> {
    let items = parse_format_string(&format!("%{}d", selector(location)))?;
    let mut output = Vec::new();
    for item in &items {
        let _ = item.write(&mut output, args).map_err(map_format_error)?;
    }
    std::str::from_utf8(&output)
        .ok()
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| {
            Error::from(ErrorKind::PrintfInvalidUsage(
                "printf formatting error: invalid field width or precision".into(),
            ))
        })
}

/// Consume one value argument from the shared cursor without producing output.
/// `%c` consumes exactly one argument at the same location as any other
/// conversion and never parses it numerically.
fn consume_value(
    args: &mut format::FormatArguments<'_>,
    location: ArgumentLocation,
) -> Result<(), brush_core::Error> {
    let items = parse_format_string(&format!("%{}c", selector(location)))?;
    let mut sink = io::sink();
    for item in &items {
        let _ = item.write(&mut sink, args).map_err(map_format_error)?;
    }
    Ok(())
}

fn default_argument(conversion: u8) -> format::FormatArgument {
    match conversion {
        b'c' => format::FormatArgument::Char('\0'),
        b's' | b'b' | b'q' => format::FormatArgument::String(OsString::new()),
        b'd' | b'i' => format::FormatArgument::SignedInt(0),
        b'u' | b'o' | b'x' | b'X' => format::FormatArgument::UnsignedInt(0),
        _ => format::FormatArgument::Unparsed(OsString::from("0")),
    }
}

fn rebuild_content_spec(spec: &SpecMetadata, precision: Option<usize>) -> String {
    let mut rebuilt = String::with_capacity(spec.flags.raw.len() + spec.length.len() + 16);
    rebuilt.push('%');
    rebuilt.push_str(std::str::from_utf8(&spec.flags.raw).unwrap_or_default());
    if let Some(precision) = precision {
        rebuilt.push('.');
        rebuilt.push_str(&precision.to_string());
    }
    rebuilt.push_str(std::str::from_utf8(&spec.length).unwrap_or_default());
    rebuilt.push(char::from(spec.conversion));
    rebuilt
}

fn write_padding(writer: &mut impl Write, byte: u8, count: usize) -> io::Result<()> {
    let chunk = [byte; 4096];
    let mut remaining = count;
    while remaining > 0 {
        let take = remaining.min(chunk.len());
        writer.write_all(&chunk[..take])?;
        remaining -= take;
    }
    Ok(())
}

fn parse_format_string(
    format_string: &str,
) -> Result<Vec<format::FormatItem<format::EscapedChar>>, brush_core::Error> {
    let format_items: Result<Vec<_>, _> =
        format::parse_spec_and_escape(format_string.as_bytes()).collect();

    // Observe any errors we encountered along the way.
    let format_items = format_items
        .map_err(|e| ErrorKind::PrintfInvalidUsage(format!("printf parsing error: {e}")))?;

    Ok(format_items)
}

#[cfg(test)]
#[expect(clippy::panic_in_result_fn)]
mod tests {
    use super::*;
    use anyhow::Result;

    fn sprintf_via_uucore(
        format_string: &str,
        args: impl Iterator<Item = impl Into<OsString>>,
    ) -> Result<String> {
        let mut result = vec![];
        format_via_uucore(format_string, args, &mut result)?;

        Ok(String::from_utf8(result)?)
    }

    #[test]
    fn test_basic_sprintf() -> Result<()> {
        assert_eq!(sprintf_via_uucore("%s", std::iter::once(&"xyz"))?, "xyz");
        assert_eq!(sprintf_via_uucore(r"%d\n", std::iter::once(&"1"))?, "1\n");

        Ok(())
    }

    #[test]
    fn test_sprintf_without_args() -> Result<()> {
        let empty: [&str; 0] = [];

        assert_eq!(sprintf_via_uucore("xyz", empty.iter())?, "xyz");
        assert_eq!(sprintf_via_uucore("%s|", empty.iter())?, "|");

        Ok(())
    }

    #[test]
    fn test_sprintf_with_cycles() -> Result<()> {
        assert_eq!(sprintf_via_uucore("%s|", ["x", "y"].iter())?, "x|y|");

        Ok(())
    }

    #[test]
    fn test_large_width_and_precision_padding() -> Result<()> {
        let d = sprintf_via_uucore("%.70000d", std::iter::once(&"7"))?;
        assert_eq!(d.len(), 70_000);
        assert!(d.starts_with("0000"));
        assert!(d.ends_with('7'));
        let d65535 = sprintf_via_uucore("%.65535d", std::iter::once(&"7"))?;
        assert_eq!(d65535.len(), 65_535);
        let d65536 = sprintf_via_uucore("%.65536d", std::iter::once(&"7"))?;
        assert_eq!(d65536.len(), 65_536);
        let negative = sprintf_via_uucore("%.70000d", std::iter::once(&"-7"))?;
        assert_eq!(negative.len(), 70_001);
        assert!(negative.starts_with("-0"));
        assert!(negative.ends_with('7'));
        let hex = sprintf_via_uucore("%#.70000x", std::iter::once(&"255"))?;
        assert_eq!(hex.len(), 70_002);
        assert!(hex.starts_with("0x"));
        assert!(hex.ends_with("ff"));
        let octal = sprintf_via_uucore("%#.70000o", std::iter::once(&"8"))?;
        assert_eq!(octal.len(), 70_000);
        assert!(octal.ends_with("10"));
        let text = sprintf_via_uucore("%.70000s", std::iter::once(&"abc"))?;
        assert_eq!(text, "abc");
        let f = sprintf_via_uucore("%.70000f", std::iter::once(&"1.5"))?;
        assert_eq!(f.len(), 70_002);
        assert!(f.starts_with("1.5"));
        assert!(f.ends_with("0000"));
        let f65536 = sprintf_via_uucore("%.65536f", std::iter::once(&"1.5"))?;
        assert_eq!(f65536.len(), 65_538);
        let ez = sprintf_via_uucore("%.70000e", std::iter::once(&"0"))?;
        assert_eq!(ez.len(), 70_006);
        assert!(ez.starts_with("0."));
        assert!(ez.ends_with("e+00"));
        let en = sprintf_via_uucore("%.70000e", std::iter::once(&"1.5"))?;
        assert_eq!(en.len(), 70_006);
        assert!(en.starts_with("1.5"));
        assert!(en.ends_with("e+00"));
        let gz = sprintf_via_uucore("%#.70000g", std::iter::once(&"0"))?;
        assert_eq!(gz.len(), 70_001);
        assert!(gz.starts_with("0."));
        let gn = sprintf_via_uucore("%.70000g", std::iter::once(&"1.5"))?;
        assert_eq!(gn, "1.5");
        let star = sprintf_via_uucore("%.*d", ["70000", "7"].into_iter())?;
        assert_eq!(star.len(), 70_000);
        let starf = sprintf_via_uucore("%.*f", ["70000", "1.5"].into_iter())?;
        assert_eq!(starf.len(), 70_002);
        let stars = sprintf_via_uucore("%.*s", ["70000", "abc"].into_iter())?;
        assert_eq!(stars, "abc");
        let combo = sprintf_via_uucore("%080000.70000d", std::iter::once(&"7"))?;
        assert_eq!(combo.len(), 80_000);
        assert!(combo.starts_with(&" ".repeat(10_000)));
        assert!(combo.ends_with('7'));
        let small_f_zero = sprintf_via_uucore("%.10f", std::iter::once(&"0"))?;
        assert_eq!(small_f_zero, "0.0000000000");
        // bigdecimal's integer padding guard makes zero/integer `%f` output
        // plain "0"/"7" at large precision, matching the 65535-boundary result.
        let f_zero = sprintf_via_uucore("%.70000f", std::iter::once(&"0"))?;
        assert_eq!(f_zero, "0");
        let f_zero_65535 = sprintf_via_uucore("%.65535f", std::iter::once(&"0"))?;
        assert_eq!(f_zero_65535, "0");
        let f_zero_decimal = sprintf_via_uucore("%.70000f", std::iter::once(&"0.0"))?;
        assert_eq!(f_zero_decimal, "0");
        let f_integer = sprintf_via_uucore("%.70000f", std::iter::once(&"7"))?;
        assert_eq!(f_integer, "7");
        let star_e = sprintf_via_uucore("%.*e", ["70000", "0"].into_iter())?;
        assert_eq!(star_e.len(), 70_006);
        assert!(star_e.starts_with("0."));
        assert!(star_e.ends_with("e+00"));
        let star_g = sprintf_via_uucore("%#.*g", ["70000", "0"].into_iter())?;
        assert_eq!(star_g.len(), 70_001);
        let hex_float = sprintf_via_uucore("%.70000a", std::iter::once(&"0"))?;
        assert_eq!(hex_float.len(), 70_007);
        assert!(hex_float.starts_with("0x0."));
        assert!(hex_float.ends_with("p+0"));
        let hex_float_nonzero = sprintf_via_uucore("%.70000a", std::iter::once(&"1.5"))?;
        assert!(hex_float_nonzero.len() > 70_000);
        let non_finite = sprintf_via_uucore("%.70000f", std::iter::once(&"inf"))?;
        assert_eq!(non_finite, "inf");
        let width_zero = sprintf_via_uucore("%080000.70000f", std::iter::once(&"1.5"))?;
        assert_eq!(width_zero.len(), 80_000);
        assert!(width_zero.starts_with("000"));
        assert!(width_zero.contains("1.5"));
        let negative_zero = sprintf_via_uucore("%.70000e", std::iter::once(&"-0"))?;
        assert_eq!(negative_zero.len(), 70_007);
        assert!(negative_zero.starts_with("-0."));
        assert!(negative_zero.ends_with("e+00"));
        assert!(sprintf_via_uucore("%.1000001d", std::iter::once(&"7")).is_err());
        Ok(())
    }
}
