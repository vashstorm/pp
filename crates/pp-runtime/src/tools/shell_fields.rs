//! 模型结果序列化前的 shell 展示字段剥离与恢复。
use serde_json::Value;

/// 只遍历工具结果对象；MCP 同名字段不经过此入口。
pub(super) fn remove_shell_display(value: &mut Value) -> Vec<(Vec<String>, Value)> {
    fn walk(value: &mut Value, path: &mut Vec<String>, saved: &mut Vec<(Vec<String>, Value)>) {
        match value {
            Value::Object(object) => {
                let display = object.get("shell").is_some_and(|shell| {
                    shell["version"].is_u64()
                        && shell["revision"].is_u64()
                        && shell["rows"].is_array()
                        && shell["running"].is_boolean()
                });
                if display && let Some(shell) = object.remove("shell") {
                    saved.push((path.clone(), shell));
                }
                for (key, child) in object {
                    path.push(key.clone());
                    walk(child, path, saved);
                    path.pop();
                }
            }
            Value::Array(values) => {
                for (index, child) in values.iter_mut().enumerate() {
                    path.push(index.to_string());
                    walk(child, path, saved);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut saved = Vec::new();
    walk(value, &mut Vec::new(), &mut saved);
    saved
}

pub(super) fn restore_shell_display(value: &mut Value, path: &[String], shell: Value) {
    let mut target = value;
    for key in path {
        target = if target.is_array() {
            &mut target[key.parse::<usize>().expect("数组路径由遍历器生成")]
        } else {
            &mut target[key.as_str()]
        };
    }
    target["shell"] = shell;
}
