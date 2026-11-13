//! `rg` 结果的分组标题渲染。

pub(super) fn grouped_header(
    path: &str,
    tag: Option<&str>,
    common: usize,
    previous: &mut Vec<String>,
) -> String {
    let parts: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
    let dirs = &parts[..parts.len().saturating_sub(1)];
    let mut groups = Vec::new();
    if common > 0 {
        groups.push(dirs[..common].join("/"));
    }
    groups.extend(dirs[common..].iter().map(|part| (*part).to_owned()));
    let shared = groups
        .iter()
        .zip(previous.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut output = String::new();
    for (depth, directory) in groups.iter().enumerate().skip(shared) {
        output.push_str(&format!("{} {directory}/\n", "#".repeat(depth + 1)));
    }
    let name = parts.last().copied().unwrap_or(path);
    output.push_str(&format!(
        "{} {name}{}\n",
        "#".repeat(groups.len() + 1),
        tag.map(|t| format!("#{t}")).unwrap_or_default()
    ));
    *previous = groups;
    output
}
