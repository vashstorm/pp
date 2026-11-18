use super::SearchSource;

const MAX_SNIPPET_CHARS: usize = 240;

pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    for (count, (idx, _)) in text.char_indices().enumerate() {
        if count == max_chars {
            let trimmed = text[..idx].trim_end();
            return format!("{trimmed}...");
        }
    }
    text.to_owned()
}

pub fn format_age(age_seconds: Option<i64>) -> Option<String> {
    let age = age_seconds?;
    if age < 0 {
        return None;
    }
    if age < 60 {
        return Some("just now".to_owned());
    }
    if age < 3_600 {
        return Some(format!("{}m ago", age / 60));
    }
    if age < 86_400 {
        return Some(format!("{}h ago", age / 3_600));
    }
    if age < 2_592_000 {
        return Some(format!("{}d ago", age / 86_400));
    }
    if age < 31_536_000 {
        return Some(format!("{}mo ago", age / 2_592_000));
    }
    Some(format!("{}y ago", age / 31_536_000))
}

/// 将统一搜索响应格式化为模型易读的紧凑文本。
pub fn format_for_llm(answer: Option<&str>, sources: &[SearchSource], notes: &[String]) -> String {
    let mut parts = Vec::new();
    for note in notes {
        parts.push(format!("Note: {note}"));
    }
    if let Some(answer) = answer.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(answer.to_owned());
        if !sources.is_empty() {
            parts.push("\n## Sources".to_owned());
            let noun = if sources.len() == 1 {
                "source"
            } else {
                "sources"
            };
            parts.push(format!("{} {noun}", sources.len()));
        }
    }
    for (i, src) in sources.iter().enumerate() {
        let age = format_age(src.age_seconds).or_else(|| src.published_date.clone());
        let age_part = age.map_or_else(String::new, |a| format!(" ({a})"));
        parts.push(format!(
            "[{}] {}{}\n    {}",
            i + 1,
            src.title,
            age_part,
            src.url
        ));
        if let Some(snippet) = &src.snippet {
            parts.push(format!(
                "    {}",
                truncate_chars(snippet, MAX_SNIPPET_CHARS)
            ));
        }
    }
    parts.join("\n")
}
